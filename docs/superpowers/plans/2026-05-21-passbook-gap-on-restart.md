# Gap-on-Restart Marker-Fill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the silent-advance hazard described in [passbook#14](https://github.com/ajsutton/passbook/issues/14): when a committed-chain notification arrives with `first_block > last_durable_block + 1`, write a `block_not_delivered` `unattributed_deltas` marker per watched address for every block in the gap before advancing the ledger high-water.

**Architecture:** Single new responsibility added to the `run_passbook` driver. One new writer helper (`write_gap_block_marker`), one new `UnattributedDeltaCause` variant (`BlockNotDelivered`), one pure helper (`gap_range`) for boundary logic, and one tracker variable (`high_water`) initialised from the existing ledger resume point. The gap-fill loop sits at the top of the `committed_chain()` branch, BEFORE the per-block processing loop. Header lookups go through `ctx.provider().block_hash(n)` (`BlockHashReader`); marker writes go through the new helper; both retry forever with the existing `BACKOFF_START`/`BACKOFF_CAP` constants on `Err`, never propagating. No schema change — the v3 `cause` column accepts the new variant directly.

**Tech Stack:** Rust 2024 edition, `rusqlite` for the SQLite ledger, `reth-ethereum` / `reth-op` provider traits, `tokio` for the async retry, `reth-exex-test-utils` for the integration harness.

**Spec:** `docs/superpowers/specs/2026-05-21-passbook-gap-on-restart-design.md` (commit `890bf98` on branch `gap-on-restart-issue-14`).

---

## File Structure

Files touched, one task per file (Task 1 → Task 4) plus an integration test (Task 5):

| File | Responsibility |
|---|---|
| `crates/passbook-core/src/model.rs` | New `UnattributedDeltaCause::BlockNotDelivered` variant + roundtrip. |
| `crates/passbook-core/src/ledger/writer.rs` | New `write_gap_block_marker` helper: per-block atomic TX (markers + `meta` advance). |
| `crates/passbook-core/src/exex.rs` | New pure helper `gap_range`; wire gap detect + fill loop into `run_passbook`; add `high_water` tracker bump in the existing `write_block` success arm. |
| `crates/passbook-core/tests/exex_integration.rs` | One end-to-end test driving two notifications with a gap, asserting markers + advance. |

No new files. No schema migration. No CLI / RPC / config changes.

---

### Task 1: Add the `BlockNotDelivered` cause variant

**Files:**
- Modify: `crates/passbook-core/src/model.rs:110-136` (enum + impls + tests).

- [ ] **Step 1: Extend the round-trip test with the new variant (failing test)**

In `crates/passbook-core/src/model.rs`, locate the existing test `unattributed_delta_cause_roundtrips_as_str` (around line 166-184) and extend it as follows. The new assertions must come BEFORE the trailing `assert!(UnattributedDeltaCause::from_str("nope").is_err());` line:

```rust
    #[test]
    fn unattributed_delta_cause_roundtrips_as_str() {
        assert_eq!(
            UnattributedDeltaCause::ParentStateUnavailable.as_str(),
            "parent_state_unavailable"
        );
        assert_eq!(
            UnattributedDeltaCause::UnexplainedResidual.as_str(),
            "unexplained_residual"
        );
        // Issue #14: gap-on-restart marker discriminator.
        assert_eq!(
            UnattributedDeltaCause::BlockNotDelivered.as_str(),
            "block_not_delivered"
        );
        assert_eq!(
            UnattributedDeltaCause::from_str("parent_state_unavailable").unwrap(),
            UnattributedDeltaCause::ParentStateUnavailable
        );
        assert_eq!(
            UnattributedDeltaCause::from_str("unexplained_residual").unwrap(),
            UnattributedDeltaCause::UnexplainedResidual
        );
        assert_eq!(
            UnattributedDeltaCause::from_str("block_not_delivered").unwrap(),
            UnattributedDeltaCause::BlockNotDelivered
        );
        assert!(UnattributedDeltaCause::from_str("nope").is_err());
    }
```

- [ ] **Step 2: Run the test, confirm it fails**

```sh
cargo test -p passbook-core --lib model::tests::unattributed_delta_cause_roundtrips_as_str -- --exact
```

Expected: compile error or test failure mentioning `BlockNotDelivered` not in scope.

- [ ] **Step 3: Add the variant + impl arms**

In `crates/passbook-core/src/model.rs`:

Locate the enum at line 110-116 and extend it (add `BlockNotDelivered` AFTER `UnexplainedResidual`, keeping the existing variants' docs intact):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnattributedDeltaCause {
    /// Skip-path marker (parent state unavailable).
    ParentStateUnavailable,
    /// Live-mode reconcile residual (block STALLED).
    UnexplainedResidual,
    /// Gap-on-restart marker (issue #14): this block was canonical
    /// (header in node DB) but was NOT delivered as part of any
    /// committed-chain notification we processed; the per-block
    /// processing path did not run. `observed`/`attributed`/`residual`
    /// are all zero — the row records block existence only.
    BlockNotDelivered,
}
```

Add the `as_str` arm (line 119-124):

```rust
impl UnattributedDeltaCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ParentStateUnavailable => "parent_state_unavailable",
            Self::UnexplainedResidual => "unexplained_residual",
            Self::BlockNotDelivered => "block_not_delivered",
        }
    }
}
```

Add the `from_str` arm (line 127-136):

```rust
impl std::str::FromStr for UnattributedDeltaCause {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "parent_state_unavailable" => Ok(Self::ParentStateUnavailable),
            "unexplained_residual" => Ok(Self::UnexplainedResidual),
            "block_not_delivered" => Ok(Self::BlockNotDelivered),
            other => Err(format!("unknown unattributed_deltas cause: {other}")),
        }
    }
}
```

Also extend the doc comment on the enum (lines 87-109) to mention the new variant. Add this paragraph BEFORE the `Stored as the SQLite-side` paragraph:

```rust
/// - [`UnattributedDeltaCause::BlockNotDelivered`] — gap-on-restart
///   marker (issue #14): a committed-chain notification arrived
///   with `first_block > last_durable_block + 1`, so the driver
///   filled the missing range with one marker row per watched address
///   per gap block. `observed`/`attributed`/`residual` are all zero
///   — the row records that the block was canonical but never
///   processed through the per-block pipeline. Expected briefly on
///   restart in staged-sync mode; outside that, a cluster of these
///   rows is itself a fault signal worth investigating.
```

- [ ] **Step 4: Run the test, confirm it passes**

```sh
cargo test -p passbook-core --lib model::tests::unattributed_delta_cause_roundtrips_as_str -- --exact
```

Expected: PASS.

- [ ] **Step 5: Verify the whole crate still builds + tests pass**

```sh
cargo test -p passbook-core --lib
```

Expected: all existing tests still PASS (the `match` arms exhaustively cover the new variant only via the new code we added; no existing matches use `_` so the compiler would surface any missed match site).

- [ ] **Step 6: Commit**

```sh
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/model.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: BlockNotDelivered cause variant (issue #14)

Third UnattributedDeltaCause for gap-on-restart markers: block was
canonical but not delivered as part of any committed-chain notification
the ExEx processed. observed=attributed=residual=0 distinguishes it
from the existing skip-mode and reconcile-stall variants which carry
non-zero bundle deltas.

No schema change: the v3 cause column (TEXT NOT NULL, issue #15)
accepts the new on-disk spelling 'block_not_delivered' directly.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Add the `write_gap_block_marker` writer

**Files:**
- Modify: `crates/passbook-core/src/ledger/writer.rs` (new fn + 3 unit tests).

- [ ] **Step 1: Add the failing unit test (basic happy path)**

Append this test to `crates/passbook-core/src/ledger/writer.rs`'s `#[cfg(test)] mod tests` (after `write_block_persists_last_block_hash`, before `delete_by_block_hash_removes_all_categories`):

```rust
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
```

- [ ] **Step 2: Run the test, confirm it fails**

```sh
cargo test -p passbook-core --lib ledger::writer::tests::write_gap_block_marker_emits_per_address_rows_and_advances_meta -- --exact
```

Expected: compile error — `write_gap_block_marker` is not in scope.

- [ ] **Step 3: Implement `write_gap_block_marker`**

Add this function to `crates/passbook-core/src/ledger/writer.rs`, right after the existing `write_unattributed` function (after line 130, before `delete_blocks`):

```rust
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
```

- [ ] **Step 4: Run the test, confirm it passes**

```sh
cargo test -p passbook-core --lib ledger::writer::tests::write_gap_block_marker_emits_per_address_rows_and_advances_meta -- --exact
```

Expected: PASS.

- [ ] **Step 5: Add the idempotency test (failing)**

Append after the test from Step 1:

```rust
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
```

- [ ] **Step 6: Run it, confirm PASS (idempotency is structural, no code change needed)**

```sh
cargo test -p passbook-core --lib ledger::writer::tests::write_gap_block_marker_is_idempotent -- --exact
```

Expected: PASS (the function already uses `INSERT OR REPLACE`).

- [ ] **Step 7: Add the empty-watched test (failing)**

Append after Step 5's test:

```rust
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
```

- [ ] **Step 8: Run it, confirm PASS**

```sh
cargo test -p passbook-core --lib ledger::writer::tests::write_gap_block_marker_empty_watched_advances_meta_with_zero_rows -- --exact
```

Expected: PASS.

- [ ] **Step 9: Add the readonly-DB test (failing → PASS)**

Append after Step 7's test:

```rust
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
```

- [ ] **Step 10: Run it, confirm PASS**

```sh
cargo test -p passbook-core --lib ledger::writer::tests::write_gap_block_marker_errors_not_panics_on_readonly_db -- --exact
```

Expected: PASS (the `?` operators propagate the rusqlite error).

- [ ] **Step 11: Run the whole writer test suite**

```sh
cargo test -p passbook-core --lib ledger::writer::tests
```

Expected: all existing tests still PASS, the four new tests PASS.

- [ ] **Step 12: Commit**

```sh
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/ledger/writer.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: write_gap_block_marker writer (issue #14)

One DB TX per gap block: insert per-watched-address
unattributed_deltas marker (cause=block_not_delivered, all-zero
amounts), atomic with meta.last_block / last_block_hash advance.
Empty watched set still advances meta. Idempotent via INSERT OR
REPLACE on the (chain_id, block_hash, address) natural PK; replay
after a crash mid-gap-fill is a no-op.

Mirrors write_block's per-block-TX shape so the existing reorg
delete_blocks(by block_hash) path reorg-cleans gap markers with no
new code.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Add the `gap_range` pure helper

**Files:**
- Modify: `crates/passbook-core/src/exex.rs` (new pure helper + 4 unit tests in the existing `tests` mod).

- [ ] **Step 1: Write the failing test**

Append these tests to the `#[cfg(test)] mod tests` block at the bottom of `crates/passbook-core/src/exex.rs` (after the existing `inner_does_not_call_parent_state_thunk_when_no_watched_change` test):

```rust
    /// Issue #14: gap detection — strict `>` so reorg re-delivery
    /// (committed.first() <= high_water) stays on the existing
    /// idempotent path; only true gaps trigger the fill.
    #[test]
    fn gap_range_strict_inequality() {
        // No prior high-water: no gap (first notification establishes it).
        assert_eq!(gap_range(None, 100), None);
        // In-order: committed.first() == high_water + 1 ⇒ no gap.
        assert_eq!(gap_range(Some(10), 11), None);
        // Reorg redelivery: committed.first() == high_water ⇒ no gap.
        assert_eq!(gap_range(Some(10), 10), None);
        // Reorg redelivery deeper: committed.first() < high_water ⇒ no gap.
        assert_eq!(gap_range(Some(10), 5), None);
    }

    #[test]
    fn gap_range_one_block_gap() {
        // committed.first() = 12, high_water = 10 ⇒ gap [11..=11].
        assert_eq!(gap_range(Some(10), 12), Some(11..=11));
    }

    #[test]
    fn gap_range_multi_block_gap() {
        // committed.first() = 30, high_water = 10 ⇒ gap [11..=29].
        assert_eq!(gap_range(Some(10), 30), Some(11..=29));
    }

    #[test]
    fn gap_range_high_water_zero() {
        // Genesis (block 0) durable, next committed is 5 ⇒ gap [1..=4].
        assert_eq!(gap_range(Some(0), 5), Some(1..=4));
    }
```

- [ ] **Step 2: Run, confirm it fails**

```sh
cargo test -p passbook-core --lib exex::tests::gap_range -- --exact
cargo test -p passbook-core --lib exex::tests::gap_range_strict_inequality -- --exact
```

Expected: compile error — `gap_range` not in scope.

- [ ] **Step 3: Implement the helper**

Add this function to `crates/passbook-core/src/exex.rs`, placed immediately after the existing `lag_finished` helper (around line 328-333):

```rust
/// Issue #14: gap-on-restart detection. Returns `Some(range)` if a
/// committed-chain notification's first block is beyond
/// `high_water + 1` — i.e. the ExEx missed delivery of every block in
/// `[high_water + 1 ..= first_committed - 1]`. Returns `None` for
/// fresh-datadir (no prior high-water), in-order (`first_committed ==
/// high_water + 1`), and reorg-redelivery (`first_committed <=
/// high_water`) cases. The strict `>` is essential: `<=` is the
/// reorg-redelivery path and must stay on the existing per-block
/// INSERT-OR-REPLACE writer.
pub(crate) fn gap_range(
    high_water: Option<u64>,
    first_committed: u64,
) -> Option<std::ops::RangeInclusive<u64>> {
    let hw = high_water?;
    if first_committed > hw + 1 {
        Some((hw + 1)..=(first_committed - 1))
    } else {
        None
    }
}
```

- [ ] **Step 4: Run all 4 gap-range tests, confirm they pass**

```sh
cargo test -p passbook-core --lib exex::tests::gap_range
```

Expected: 4 tests PASS.

- [ ] **Step 5: Commit**

```sh
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: gap_range pure detection helper (issue #14)

Returns Some(range) iff committed.first() > high_water + 1 (the
gap-on-restart case). Strict > leaves the reorg-redelivery and
in-order cases on None so the existing per-block path handles them
unchanged.

Four unit tests cover: no high_water, in-order, reorg-redelivery,
one-block gap, multi-block gap, high_water=0 genesis case.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Wire gap detect + fill into `run_passbook`

**Files:**
- Modify: `crates/passbook-core/src/exex.rs:396-595` (the `run_passbook` body).

- [ ] **Step 1: Add the `BlockHashReader` import + tracker initialisation**

At the top of `crates/passbook-core/src/exex.rs`, add to the existing imports (around line 19-20, alongside the other `reth_ethereum` imports):

```rust
use reth_ethereum::storage::{BlockHashReader, StateProviderBox, StateProviderFactory};
```

This replaces the existing `use reth_ethereum::storage::{StateProviderBox, StateProviderFactory};` import (line 20).

Also import the new writer in the same file:

```rust
use crate::ledger::writer::{delete_blocks, write_block, write_gap_block_marker, write_unattributed};
```

This replaces the existing `use crate::ledger::writer::{delete_blocks, write_block, write_unattributed};` (line 5).

Inside `run_passbook` (line 396 onwards), modify the existing resume-head block (lines 416-429) to capture the high-water number into a mutable local:

Replace this block:

```rust
    {
        let head = {
            let guard = ledger.lock().unwrap_or_else(|e| e.into_inner());
            crate::ledger::queries::resume_head(guard.conn())?
        };
        if let Some((number, hash)) = head {
            ctx.set_notifications_with_head(reth_ethereum::exex::ExExHead {
                block: alloy_eips::BlockNumHash { number, hash },
            });
            tracing::info!(number, %hash, "passbook ExEx resuming with head");
        } else {
            tracing::info!("passbook ExEx starting head-less (fresh datadir)");
        }
    }
```

With this:

```rust
    let mut high_water: Option<u64> = {
        let head = {
            let guard = ledger.lock().unwrap_or_else(|e| e.into_inner());
            crate::ledger::queries::resume_head(guard.conn())?
        };
        if let Some((number, hash)) = head {
            ctx.set_notifications_with_head(reth_ethereum::exex::ExExHead {
                block: alloy_eips::BlockNumHash { number, hash },
            });
            tracing::info!(number, %hash, "passbook ExEx resuming with head");
            Some(number)
        } else {
            tracing::info!("passbook ExEx starting head-less (fresh datadir)");
            None
        }
    };
```

- [ ] **Step 2: Wire the high_water bump into the per-block write-success arm**

Inside the per-block processing loop (around line 500-525), the existing `Ok(())` arm of the `write_block` retry breaks the inner loop. Update it to also advance the tracker. Replace this:

```rust
                            match res {
                                Ok(()) => break,
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        "ExEx durable block write failed, retrying (not advancing)"
                                    );
                                    tokio::time::sleep(backoff).await;
                                    backoff = (backoff * 2).min(BACKOFF_CAP);
                                    continue;
                                }
                            }
```

With this:

```rust
                            match res {
                                Ok(()) => {
                                    use alloy_consensus::BlockHeader;
                                    high_water = Some(block.header().number());
                                    break;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        "ExEx durable block write failed, retrying (not advancing)"
                                    );
                                    tokio::time::sleep(backoff).await;
                                    backoff = (backoff * 2).min(BACKOFF_CAP);
                                    continue;
                                }
                            }
```

The `use alloy_consensus::BlockHeader;` is needed in scope for `.header().number()`; if it's already imported elsewhere in the function (look for the same import in `process_committed_block_inner`), the local `use` becomes redundant but harmless.

- [ ] **Step 3: Insert the gap-fill block at the top of the `committed_chain()` branch**

In `run_passbook` (around line 480), the `if let Some(chain) = notification.committed_chain()` branch currently jumps straight into `for block in chain.blocks_iter()`. Insert the gap-fill BEFORE the per-block loop. The full updated branch looks like this:

```rust
        if let Some(chain) = notification.committed_chain() {
            // Issue #14: gap detection — if the first committed block
            // is beyond high_water+1, fill the gap with one
            // block_not_delivered marker per watched address per
            // missing block BEFORE processing the live notification.
            // Header fetches / marker writes retry forever with bounded
            // backoff on Err; we never advance past a block we cannot
            // mark.
            let first_committed = {
                use alloy_consensus::BlockHeader;
                chain.first().header().number()
            };
            if let Some(range) = gap_range(high_water, first_committed) {
                let gap_size = range.end() - range.start() + 1;
                tracing::warn!(
                    gap_first = *range.start(),
                    gap_last = *range.end(),
                    gap_size,
                    "gap-on-restart: filling block_not_delivered markers"
                );
                for n in range.clone() {
                    let mut backoff = BACKOFF_START;
                    loop {
                        let bh = match ctx.provider().block_hash(n) {
                            Ok(Some(h)) => h,
                            Ok(None) => {
                                tracing::error!(
                                    block = n,
                                    "gap-fill: header missing (block_hash returned None), retrying (not advancing)"
                                );
                                tokio::time::sleep(backoff).await;
                                backoff = (backoff * 2).min(BACKOFF_CAP);
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    block = n,
                                    "gap-fill: block_hash lookup failed, retrying (not advancing)"
                                );
                                tokio::time::sleep(backoff).await;
                                backoff = (backoff * 2).min(BACKOFF_CAP);
                                continue;
                            }
                        };
                        let res = write_gap_block_marker(
                            ledger.lock().unwrap_or_else(|e| e.into_inner()).conn_mut(),
                            chain_id,
                            n,
                            bh,
                            &cfg.watched,
                        );
                        match res {
                            Ok(()) => {
                                high_water = Some(n);
                                break;
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    block = n,
                                    "gap-fill: marker write failed, retrying (not advancing)"
                                );
                                tokio::time::sleep(backoff).await;
                                backoff = (backoff * 2).min(BACKOFF_CAP);
                            }
                        }
                    }
                }
                tracing::warn!(
                    gap_first = *range.start(),
                    gap_last = *range.end(),
                    gap_size,
                    "gap-on-restart: complete, advanced high_water to gap_last"
                );
            }

            for block in chain.blocks_iter() {
                // ...existing per-block loop, unchanged...
```

Keep the existing `for block in chain.blocks_iter() { ... }` body unchanged below this; only the gap-fill block is inserted BEFORE it.

- [ ] **Step 4: Compile-check (no test yet — TDD for the wiring comes via Task 5)**

```sh
cargo build -p passbook-core
```

Expected: clean build. If the build complains about `BlockHashReader` not being in scope at the `ctx.provider().block_hash(n)` call site, ensure the import in Step 1 (`use reth_ethereum::storage::{BlockHashReader, ...};`) is correct. If `reth_ethereum::storage::BlockHashReader` isn't re-exported, fall back to `use reth_ethereum::provider::BlockHashReader;` — confirm by `cargo doc -p reth-ethereum --no-deps` or by grep of `reth_ethereum::storage` re-exports.

- [ ] **Step 5: Run the existing exex unit tests for regression**

```sh
cargo test -p passbook-core --lib exex::tests
```

Expected: every existing test still PASSES. The new code only fires when the gap path is taken; no existing test fixture sets up a gap, so they are all on the unchanged path.

- [ ] **Step 6: Commit (driver wiring only — integration test follows in Task 5)**

```sh
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: gap-on-restart detect + fill in run_passbook (issue #14)

At the top of each Committed notification branch, if
committed.first() > high_water+1 (strict), fill the gap with
write_gap_block_marker for every missing block. Header lookups via
ctx.provider().block_hash(n) (BlockHashReader; pre-Finish-gate,
available throughout staged-sync catch-up). Both header-fetch and
marker-write Err route to the same retry-forever bounded-backoff
loop used by reorg-delete and write_block; we never advance past a
block we cannot mark.

high_water tracker initialised from the existing resume_head ledger
lookup; bumped on every successful write_block AND every successful
gap-marker write. The diagnostic write_unattributed in the
UnexplainedResidual stall arm intentionally does NOT advance
high_water (the block is not durable — we are stalled on it).

No schema change. Reorgs reuse the existing delete_blocks(by
block_hash) path; gap markers reorg-clean automatically.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: End-to-end integration test for the gap-fill happy path

**Files:**
- Modify: `crates/passbook-core/tests/exex_integration.rs` (append one new test at the end).

**Test strategy:** the integration harness's `notification_sender` lets us emit Committed notifications with arbitrary chains. To simulate the gap-on-restart scenario in a single test, we pre-populate `meta.last_block`/`last_block_hash` to a non-canonical synthetic value (simulating "I crashed after writing block N"), then send a fresh notification whose first block is beyond `N+1`, then assert markers were written for the gap range. The provider's `block_hash(n)` must return a hash for every block in `[N+1, committed.first()-1]` — we satisfy this by building a single physical chain of blocks 1..=K where K covers the gap+committed range, and sending only the *tail* of that chain as the notification. Headers for the un-delivered slice live in the provider via the `notification_sender`'s prior context.

- [ ] **Step 1: Read the existing test fixtures to understand the harness**

Skim `crates/passbook-core/tests/exex_integration.rs` lines 1-500 (the `erc20_internal_gas_capture_zero_residual` test and its helpers `make_genesis`, `build_block`, `sign_legacy`, `acct`). The fixture pattern for a single block: build a `ChainSpec` via `make_genesis`, execute a block with `EthEvmConfig`, wrap in `Chain`, send via `handle.send_notification_chain_committed(chain)`, then `run_passbook` to completion (via `handle.run_exex_until_idle()` or equivalent — look at the existing call site).

Also note line 408 and 632: `handle.provider_factory.history_by_block_hash(...)` is how the existing tests give `run_passbook` a working state provider. The provider factory exposes `BlockHashReader` directly, but `ctx.provider().block_hash(n)` inside `run_passbook` goes through whatever `Node::Provider` the harness wires up — which should also implement `BlockHashReader`. Confirm by reading the test's `test_exex_context_with_chain_spec` return values.

- [ ] **Step 2: Append the failing integration test**

At the end of `crates/passbook-core/tests/exex_integration.rs`, append:

```rust
/// Issue #14 (C4): a committed-chain notification whose first block is
/// beyond `high_water + 1` must NOT silently advance the ledger
/// high-water. The driver fills the gap with one
/// `block_not_delivered` `unattributed_deltas` marker per watched
/// address per gap block before processing the live notification.
///
/// Setup: drive two committed-chain notifications in sequence.
///   * Notification 1 — committed chain of block 1 only (so the
///     ledger writes block 1's row + advances meta.last_block=1).
///   * Notification 2 — committed chain of block 4 only. The
///     provider's chain canonically contains blocks 1..=4 (built
///     in advance and seeded via notification 1 + intervening
///     headers), so `block_hash(2)` and `block_hash(3)` succeed.
///     The notification's first block (= 4) is `high_water + 1 + 2`
///     ⇒ gap = [2, 3] ⇒ fill 2 markers per watched address.
///
/// Asserts:
///   - `meta.last_block = 4` after notification 2.
///   - Exactly `2 * watched.len()` rows in `unattributed_deltas`
///     with `cause = 'block_not_delivered'`, one per (gap_block,
///     watched_address) pair.
///   - All marker rows have `observed_wei = attributed_wei =
///     residual_wei = '0'`.
///   - The committed block 4 itself wrote its normal per-block rows
///     (no gap-marker confusion).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_on_restart_writes_block_not_delivered_markers() {
    // ── Setup: chain spec + watched address + genesis with balance.
    let signer = PrivateKeySigner::random();
    let s_addr = signer.address();
    let w = Address::repeat_byte(0xCC);
    let genesis = make_genesis(
        1,
        &[
            (s_addr, acct(U256::from(10u64).pow(U256::from(18)), None)),
            (w, acct(U256::ZERO, None)),
        ],
    );
    let chain_spec = Arc::new(ChainSpec::from_genesis(genesis));

    let (ctx, handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("harness");

    // Build a synthetic 4-block chain whose blocks each transfer 1 wei
    // from s → w. We deliver block 1, then block 4 only (creating a
    // 2-block gap at 2, 3).
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let mut parent_hash = chain_spec.genesis_hash();
    let mut blocks: Vec<RecoveredBlock<reth_ethereum::Block>> = Vec::new();
    let mut nonce: u64 = 0;
    for block_number in 1u64..=4 {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(w),
            value: U256::from(1u64),
            input: Bytes::new(),
        };
        nonce += 1;
        let signed = sign_legacy(&signer, tx);
        let block = build_block(
            &evm_config,
            chain_spec.clone(),
            parent_hash,
            block_number,
            vec![signed],
            &handle.provider_factory,
        );
        parent_hash = block.hash();
        blocks.push(block);
    }

    // Open a ledger + watch set.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("pb.db");
    let ledger = Arc::new(Mutex::new(Ledger::open(&db, 1).unwrap()));
    let mut cfg = PassbookConfig::default();
    cfg.watched = [w].into_iter().collect();

    // Spawn run_passbook.
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg,
        ledger.clone(),
        passbook_core::chain::EthChainExec::new(L1Adapter),
    ));

    // ── Notification 1: deliver only block 1 — establishes high_water = 1.
    let outcome_1 = ExecutionOutcome::default(); // populated by build_block via handle.provider_factory
    let chain_1 = Chain::new(vec![blocks[0].clone()], outcome_1, None);
    handle
        .send_notification_chain_committed(chain_1)
        .await
        .expect("send notif 1");
    handle.wait_until_idle(Duration::from_secs(10)).await;

    // ── Sanity: meta.last_block == 1 after notif 1.
    {
        let guard = ledger.lock().unwrap();
        let lb: String = guard
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lb, "1", "high_water established at 1 after notif 1");
    }

    // ── Notification 2: deliver block 4 only — gap = [2, 3].
    let outcome_4 = ExecutionOutcome::default();
    let chain_4 = Chain::new(vec![blocks[3].clone()], outcome_4, None);
    handle
        .send_notification_chain_committed(chain_4)
        .await
        .expect("send notif 2");
    handle.wait_until_idle(Duration::from_secs(10)).await;

    // ── Assert: meta.last_block == 4 (gap fill + block 4 write).
    let guard = ledger.lock().unwrap();
    let lb: String = guard
        .conn()
        .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lb, "4", "high_water advanced to 4 (gap-filled + block 4 processed)");

    // ── Assert: exactly 2 block_not_delivered markers (blocks 2 and 3),
    //   one per watched address.
    let n_markers: i64 = guard
        .conn()
        .query_row(
            "SELECT count(*) FROM unattributed_deltas \
              WHERE cause = 'block_not_delivered'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        n_markers, 2,
        "two gap-fill markers (1 watched x 2 gap blocks)"
    );

    // ── Assert: every marker has all-zero amounts + the gap block numbers.
    let mut s = guard
        .conn()
        .prepare(
            "SELECT block_number, observed_wei, attributed_wei, residual_wei \
               FROM unattributed_deltas WHERE cause = 'block_not_delivered' \
               ORDER BY block_number",
        )
        .unwrap();
    let rows: Vec<(i64, String, String, String)> = s
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 2);
    assert_eq!(rows[1].0, 3);
    for (_, o, a, res) in &rows {
        assert_eq!(o, "0");
        assert_eq!(a, "0");
        assert_eq!(res, "0");
    }

    // ── Assert: block 4 itself processed through the per-block path
    //    (it transfers 1 wei to W; expect a gas_payment + eth_transfer
    //    row for block 4 — NOT a block_not_delivered marker).
    let n_eth_4: i64 = guard
        .conn()
        .query_row(
            "SELECT count(*) FROM eth_transfers WHERE block_number = 4",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        n_eth_4 >= 1,
        "block 4 must have at least one normal eth_transfer row, got {n_eth_4}"
    );
    let n_unattr_4: i64 = guard
        .conn()
        .query_row(
            "SELECT count(*) FROM unattributed_deltas WHERE block_number = 4",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        n_unattr_4, 0,
        "block 4 was delivered normally; no gap marker for it"
    );

    drop(guard);
    drop(driver); // exit cleanly
}
```

Notes for the implementer:
  - `build_block` already lives in this file (line 235). It executes the block against the harness's provider, so its receipts/bundle state end up durable in `handle.provider_factory` and `block_hash(n)` then resolves for every built block — including the un-delivered blocks 2 and 3.
  - `ExecutionOutcome::default()` is a stand-in; if the existing fixtures pass a real outcome (look at how `chain_1` is constructed in other tests), copy that pattern. The point of this test is the gap markers, not the live block's content.
  - `handle.wait_until_idle(...)`: the exact helper name in this harness may be `wait_until_idle`, `poll_until_idle`, or similar — search for `wait_until` in `crates/passbook-core/tests/exex_integration.rs` and copy the working call.

- [ ] **Step 3: Run the test, confirm it fails (or compiles + asserts fire)**

```sh
cargo test -p passbook-core --test exex_integration gap_on_restart_writes_block_not_delivered_markers -- --nocapture
```

Expected: FAIL at one of the assertions, OR compile error if any helper name above is wrong. If it's a helper-name issue, fix by grepping for the actual symbol in the file:

```sh
grep -n "wait_until\|run_exex_until\|notification_sender" crates/passbook-core/tests/exex_integration.rs | head -20
```

- [ ] **Step 4: Fix any harness-name discrepancies and re-run**

Most likely fixes:
- `wait_until_idle(Duration)` → whatever method the harness exposes for "wait until the ExEx has consumed all pending notifications". If it's `Handle::wait_for_finished_height(target)`, swap accordingly.
- `Chain::new(blocks, outcome, ...)` arity: copy from an existing fixture (search `Chain::new` in the file).
- Sending notifications: confirm `send_notification_chain_committed(chain).await` is the right async signature.

Re-run after each fix.

- [ ] **Step 5: Confirm the test passes against the wired driver from Task 4**

Expected output: 1 PASS.

- [ ] **Step 6: Run the full integration suite for regression**

```sh
cargo test -p passbook-core --test exex_integration
```

Expected: every existing integration test still PASSES. The new test PASSES. The gap-detect logic only fires when `committed.first() > high_water + 1`; every existing fixture sends in-order or fresh-datadir notifications, so the new code is inert in all of them.

- [ ] **Step 7: Run the entire workspace test suite**

```sh
cargo test --workspace
```

Expected: zero new failures.

- [ ] **Step 8: Commit**

```sh
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/tests/exex_integration.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: integration test — gap-on-restart fills markers (issue #14)

End-to-end fixture: drive two committed-chain notifications with a
2-block gap between them, assert (a) two block_not_delivered markers
written (one per watched-address per gap block), (b) all markers
have all-zero observed/attributed/residual, (c) ledger high_water
advanced to the second notification's tip, (d) the live block from
notification 2 wrote its normal per-block rows (not confused with
gap markers).

Regression test for the silent-advance hazard the original wedge
exposed; the new gap-on-restart path is inert for any existing
test fixture (in-order notifications do not trigger the > strict
gap-detect guard).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Final verification + image rebuild handoff

**Files:**
- (no edits — verification only)

- [ ] **Step 1: Full workspace check + lint**

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all clean.

- [ ] **Step 2: Manual review of the diff against the spec**

```sh
git -C /Users/aj/Documents/code/passbook log --oneline main..HEAD
git -C /Users/aj/Documents/code/passbook diff main..HEAD -- \
  crates/passbook-core/src/model.rs \
  crates/passbook-core/src/ledger/writer.rs \
  crates/passbook-core/src/exex.rs \
  crates/passbook-core/tests/exex_integration.rs
```

Read the diff against `docs/superpowers/specs/2026-05-21-passbook-gap-on-restart-design.md` § "Components touched" — every numbered item should be visible in the diff.

- [ ] **Step 3: Hand off to the user**

Tell the user:
- All tests pass on branch `gap-on-restart-issue-14`.
- The diff is N commits (one per task): cause variant, writer helper, gap_range pure helper, run_passbook wiring, integration test.
- Deployment per the spec's § "Deployment": rebuild + push image, bump big-docker `op-mainnet`/`op-sepolia` tags. Operator pushes big-docker.
- DO NOT push or open a PR without explicit user approval (per `~/.claude/CLAUDE.md` and the big-docker repo's commit/push rules).

---

## Self-Review

**Spec coverage check (every section of `docs/superpowers/specs/2026-05-21-passbook-gap-on-restart-design.md`):**

- § Design principle (never silent advance) → Task 4 (gap detect + fill + retry-forever).
- § Why not reth-side backfill → covered in spec / out-of-scope by design; no plan task.
- § Behaviour table (4 rows) → all covered: in-order (no code), reorg (no code, strict `>`), gap (Task 4), fresh datadir (Task 4's `None` arm). Err path → Task 4's retry-forever loop.
- § Marker contract (column-by-column) → Task 2 (writer) + Task 5 (integration test asserting every column).
- § Components touched item 1 (model.rs) → Task 1.
- § Components touched item 2 (writer.rs) → Task 2.
- § Components touched item 3 (run_passbook) → Task 4.
- § Components touched item 4 (tests) → unit tests in Tasks 1/2/3; integration test in Task 5.
- § Data / schema (no migration) → Task 2 confirms by exercising the existing `cause` column.
- § Reorgs → Task 4's strict `>` guard + existing `delete_blocks` reuse (no code change for reorg-clean of markers).
- § Performance (per-block TX) → Task 2's `write_gap_block_marker` implements per-block TX.
- § Logging → Task 4's two WARN lines + per-block ERROR on Err.
- § Out of scope → no plan tasks (correct).
- § Risks → header availability assumption tested implicitly by Task 5 (build_block populates the provider so block_hash(n) resolves).

**Placeholder scan:** none found. Every code step shows the actual code; every shell step shows the actual command + expected outcome.

**Type consistency:**
- `UnattributedDeltaCause::BlockNotDelivered` — defined Task 1, used Task 2 (writer), used Task 5 (test assertions).
- `write_gap_block_marker(conn, chain_id, block_number, block_hash, watched)` — defined Task 2, called Task 4. Signature matches across both.
- `gap_range(high_water: Option<u64>, first_committed: u64) -> Option<RangeInclusive<u64>>` — defined Task 3, called Task 4. Signature matches.
- `high_water: Option<u64>` — declared Task 4 Step 1, mutated Task 4 Steps 2 + 3.
- `BlockHashReader::block_hash(&self, n: BlockNumber) -> ProviderResult<Option<B256>>` — used Task 4 Step 3. Trait verified in reth pinned source.

No inconsistencies.
