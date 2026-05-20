use crate::model::*;
use rusqlite::Connection;

#[derive(Debug)]
pub struct BlockBatch {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: alloy_primitives::B256,
    pub eth: Vec<EthTransferRow>,
    pub erc20: Vec<Erc20TransferRow>,
    pub gas: Vec<GasPaymentRow>,
    pub unattributed: Vec<UnattributedDeltaRow>,
}

fn h(b: &alloy_primitives::B256) -> String {
    format!("{b:#x}")
}
fn a(x: &alloy_primitives::Address) -> String {
    format!("{x:#x}")
}
fn u(x: &alloy_primitives::U256) -> String {
    x.to_string()
}

/// One DB transaction per block. INSERT OR REPLACE on the natural PKs makes
/// replay (after a crash between commit and FinishedHeight) a no-op.
pub fn write_block(conn: &mut Connection, b: &BlockBatch) -> eyre::Result<()> {
    let tx = conn.transaction()?;
    for r in &b.eth {
        tx.execute(
            "INSERT OR REPLACE INTO eth_transfers VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                r.chain_id,
                r.block_number,
                h(&r.block_hash),
                r.tx_hash.as_ref().map(h),
                r.trace_path,
                a(&r.address),
                r.direction.as_str(),
                a(&r.counterparty),
                u(&r.amount_wei),
                r.kind.as_str(),
                r.reverted as i64
            ],
        )?;
    }
    for r in &b.erc20 {
        tx.execute(
            "INSERT OR REPLACE INTO erc20_transfers VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                r.chain_id,
                r.block_number,
                h(&r.block_hash),
                h(&r.tx_hash),
                r.log_index,
                a(&r.token),
                a(&r.from),
                a(&r.to),
                u(&r.amount),
                a(&r.address),
                r.direction.as_str()
            ],
        )?;
    }
    for r in &b.gas {
        tx.execute(
            "INSERT OR REPLACE INTO gas_payments VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                r.chain_id,
                r.block_number,
                h(&r.block_hash),
                h(&r.tx_hash),
                a(&r.address),
                r.gas_used,
                r.effective_gas_price.to_string(),
                u(&r.l2_fee_wei),
                r.l1_fee_wei.as_ref().map(u),
                u(&r.total_wei)
            ],
        )?;
    }
    for r in &b.unattributed {
        tx.execute(
            "INSERT OR REPLACE INTO unattributed_deltas VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                r.chain_id,
                r.block_number,
                h(&r.block_hash),
                a(&r.address),
                u(&r.observed_wei),
                u(&r.attributed_wei),
                u(&r.residual_wei),
                r.cause.as_str()
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('last_block',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [b.block_number.to_string()],
    )?;
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('last_block_hash',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [format!("{:#x}", b.block_hash)],
    )?;
    tx.commit()?;
    Ok(())
}

/// Single-row write for an unexplained-residual block. The ExEx loop calls
/// this when `process_one_committed_block` returns `UnexplainedResidual`
/// (it then stalls and retries — this row records the stall for the health
/// query). INSERT OR REPLACE on the natural PK keeps retries idempotent.
pub fn write_unattributed(conn: &mut Connection, r: &UnattributedDeltaRow) -> eyre::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO unattributed_deltas VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            r.chain_id,
            r.block_number,
            h(&r.block_hash),
            a(&r.address),
            u(&r.observed_wei),
            u(&r.attributed_wei),
            u(&r.residual_wei),
            r.cause.as_str()
        ],
    )?;
    Ok(())
}

/// Gap-on-restart marker write (issue #14). One DB transaction per
/// gap block: insert one `unattributed_deltas` row per watched address
/// with `cause = block_not_delivered` and all-zero amounts, then
/// advance `meta.last_block` / `meta.last_block_hash` to
/// `(block_number, block_hash)`. Atomic per block; crash-safe via
/// `INSERT OR REPLACE` idempotency. Mirrors `write_block`'s
/// per-block-TX shape so a partial gap-fill resumes cleanly from
/// the durable high-water.
///
/// An empty `watched` set is valid: zero marker rows are written, but
/// `meta` still advances so future gap detection stays correct.
pub fn write_gap_block_marker(
    conn: &mut Connection,
    chain_id: u64,
    block_number: u64,
    block_hash: alloy_primitives::B256,
    watched: &std::collections::HashSet<alloy_primitives::Address>,
) -> eyre::Result<()> {
    let tx = conn.transaction()?;
    for addr in watched {
        tx.execute(
            "INSERT OR REPLACE INTO unattributed_deltas VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                chain_id,
                block_number,
                h(&block_hash),
                a(addr),
                "0",
                "0",
                "0",
                crate::model::UnattributedDeltaCause::BlockNotDelivered.as_str()
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('last_block',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [block_number.to_string()],
    )?;
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('last_block_hash',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [format!("{:#x}", block_hash)],
    )?;
    tx.commit()?;
    Ok(())
}

/// Reorg handling: drop every row for the reverted block hashes.
pub fn delete_blocks(
    conn: &mut Connection,
    chain_id: u64,
    hashes: &[alloy_primitives::B256],
) -> eyre::Result<()> {
    let tx = conn.transaction()?;
    for bh in hashes {
        let hs = h(bh);
        for table in [
            "eth_transfers",
            "erc20_transfers",
            "gas_payments",
            "unattributed_deltas",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE chain_id=?1 AND block_hash=?2"),
                rusqlite::params![chain_id, hs],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};

    fn ledger() -> (crate::ledger::Ledger, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pb.db");
        let l = crate::ledger::Ledger::open(&p, 1).unwrap();
        (l, dir)
    }

    #[test]
    fn write_block_is_idempotent() {
        let (mut l, _tmp) = ledger();
        let bh = B256::repeat_byte(7);
        let batch = BlockBatch {
            chain_id: 1,
            block_number: 100,
            block_hash: bh,
            eth: vec![EthTransferRow {
                chain_id: 1,
                block_number: 100,
                block_hash: bh,
                tx_hash: Some(B256::repeat_byte(1)),
                trace_path: "0".into(),
                address: Address::repeat_byte(0xaa),
                direction: Direction::In,
                counterparty: Address::repeat_byte(0xbb),
                amount_wei: U256::from(5),
                kind: EthKind::TopLevel,
                reverted: false,
            }],
            erc20: vec![],
            gas: vec![],
            unattributed: vec![],
        };
        write_block(l.conn_mut(), &batch).unwrap();
        write_block(l.conn_mut(), &batch).unwrap(); // replay -> no dup
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM eth_transfers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let last: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(last, "100");
    }

    /// Issue #4 (C3): a single ERC20 `Transfer` log whose `from` AND `to`
    /// are both watched yields two `Erc20TransferRow`s with identical
    /// `(chain_id, block_hash, tx_hash, log_index)` but different
    /// `address`/`direction`. Before the PK fix `INSERT OR REPLACE` made
    /// the second clobber the first — one directional row was silently
    /// destroyed. With `address` in the PK both rows must persist.
    #[test]
    fn watched_to_watched_erc20_keeps_both_rows() {
        let (mut l, _tmp) = ledger();
        let bh = B256::repeat_byte(0x21);
        let tx = B256::repeat_byte(0x22);
        let from = Address::repeat_byte(0xa1);
        let to = Address::repeat_byte(0xb2);
        let token = Address::repeat_byte(0xcc);
        // Exactly what process_block pushes for a watched→watched transfer:
        // (to, In) then (from, Out), same PK columns, different address.
        let batch = BlockBatch {
            chain_id: 1,
            block_number: 50,
            block_hash: bh,
            eth: vec![],
            erc20: vec![
                Erc20TransferRow {
                    chain_id: 1,
                    block_number: 50,
                    block_hash: bh,
                    tx_hash: tx,
                    log_index: 0,
                    token,
                    from,
                    to,
                    amount: U256::from(777),
                    address: to,
                    direction: Direction::In,
                },
                Erc20TransferRow {
                    chain_id: 1,
                    block_number: 50,
                    block_hash: bh,
                    tx_hash: tx,
                    log_index: 0,
                    token,
                    from,
                    to,
                    amount: U256::from(777),
                    address: from,
                    direction: Direction::Out,
                },
            ],
            gas: vec![],
            unattributed: vec![],
        };
        write_block(l.conn_mut(), &batch).unwrap();
        write_block(l.conn_mut(), &batch).unwrap(); // replay -> still idempotent

        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM erc20_transfers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "both directional rows must survive (issue #4)");
        let n_in: i64 = l
            .conn()
            .query_row(
                "SELECT count(*) FROM erc20_transfers WHERE direction='in'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_out: i64 = l
            .conn()
            .query_row(
                "SELECT count(*) FROM erc20_transfers WHERE direction='out'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((n_in, n_out), (1, 1), "one in row and one out row");
    }

    #[test]
    fn write_unattributed_is_queryable() {
        let (mut l, _tmp) = ledger();
        let row = UnattributedDeltaRow {
            chain_id: 1,
            block_number: 42,
            block_hash: B256::repeat_byte(4),
            address: Address::repeat_byte(0xab),
            observed_wei: U256::from(7),
            attributed_wei: U256::ZERO,
            residual_wei: U256::from(7),
            cause: UnattributedDeltaCause::UnexplainedResidual,
        };
        write_unattributed(l.conn_mut(), &row).unwrap();
        write_unattributed(l.conn_mut(), &row).unwrap(); // retry -> no dup
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM unattributed_deltas", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Issue #15: writer must persist the `cause` discriminator so
    /// operators can SQL-filter skip-path markers apart from reconcile-
    /// stall residuals. Round-trip both variants through the table.
    #[test]
    fn write_unattributed_persists_cause_discriminator() {
        let (mut l, _tmp) = ledger();
        let bh1 = B256::repeat_byte(0x11);
        let bh2 = B256::repeat_byte(0x22);
        let addr = Address::repeat_byte(0xab);
        write_unattributed(
            l.conn_mut(),
            &UnattributedDeltaRow {
                chain_id: 1,
                block_number: 1,
                block_hash: bh1,
                address: addr,
                observed_wei: U256::from(7),
                attributed_wei: U256::ZERO,
                residual_wei: U256::from(7),
                cause: UnattributedDeltaCause::ParentStateUnavailable,
            },
        )
        .unwrap();
        write_unattributed(
            l.conn_mut(),
            &UnattributedDeltaRow {
                chain_id: 1,
                block_number: 2,
                block_hash: bh2,
                address: addr,
                observed_wei: U256::from(8),
                attributed_wei: U256::ZERO,
                residual_wei: U256::from(8),
                cause: UnattributedDeltaCause::UnexplainedResidual,
            },
        )
        .unwrap();
        let mut stmt = l
            .conn()
            .prepare("SELECT cause FROM unattributed_deltas ORDER BY block_number")
            .unwrap();
        let causes: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            causes,
            vec!["parent_state_unavailable", "unexplained_residual"]
        );
    }

    /// Issue #3 (C2): a DB-write fault must surface as `Err` (so the ExEx
    /// loop can retry/stall), never panic. A `query_only` connection makes
    /// every write fail with "attempt to write a readonly database" — the
    /// stand-in for disk-full / persistent SQLITE_BUSY / I/O error.
    #[test]
    fn writes_error_not_panic_on_readonly_db() {
        let (mut l, _tmp) = ledger();
        l.conn()
            .pragma_update(None, "query_only", "ON")
            .expect("inject query_only");
        let bh = B256::repeat_byte(8);
        let batch = BlockBatch {
            chain_id: 1,
            block_number: 7,
            block_hash: bh,
            eth: vec![],
            erc20: vec![],
            gas: vec![],
            unattributed: vec![],
        };
        assert!(
            write_block(l.conn_mut(), &batch).is_err(),
            "write_block must return Err on a read-only DB, not panic"
        );
        let row = UnattributedDeltaRow {
            chain_id: 1,
            block_number: 7,
            block_hash: bh,
            address: Address::repeat_byte(0xab),
            observed_wei: U256::from(1),
            attributed_wei: U256::ZERO,
            residual_wei: U256::from(1),
            cause: UnattributedDeltaCause::UnexplainedResidual,
        };
        assert!(
            write_unattributed(l.conn_mut(), &row).is_err(),
            "write_unattributed must return Err on a read-only DB, not panic"
        );
        assert!(
            delete_blocks(l.conn_mut(), 1, &[bh]).is_err(),
            "delete_blocks must return Err on a read-only DB, not panic"
        );
    }

    #[test]
    fn write_block_persists_last_block_hash() {
        let bh = alloy_primitives::B256::repeat_byte(0xab);
        let (mut led, _tmp) = ledger();
        let batch = BlockBatch {
            chain_id: 1,
            block_number: 777,
            block_hash: bh,
            eth: vec![],
            erc20: vec![],
            gas: vec![],
            unattributed: vec![],
        };
        write_block(led.conn_mut(), &batch).unwrap();
        let stored: String = led
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block_hash'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, format!("{bh:#x}"));
    }

    /// Issue #14: gap-on-restart marker writer emits one row per
    /// watched address with all-zero amounts + `block_not_delivered`
    /// cause, and atomically advances meta.last_block/last_block_hash
    /// in the SAME transaction.
    #[test]
    fn write_gap_block_marker_emits_per_address_rows_and_advances_meta() {
        let (mut l, _tmp) = ledger();
        let bh = B256::repeat_byte(0x77);
        let a = Address::repeat_byte(0xa1);
        let b = Address::repeat_byte(0xb2);
        let watched: std::collections::HashSet<Address> = [a, b].into_iter().collect();

        write_gap_block_marker(l.conn_mut(), 1, 42, bh, &watched).unwrap();

        // One row per watched address.
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM unattributed_deltas", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        // Every row has all-zero amounts and the new cause.
        let mut s = l
            .conn()
            .prepare(
                "SELECT address, observed_wei, attributed_wei, residual_wei, cause \
                   FROM unattributed_deltas WHERE block_number=42 ORDER BY address",
            )
            .unwrap();
        let rows: Vec<(String, String, String, String, String)> = s
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for (_, o, a_, res, cause) in &rows {
            assert_eq!(o, "0");
            assert_eq!(a_, "0");
            assert_eq!(res, "0");
            assert_eq!(cause, "block_not_delivered");
        }

        // Meta advanced.
        let lb: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        let lh: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block_hash'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(lb, "42");
        assert_eq!(lh, format!("{bh:#x}"));
    }

    /// Replay-safety: re-running write_gap_block_marker for the same
    /// block must not duplicate rows or corrupt meta. The natural PK
    /// (chain_id, block_hash, address) + INSERT OR REPLACE makes the
    /// write idempotent — same contract as write_block.
    #[test]
    fn write_gap_block_marker_is_idempotent() {
        let (mut l, _tmp) = ledger();
        let bh = B256::repeat_byte(0x55);
        let w = Address::repeat_byte(0xaa);
        let watched: std::collections::HashSet<Address> = [w].into_iter().collect();
        write_gap_block_marker(l.conn_mut(), 1, 11, bh, &watched).unwrap();
        write_gap_block_marker(l.conn_mut(), 1, 11, bh, &watched).unwrap();
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM unattributed_deltas", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "replay must not duplicate the marker row");
    }

    /// Edge case: empty watched set must still advance meta (so future
    /// gap detection remains correct), with zero marker rows written.
    /// The ExEx with no watched addresses is degenerate but must still
    /// track high-water gap-free.
    #[test]
    fn write_gap_block_marker_empty_watched_advances_meta_with_zero_rows() {
        let (mut l, _tmp) = ledger();
        let bh = B256::repeat_byte(0x33);
        let empty: std::collections::HashSet<Address> = Default::default();
        write_gap_block_marker(l.conn_mut(), 1, 99, bh, &empty).unwrap();
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM unattributed_deltas", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "no markers for an empty watched set");
        let lb: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lb, "99", "meta still advances on an empty fill");
    }

    /// Issue #3 / consistency with the other writers: a DB-write fault
    /// must surface as Err (so the ExEx loop can retry/stall), never
    /// panic. A `query_only` connection makes every write fail.
    #[test]
    fn write_gap_block_marker_errors_not_panics_on_readonly_db() {
        let (mut l, _tmp) = ledger();
        l.conn()
            .pragma_update(None, "query_only", "ON")
            .expect("inject query_only");
        let bh = B256::repeat_byte(0x44);
        let w = Address::repeat_byte(0xab);
        let watched: std::collections::HashSet<Address> = [w].into_iter().collect();
        assert!(
            write_gap_block_marker(l.conn_mut(), 1, 7, bh, &watched).is_err(),
            "must return Err on a read-only DB, not panic"
        );
    }

    #[test]
    fn delete_by_block_hash_removes_all_categories() {
        let (mut l, _tmp) = ledger();
        let bh = B256::repeat_byte(9);
        let batch = BlockBatch {
            chain_id: 1,
            block_number: 5,
            block_hash: bh,
            eth: vec![EthTransferRow {
                chain_id: 1,
                block_number: 5,
                block_hash: bh,
                tx_hash: Some(B256::repeat_byte(2)),
                trace_path: "0".into(),
                address: Address::repeat_byte(1),
                direction: Direction::Out,
                counterparty: Address::repeat_byte(2),
                amount_wei: U256::from(1),
                kind: EthKind::TopLevel,
                reverted: false,
            }],
            erc20: vec![],
            gas: vec![],
            unattributed: vec![],
        };
        write_block(l.conn_mut(), &batch).unwrap();
        delete_blocks(l.conn_mut(), 1, &[bh]).unwrap();
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM eth_transfers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
