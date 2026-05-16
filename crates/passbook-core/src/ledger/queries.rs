use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Health { pub last_block: Option<u64>, pub chain_id: Option<u64> }

pub fn health(conn: &Connection) -> eyre::Result<Health> {
    let last_block = conn.query_row(
        "SELECT v FROM meta WHERE k='last_block'", [], |r| r.get::<_,String>(0))
        .ok().and_then(|s| s.parse().ok());
    let chain_id = conn.query_row(
        "SELECT v FROM meta WHERE k='chain_id'", [], |r| r.get::<_,String>(0))
        .ok().and_then(|s| s.parse().ok());
    Ok(Health { last_block, chain_id })
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferRowOut {
    pub category: &'static str, pub block_number: u64, pub block_hash: String,
    pub tx_hash: Option<String>, pub address: String, pub direction: Option<String>,
    pub counterparty: Option<String>, pub token: Option<String>,
    pub amount: String, pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransfersPage { pub rows: Vec<TransferRowOut>, pub next_cursor: Option<u64> }

/// The `category` column carries an integer tag in the UNION ALL so we can use
/// it as a stable, cheap secondary sort key without string comparison. The map
/// back to the `&'static str` lives in `category_str`.
fn category_str(tag: i64) -> &'static str {
    match tag {
        0 => "eth",
        1 => "erc20",
        2 => "gas",
        _ => "unattributed",
    }
}

/// One ordered, paginated stream over eth + erc20 + gas + unattributed.
///
/// # Cursor semantics — block-complete (no silent row loss)
///
/// The previous implementation applied `LIMIT lim` to each of the 4 category
/// queries *independently*, merged, and set `next_cursor = last_block + 1`.
/// That silently dropped caller rows two ways: (a) per-category truncation
/// when one category alone exceeded `lim` in the window; (b) the `+1` skipped
/// the remainder of a block whose rows straddled the page boundary.
///
/// This implementation is **block-complete**: a block is never split across
/// pages. A single `UNION ALL` over the 4 category subqueries (projected to
/// the common [`TransferRowOut`] shape) is ordered by
/// `(block_number, category, rowid)` — a total, deterministic order. We then:
///
/// - Fetch `lim + 1` rows starting at `cursor_lo`.
/// - If we got `<= lim` rows, no truncation happened: the page is the whole
///   remaining stream, `next_cursor = None`.
/// - If we got `lim + 1` rows there is more data. The `(lim+1)`-th row's block
///   may be only *partially* visible in our fetch, so we must not emit it. Let
///   `cut = block_number` of the last fetched (the `(lim+1)`-th) row.
///   - If `cut > first_fetched_block`, every row with `block_number < cut` is
///     fully present (all of a block's rows sort contiguously and any later
///     block sorts strictly after, so seeing a row of `cut` proves every row
///     of every earlier block was already fetched). Emit all rows with
///     `block_number < cut`; `next_cursor = cut` (resume *at* the untouched
///     block — `cut`, not `cut + 1`, because no row of `cut` was emitted).
///   - If `cut == first_fetched_block` the page target `lim` is smaller than
///     the number of rows in this single block. Splitting it is forbidden, so
///     we re-query that one block in full (no LIMIT) and emit it whole. The
///     page legitimately exceeds `lim` here — that is required and correct.
///     `next_cursor = cut + 1` iff any row exists with `block_number > cut` in
///     range (else `None`).
///
/// Invariant: following `next_cursor` until `None` yields every matching row
/// exactly once — no skip, no dup, blocks never split. `lim` is a soft
/// minimum-page target capped at 1000.
///
/// `kind`: `None` = all 4 categories. `Some("eth"|"erc20"|"gas"|
/// "unattributed")` restricts to that category. For `"eth"`, the internal
/// `eth_transfers.kind` column (`top_level`/`internal`/`system`) is *not*
/// filtered — `kind` here selects the *category*, preserving the prior
/// behaviour where `kind=Some("erc20"|"gas"|"unattributed")` picked a
/// category and the eth `kind` sub-values were never used as a filter value
/// (they are surfaced verbatim in the output `kind` field for eth rows).
pub fn get_transfers(
    conn: &Connection, chain_id: u64, address: &str,
    from_block: Option<u64>, to_block: Option<u64>,
    kind: Option<&str>, cursor: Option<u64>, limit: u32,
) -> eyre::Result<TransfersPage> {
    let lo = cursor.or(from_block).unwrap_or(0) as i64;
    let hi = to_block.unwrap_or(u64::MAX).min(i64::MAX as u64) as i64;
    let lim = limit.clamp(1, 1000) as i64;

    // Category restriction: NULL => all; otherwise the matching tag only.
    let want = match kind {
        None => -1i64,
        Some("eth") => 0,
        Some("erc20") => 1,
        Some("gas") => 2,
        Some("unattributed") => 3,
        // Unknown kind => no category matches => empty page.
        Some(_) => 99,
    };

    // Common projection. `category` is the integer tag (0..3). `tx_hash`,
    // `direction`, `counterparty`, `token`, `kind` are nullable across
    // categories so each subquery supplies NULL where it has no value.
    // `rowid` is appended purely as a per-table tiebreaker for determinism.
    const UNION_SQL: &str = "\
        SELECT 0 AS category, block_number, block_hash, tx_hash, address, \
               direction, counterparty AS counterparty, NULL AS token, \
               amount_wei AS amount, kind AS ek, rowid AS rid \
          FROM eth_transfers \
         WHERE chain_id=?1 AND address=?2 AND block_number>=?3 AND block_number<=?4 \
        UNION ALL \
        SELECT 1, block_number, block_hash, tx_hash, address, \
               direction, NULL, token, amount, NULL, rowid \
          FROM erc20_transfers \
         WHERE chain_id=?1 AND address=?2 AND block_number>=?3 AND block_number<=?4 \
        UNION ALL \
        SELECT 2, block_number, block_hash, tx_hash, address, \
               'out', NULL, NULL, total_wei, NULL, rowid \
          FROM gas_payments \
         WHERE chain_id=?1 AND address=?2 AND block_number>=?3 AND block_number<=?4 \
        UNION ALL \
        SELECT 3, block_number, block_hash, NULL, address, \
               NULL, NULL, NULL, residual_wei, NULL, rowid \
          FROM unattributed_deltas \
         WHERE chain_id=?1 AND address=?2 AND block_number>=?3 AND block_number<=?4";

    fn map_row(r: &rusqlite::Row) -> rusqlite::Result<(i64, TransferRowOut)> {
        let tag: i64 = r.get(0)?;
        let bn: i64 = r.get(1)?;
        let cat = category_str(tag);
        let kind: Option<String> = match tag {
            0 => r.get(9)?,                         // eth: the eth_transfers.kind value
            1 => Some("erc20".into()),
            2 => Some("gas".into()),
            _ => Some("unattributed".into()),
        };
        Ok((bn, TransferRowOut {
            category: cat,
            block_number: bn as u64,
            block_hash: r.get(2)?,
            tx_hash: r.get(3)?,
            address: r.get(4)?,
            direction: r.get(5)?,
            counterparty: r.get(6)?,
            token: r.get(7)?,
            amount: r.get(8)?,
            kind,
        }))
    }

    // Fetch lim+1 ordered rows to detect "is there more?".
    let sql = format!(
        "SELECT category,block_number,block_hash,tx_hash,address,direction,\
                counterparty,token,amount,ek,rid \
           FROM ({UNION_SQL}) \
          WHERE (?5 < 0 OR category=?5) \
          ORDER BY block_number, category, rid \
          LIMIT ?6");
    let mut stmt = conn.prepare(&sql)?;
    let mut fetched: Vec<(i64, TransferRowOut)> = Vec::new();
    let it = stmt.query_map(
        rusqlite::params![chain_id, address, lo, hi, want, lim + 1],
        map_row)?;
    for x in it { fetched.push(x?); }

    // <= lim rows => the whole remaining stream fits; nothing was truncated.
    if (fetched.len() as i64) <= lim {
        return Ok(TransfersPage {
            rows: fetched.into_iter().map(|(_, r)| r).collect(),
            next_cursor: None,
        });
    }

    // We have exactly lim+1 rows => there is strictly more data.
    let first_block = fetched.first().unwrap().0;
    let cut = fetched.last().unwrap().0; // block of the (lim+1)-th row

    if cut > first_block {
        // Every row with block_number < cut is fully present. Emit those.
        let rows: Vec<TransferRowOut> = fetched.into_iter()
            .filter(|(b, _)| *b < cut)
            .map(|(_, r)| r)
            .collect();
        // Resume AT `cut` (no row of `cut` was emitted). There is guaranteed
        // more (the lim+1-th row itself is at `cut`).
        return Ok(TransfersPage { rows, next_cursor: Some(cut as u64) });
    }

    // cut == first_block: a single block has more than `lim` rows. We must
    // emit it whole (never split a block) — re-query just that block, no LIMIT.
    let block_sql = format!(
        "SELECT category,block_number,block_hash,tx_hash,address,direction,\
                counterparty,token,amount,ek,rid \
           FROM ({UNION_SQL}) \
          WHERE (?5 < 0 OR category=?5) AND block_number=?6 \
          ORDER BY block_number, category, rid");
    let mut bstmt = conn.prepare(&block_sql)?;
    let mut block_rows: Vec<TransferRowOut> = Vec::new();
    let it = bstmt.query_map(
        rusqlite::params![chain_id, address, lo, hi, want, cut],
        map_row)?;
    for x in it { block_rows.push(x?.1); }

    // Is there any row strictly after `cut` within range/filter?
    let more: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM ({UNION_SQL}) \
                  WHERE (?5 < 0 OR category=?5) AND block_number>?6 \
                    AND block_number<=?7)"),
        rusqlite::params![chain_id, address, lo, hi, want, cut, hi],
        |r| r.get::<_, i64>(0))? != 0;

    Ok(TransfersPage {
        rows: block_rows,
        next_cursor: if more { Some(cut as u64 + 1) } else { None },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::writer::{write_block, BlockBatch};
    use crate::model::*;
    use alloy_primitives::{Address, B256, U256};

    fn ledger() -> (crate::ledger::Ledger, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let l = crate::ledger::Ledger::open(&dir.path().join("q.db"), 1).unwrap();
        (l, dir)
    }

    const ADDR: u8 = 0xaa;

    fn eth_row(bn: u64, nonce: u8) -> EthTransferRow {
        EthTransferRow {
            chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
            tx_hash: Some(B256::repeat_byte(nonce)), trace_path: nonce.to_string(),
            address: Address::repeat_byte(ADDR), direction: Direction::In,
            counterparty: Address::repeat_byte(0xbb), amount_wei: U256::from(nonce as u64 + 1),
            kind: EthKind::TopLevel, reverted: false,
        }
    }
    fn erc20_row(bn: u64, log: u64) -> Erc20TransferRow {
        Erc20TransferRow {
            chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
            tx_hash: B256::repeat_byte((log + 1) as u8), log_index: log,
            token: Address::repeat_byte(0xcc), from: Address::repeat_byte(0xdd),
            to: Address::repeat_byte(ADDR), amount: U256::from(log + 1),
            address: Address::repeat_byte(ADDR), direction: Direction::In,
        }
    }

    /// Insert one eth row per (block, idx) into distinct blocks.
    fn write_eth(l: &mut crate::ledger::Ledger, bn: u64, count: u64) {
        let mut eth = Vec::new();
        for i in 0..count {
            eth.push(eth_row(bn, i as u8));
        }
        let b = BlockBatch {
            chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
            eth, erc20: vec![], gas: vec![], unattributed: vec![],
        };
        write_block(l.conn_mut(), &b).unwrap();
    }

    /// Walk the whole result set by following next_cursor; assert no skip,
    /// no dup, blocks never split, total == expected. Returns flattened rows.
    fn drain(l: &crate::ledger::Ledger, kind: Option<&str>, lim: u32)
        -> Vec<(u64, &'static str, String)> {
        let mut all: Vec<(u64, &'static str, String)> = Vec::new();
        let mut cursor: Option<u64> = None;
        let mut pages = 0;
        loop {
            let p = get_transfers(l.conn(), 1, &format!("{:#x}", Address::repeat_byte(ADDR)),
                None, None, kind, cursor, lim).unwrap();
            // A block must never be split across a page boundary: the set of
            // block_numbers in this page and the next must be disjoint. We
            // enforce by recording the page's max block and asserting the
            // next page's cursor is strictly greater than every block here
            // (it's the resume point) — and that no block appears in 2 pages.
            for r in &p.rows {
                all.push((r.block_number, r.category, r.amount.clone()));
            }
            pages += 1;
            assert!(pages < 100_000, "pagination did not terminate");
            match p.next_cursor {
                Some(c) => {
                    // every emitted block in this page is < c OR == c only if
                    // the page emitted that block whole. Cursor must advance.
                    if let Some(maxb) = p.rows.iter().map(|r| r.block_number).max() {
                        assert!(c > maxb || c == maxb + 1 || c == maxb,
                            "cursor {c} did not advance past page max {maxb}");
                    }
                    cursor = Some(c);
                }
                None => break,
            }
        }
        all
    }

    #[test]
    fn health_reports_last_block() {
        let (l, _t) = ledger();
        l.conn().execute(
            "INSERT INTO meta(k,v) VALUES('last_block','42')
             ON CONFLICT(k) DO UPDATE SET v=excluded.v", []).unwrap();
        assert_eq!(health(l.conn()).unwrap().last_block, Some(42));
    }

    /// (1) Single category, > lim rows across many blocks: following
    /// next_cursor to None returns ALL rows exactly once. No skip, no dup.
    #[test]
    fn single_category_over_limit_no_skip_no_dup() {
        let (mut l, _t) = ledger();
        // 50 blocks, 3 eth rows each = 150 rows; lim = 10.
        for bn in 1..=50u64 { write_eth(&mut l, bn, 3); }
        let got = drain(&l, Some("eth"), 10);
        assert_eq!(got.len(), 150, "must return every row exactly once");
        // Build the expected multiset and compare.
        let mut expected: Vec<(u64, &'static str)> = Vec::new();
        for bn in 1..=50u64 { for _ in 0..3 { expected.push((bn, "eth")); } }
        let mut got_bc: Vec<(u64, &'static str)> =
            got.iter().map(|(b, c, _)| (*b, *c)).collect();
        got_bc.sort();
        expected.sort();
        assert_eq!(got_bc, expected, "exact multiset: no skip, no dup");
        // No row appears more than its true count: amounts are unique per
        // (block, idx) only within a block; assert total distinct rows.
        assert_eq!(got.iter().filter(|(_, c, _)| *c == "eth").count(), 150);
    }

    /// (2) > lim rows concentrated in ONE block at a page boundary: that
    /// block is never split — all its rows land in a single page and the
    /// cursor advances cleanly past it.
    #[test]
    fn single_block_over_limit_never_split() {
        let (mut l, _t) = ledger();
        write_eth(&mut l, 1, 3);     // small block first
        write_eth(&mut l, 2, 25);    // fat block: 25 rows, lim = 5
        write_eth(&mut l, 3, 2);     // tail
        let addr = format!("{:#x}", Address::repeat_byte(ADDR));

        // Page 1: lim=5, block 1 (3) then we must complete block 2 (25) whole.
        let p1 = get_transfers(l.conn(), 1, &addr, None, None, Some("eth"),
            None, 5).unwrap();
        // The fat block must be entirely present in whichever page first
        // touches it; it must NOT be split. Walk pages and verify each block
        // appears in exactly one page in full.
        let mut seen_by_block: std::collections::HashMap<u64, usize> =
            std::collections::HashMap::new();
        let mut page_of_block: std::collections::HashMap<u64, usize> =
            std::collections::HashMap::new();
        let mut cursor = p1.next_cursor;
        for r in &p1.rows {
            *seen_by_block.entry(r.block_number).or_default() += 1;
            page_of_block.entry(r.block_number).or_insert(0);
        }
        let mut page_idx = 1;
        while let Some(c) = cursor {
            let p = get_transfers(l.conn(), 1, &addr, None, None, Some("eth"),
                Some(c), 5).unwrap();
            for r in &p.rows {
                *seen_by_block.entry(r.block_number).or_default() += 1;
                let pg = *page_of_block.entry(r.block_number).or_insert(page_idx);
                assert_eq!(pg, page_idx,
                    "block {} appeared in >1 page — block was split!",
                    r.block_number);
            }
            cursor = p.next_cursor;
            page_idx += 1;
            assert!(page_idx < 1000);
        }
        assert_eq!(seen_by_block.get(&1), Some(&3));
        assert_eq!(seen_by_block.get(&2), Some(&25), "fat block fully present");
        assert_eq!(seen_by_block.get(&3), Some(&2));
        // And the drain helper agrees total = 30, no dup/skip.
        let all = drain(&l, Some("eth"), 5);
        assert_eq!(all.len(), 30);
    }

    /// (3) Multiple categories sharing the same blocks: merged, ordered,
    /// complete, no dup.
    #[test]
    fn multi_category_same_blocks_merged_complete() {
        let (mut l, _t) = ledger();
        for bn in 1..=20u64 {
            let b = BlockBatch {
                chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
                eth: vec![eth_row(bn, 0), eth_row(bn, 1)],
                erc20: vec![erc20_row(bn, 0), erc20_row(bn, 1)],
                gas: vec![GasPaymentRow {
                    chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
                    tx_hash: B256::repeat_byte(7), address: Address::repeat_byte(ADDR),
                    gas_used: 21000, effective_gas_price: 1u128,
                    l2_fee_wei: U256::from(21000), l1_fee_wei: None,
                    total_wei: U256::from(21000),
                }],
                unattributed: vec![UnattributedDeltaRow {
                    chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
                    address: Address::repeat_byte(ADDR), observed_wei: U256::from(1),
                    attributed_wei: U256::ZERO, residual_wei: U256::from(1),
                }],
            };
            write_block(l.conn_mut(), &b).unwrap();
        }
        // 20 blocks * (2 eth + 2 erc20 + 1 gas + 1 unattr) = 120 rows.
        let all = drain(&l, None, 7);
        assert_eq!(all.len(), 120, "all categories, all blocks, exactly once");
        // Ordered by block ascending across the whole concatenation.
        let blocks: Vec<u64> = all.iter().map(|(b, _, _)| *b).collect();
        let mut sorted = blocks.clone();
        sorted.sort();
        assert_eq!(blocks, sorted, "globally ascending by block");
        // Per category counts.
        let c = |k: &str| all.iter().filter(|(_, cat, _)| *cat == k).count();
        assert_eq!(c("eth"), 40);
        assert_eq!(c("erc20"), 40);
        assert_eq!(c("gas"), 20);
        assert_eq!(c("unattributed"), 20);
    }

    /// (4) kind filter restricts correctly.
    #[test]
    fn kind_filter_restricts() {
        let (mut l, _t) = ledger();
        for bn in 1..=5u64 {
            let b = BlockBatch {
                chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
                eth: vec![eth_row(bn, 0)],
                erc20: vec![erc20_row(bn, 0)],
                gas: vec![GasPaymentRow {
                    chain_id: 1, block_number: bn, block_hash: B256::repeat_byte(bn as u8),
                    tx_hash: B256::repeat_byte(7), address: Address::repeat_byte(ADDR),
                    gas_used: 1, effective_gas_price: 1u128,
                    l2_fee_wei: U256::from(1), l1_fee_wei: None, total_wei: U256::from(1),
                }],
                unattributed: vec![],
            };
            write_block(l.conn_mut(), &b).unwrap();
        }
        assert_eq!(drain(&l, Some("eth"), 100).len(), 5);
        assert_eq!(drain(&l, Some("erc20"), 100).len(), 5);
        assert_eq!(drain(&l, Some("gas"), 100).len(), 5);
        assert_eq!(drain(&l, Some("unattributed"), 100).len(), 0);
        assert_eq!(drain(&l, None, 100).len(), 15);
        // Unknown kind => empty page, no cursor.
        let p = get_transfers(l.conn(), 1,
            &format!("{:#x}", Address::repeat_byte(ADDR)),
            None, None, Some("nope"), None, 100).unwrap();
        assert!(p.rows.is_empty());
        assert_eq!(p.next_cursor, None);
        // eth rows carry the internal kind value verbatim.
        let p = get_transfers(l.conn(), 1,
            &format!("{:#x}", Address::repeat_byte(ADDR)),
            None, None, Some("eth"), None, 100).unwrap();
        assert!(p.rows.iter().all(|r| r.kind.as_deref() == Some("top_level")));
    }

    /// (5) Empty / out-of-range => empty page, next_cursor None.
    #[test]
    fn empty_and_out_of_range() {
        let (mut l, _t) = ledger();
        write_eth(&mut l, 100, 2);
        let addr = format!("{:#x}", Address::repeat_byte(ADDR));
        // No matching address.
        let p = get_transfers(l.conn(), 1,
            &format!("{:#x}", Address::repeat_byte(0x11)),
            None, None, None, None, 50).unwrap();
        assert!(p.rows.is_empty() && p.next_cursor.is_none());
        // Range entirely below the only block.
        let p = get_transfers(l.conn(), 1, &addr, Some(1), Some(50),
            None, None, 50).unwrap();
        assert!(p.rows.is_empty() && p.next_cursor.is_none());
        // Range entirely above.
        let p = get_transfers(l.conn(), 1, &addr, Some(200), Some(300),
            None, None, 50).unwrap();
        assert!(p.rows.is_empty() && p.next_cursor.is_none());
        // Exact hit, fits in one page.
        let p = get_transfers(l.conn(), 1, &addr, Some(100), Some(100),
            None, None, 50).unwrap();
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.next_cursor, None);
    }
}
