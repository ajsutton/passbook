# Design: gap-free advancement across notification gaps in the passbook ExEx

**Status:** Approved (brainstormed 2026-05-21).
**Issue:** [passbook#14](https://github.com/ajsutton/passbook/issues/14) — C4 (critical): ExEx silently advances across an un-delivered block range after restart (no backfill, no markers, no error).
**Related:** parent-state skip-mode design at `docs/superpowers/specs/2026-05-20-passbook-parent-state-skip-design.md` (removes the upstream *stall* that caused the production wedge but does not address the gap-on-restart mechanism itself).

## Context (why this change exists)

When `run_passbook` resumes after a restart it calls
`ctx.set_notifications_with_head(ExExHead { number: last_block, hash: last_block_hash })`.
reth's `ExExNotificationsWithHead` (`crates/exex/exex/src/notifications.rs:365-415`)
then decides whether to schedule a `backfill_job` to bridge the exex head to
the node's canonical head:

```rust
match self.initial_exex_head.block.number.cmp(&self.initial_local_head.number) {
    Less    => { self.backfill_job = Some(factory.with_range(exex_head+1 ..= local_head)); }
    Equal | Greater => { /* no backfill */ }
}
```

In a **staged-pipeline sync** the node's canonical `local_head` is the
Finish-stage checkpoint — pinned at the bootstrap block (e.g. Bedrock
genesis) for the entire multi-week catch-up. Meanwhile the Execution
stage advances independently far ahead. If the exex stalls at block `N`
and the container restarts:

- `initial_exex_head = N` (e.g. 114,804,380 — the last block we durably wrote).
- `initial_local_head ≈ bootstrap checkpoint` (e.g. 105,235,063).
- `N > local_head` ⇒ **no backfill job runs**.
- The stream then consumes new `ExExNotificationSource::Pipeline`
  notifications. The reth-side filter
  `if exex_head.number >= committed.tip().number() { continue }`
  passes the next post-restart batch through unchanged.
- The next batch's first block is far above `N+1`. `run_passbook` writes
  the batch, emits `FinishedHeight(new_tip)`, and **block `N+1` is never
  re-delivered. No row. No marker. No error.**

**Observed in production (op-mainnet, 2026-05-19 → 2026-05-20):** the
wedge at block 114,804,381 silently advanced to ~116,085,739 across
restart cycles — ~1.28 M blocks unprocessed, including the first-ever
activity for one of the watched addresses (initial deposit-mint funding
of `0xa4B572eA…4d60`), with zero record in the ledger.

The parent-state skip-mode landed 2026-05-20 fixes the *upstream stall*
that triggered this incident, but the underlying gap-on-restart
mechanism is independent: any future stall (panic, disk I/O hang,
deadlock, OOM-then-restart, an unanticipated `Err` in any processing
path) reproduces silent block-skipping on the next start. That is a
class-C data-loss hazard: the ledger silently lies about coverage and no
operator signal fires.

## Design principle

> **Never advance the ledger high-water across an unprocessed block
> range. If a committed-chain notification's first block is beyond
> `last_block + 1`, fill the gap with durable markers — one per watched
> address per missing block — *before* processing the notification. If
> the markers cannot be written, stall (retry forever); never silently
> advance.**

The marker is the **block-existence** record: "we know this block was
canonical (its header is in the node DB), we did not run the per-block
processing path on it, attribution is incomplete." Operators can audit
which blocks were missed and decide whether to re-index them later.

## Why not reth-side backfill (Option 1)?

The issue ranked driver-side `BackfillJobFactory::backfill(...)` as the
preferred path *if viable*. It is **not viable in the staged-sync
window that produces the gap**. `BackfillJob::execute_range`
(reth `crates/exex/exex/src/backfill/job.rs:79-83` at pinned rev
`e8c29c9`) seeds its executor with
`self.provider.history_by_block_number(self.range.start().saturating_sub(1))`
— the same Finish-stage-gated state-provider call that
`get_parent_state()` fails on during staged sync. For the exact range we
would want to backfill (blocks past the bootstrap Finish checkpoint) the
job cannot even construct its executor, let alone iterate blocks.

In **live mode** (Finish ≈ tip) reth's own with-head backfill scheduled
by `set_notifications_with_head` already closes the gap before our
notification stream resumes — so the driver-side backfill would fire on
an already-empty range. Adding it as a third tier buys nothing for the
case that motivates this fix, while doubling the code paths the
processing loop has to handle.

This design therefore implements the issue's Option 3 (hard-stop) and
Option 2 (marker-and-advance) as a single integrated path: gap detect →
write block-existence markers per gap block → advance, with retry-
forever stall on any marker-write failure. Coverage is gap-free in both
staged-sync and live modes with a single mechanism.

## Behaviour

| State | `committed.first().number()` vs `high_water + 1` | Outcome |
|---|---|---|
| Normal (in-order) | `==` | No gap-fill. Existing per-block processing path. Unchanged. |
| Reorg-redelivery | `<=` | No gap-fill (`>` is strict). Existing reorg-first delete + idempotent `INSERT OR REPLACE` per-block writes handle it. Unchanged. |
| Gap after restart | `>` | Per gap block in `[high_water+1 ..= committed.first()-1]`: fetch header hash, write one `unattributed_deltas` row per watched address with `cause = block_not_delivered` and `observed = attributed = residual = 0`, atomically advance `meta.last_block`/`last_block_hash`. Then process the committed chain normally. |
| Fresh datadir | (no high_water) | No gap-fill (first notification establishes the high water). Unchanged. |
| Gap-fill: header fetch / marker write Err | — | Retry forever with bounded exponential backoff (`BACKOFF_START` → `BACKOFF_CAP`), exactly like the existing reorg-delete and `write_block` retry sites. The ExEx does not advance and does not emit `FinishedHeight`. |

The strict `>` comparison is essential: `<=` is the reorg re-delivery
case and must remain on the existing path (the reorg branch already
fired the `delete_blocks` and the per-block loop will `INSERT OR
REPLACE` the new canonical rows).

## Marker contract

Per gap block, per watched address (one row each):

| Column | Value |
|---|---|
| `chain_id` | current chain (from `ctx.config.chain.chain_id()`). |
| `block_number` | gap block number `N`. |
| `block_hash` | `ctx.provider().block_hash(N)?` (`BlockHashReader` trait) — the canonical hash; Headers stage runs before Execution/Finish, so this is available even during staged-sync catch-up. |
| `address` | one row per `addr ∈ cfg.watched`. |
| `observed_wei` | `0` (we did not observe deltas — the block bypassed our processing path entirely). |
| `attributed_wei` | `0`. |
| `residual_wei` | `0`. |
| `cause` | `block_not_delivered` (new variant). |

The all-zeros signature plus the `cause` discriminator distinguishes
gap-block markers from the two existing kinds:

- `parent_state_unavailable`: skip-mode partial batch; `observed_wei =
  |bundle_delta|`, `residual_wei = |bundle_delta|`.
- `unexplained_residual`: live-mode stall diagnostic; same shape as
  above.

Operator query for gap-block coverage:

```sql
SELECT block_number, block_hash, count(*) AS watched_addresses
  FROM unattributed_deltas
 WHERE cause = 'block_not_delivered'
   AND chain_id = ?
 GROUP BY block_number
 ORDER BY block_number;
```

**`watched` is empty:** the PK is `(chain_id, block_hash, address)`,
so an empty watched set yields zero marker rows for that block. The
gap-fill loop still bumps `meta.last_block`/`last_block_hash` per
block (atomic with whatever marker rows it does write — zero or
more), so high-water tracking stays correct. Empty `watched` is
already a degenerate configuration (the ExEx has nothing to capture);
gap detection in that mode is still correct, just informationless.

## Components touched

1. **`crates/passbook-core/src/model.rs`** — add a third
   `UnattributedDeltaCause` variant:
   ```rust
   pub enum UnattributedDeltaCause {
       ParentStateUnavailable,
       UnexplainedResidual,
       /// Gap-on-restart marker: this block was canonical (header in
       /// node DB) but was not delivered as part of any committed-chain
       /// notification we processed; the per-block processing path did
       /// not run. `observed`/`attributed`/`residual` are all zero — the
       /// row records block existence only.
       BlockNotDelivered,
   }
   ```
   `as_str()` → `"block_not_delivered"`; matching `from_str` arm. The
   roundtrip unit test (`unattributed_delta_cause_roundtrips_as_str`)
   gains a third assertion.

2. **`crates/passbook-core/src/ledger/writer.rs`** — add a new helper:
   ```rust
   /// One DB transaction: insert one `unattributed_deltas` row per
   /// watched address for block `(number, hash)` with cause
   /// `BlockNotDelivered` and all-zero observed/attributed/residual,
   /// AND advance `meta.last_block` / `meta.last_block_hash` to
   /// `(number, hash)`. Atomic per block; crash-safe via the same
   /// `INSERT OR REPLACE` idempotency as `write_block`.
   pub fn write_gap_block_marker(
       conn: &mut Connection,
       chain_id: u64,
       block_number: u64,
       block_hash: alloy_primitives::B256,
       watched: &HashSet<alloy_primitives::Address>,
   ) -> eyre::Result<()>;
   ```
   No new schema. No changes to existing writer functions.

3. **`crates/passbook-core/src/exex.rs::run_passbook`** — three small edits:
   - **State:** declare a local `let mut high_water: Option<u64> =
     head.map(|(n, _)| n);` initialised from the resume-point lookup
     already performed at startup.
   - **Gap detect:** at the top of the `if let Some(chain) =
     notification.committed_chain()` branch (BEFORE the per-block
     loop), if `high_water.map(|hw| chain.first().header().number() >
     hw + 1).unwrap_or(false)`, fill the gap:
     ```rust
     let gap_first = high_water.unwrap() + 1;
     let gap_last  = chain.first().header().number() - 1;
     tracing::warn!(gap_first, gap_last, size = gap_last - gap_first + 1,
                    "gap-on-restart: filling with block_not_delivered markers");
     for n in gap_first..=gap_last {
         let mut backoff = BACKOFF_START;
         loop {
             let bh = match ctx.provider().block_hash(n) {
                 Ok(Some(h)) => h,
                 Ok(None) | Err(_) => {
                     tracing::error!(block = n, "gap-fill: header unavailable, retrying (not advancing)");
                     tokio::time::sleep(backoff).await;
                     backoff = (backoff * 2).min(BACKOFF_CAP);
                     continue;
                 }
             };
             let res = write_gap_block_marker(
                 ledger.lock().unwrap_or_else(|e| e.into_inner()).conn_mut(),
                 chain_id, n, bh, &cfg.watched,
             );
             match res {
                 Ok(()) => { high_water = Some(n); break; }
                 Err(e) => {
                     tracing::error!(error = %e, block = n,
                         "gap-fill: marker write failed, retrying (not advancing)");
                     tokio::time::sleep(backoff).await;
                     backoff = (backoff * 2).min(BACKOFF_CAP);
                 }
             }
         }
     }
     ```
   - **Tracker update:** in the per-block `write_block` success branch
     (the existing `Ok(()) => break` arm) set
     `high_water = Some(block.header().number())` immediately before the
     `break`. The gap-fill branch above already sets `high_water =
     Some(n)` on every successful marker write. The single source of
     truth for `high_water` is "last block number durably written,
     either as a full per-block batch or as a gap marker," mirroring
     `meta.last_block`. The diagnostic `write_unattributed` call in the
     `UnexplainedResidual` stall arm does NOT advance `high_water` (the
     block is intentionally not durable — we are stalled on it).

   No changes to the reorg branch (`delete_blocks` does not advance
   `high_water`), no changes to `FinishedHeight` emission (`lag_finished`
   still fires once per committed-chain notification after gap-fill +
   per-block loop both complete).

4. **Tests (`crates/passbook-core/tests/exex_integration.rs` + unit tests in `exex.rs` / `writer.rs` / `model.rs`):**
   - **Unit (model.rs):** `BlockNotDelivered` round-trips via
     `as_str`/`from_str`; unknown variants still fail.
   - **Unit (writer.rs):** `write_gap_block_marker` writes
     `watched.len()` rows with all-zero amounts, `cause =
     'block_not_delivered'`, and atomically advances
     `meta.last_block`/`last_block_hash`. Idempotent on replay.
     Watched-set empty ⇒ zero rows but `meta` still advances.
   - **Integration:** drive two committed-chain notifications, the
     first ending at block 10, the second starting at block 21 (gap
     11..=20). Assert: ten `block_not_delivered` markers per watched
     address (so `10 * watched.len()` total), `meta.last_block = 30`
     (or wherever the second batch's tip is), one `FinishedHeight`
     emitted per notification.
   - **Integration (no-gap regression):** consecutive notifications
     with `committed.first() == high_water + 1` write zero
     `block_not_delivered` markers (existing path unchanged).
   - **Integration (reorg-redelivery regression):** a reverted +
     re-delivered chain where `committed.first() <= high_water`
     writes zero `block_not_delivered` markers (strict `>` guard).
   - **Integration (header-fetch Err is loud stall):** an injected
     `block_hash_by_number → Err` for the first gap block causes the
     gap-fill loop to retry without advancing; no markers, no
     `FinishedHeight`, an ERROR log line emitted. (Recovery path: the
     same fixture, after the injection is cleared, completes the fill
     and advances.)

## Data / schema

**No schema change.** `unattributed_deltas.cause` is already a free-form
TEXT column (added in v3 via `MIGRATE_V2_TO_V3` for issue #15); the
on-disk spelling `block_not_delivered` is part of the `as_str`/`from_str`
contract and is added in this change. `SCHEMA_VERSION` stays at `"3"`.

## Reorgs

A gap-marker block that later gets reverted in a reorg: the existing
reorg-first delete (`delete_blocks(reverted_hashes)` in `run_passbook`)
removes all rows for that `block_hash`, including the
`block_not_delivered` markers, via the existing `ix_unattr_bh` /
PK-on-block-hash path. No new code.

A reorg whose committed chain re-delivers blocks at or below
`high_water` falls below the strict `>` guard — no gap-fill fires, and
the existing per-block processing path's `INSERT OR REPLACE` writes
re-cover those blocks idempotently. The in-memory `high_water` tracker
walks back to whatever block the per-block loop most recently wrote;
subsequent gap checks remain correct.

## Performance

One DB transaction per gap block (matching `write_block`'s pattern).
For the production wedge that motivated this fix (~1.28 M gap blocks),
that is ~1.28 M transactions × `watched.len()` row inserts plus two
`meta` updates each — on the order of minutes of SQLite work on the
target hardware, with bounded memory. Acceptable as a one-time recovery
cost. Per-block TX is also the simplest crash-safety contract (lose at
most one block to a crash; the next start resumes at the durable
high-water + 1).

Batching multiple gap blocks per transaction (e.g. 1024) would be a
~1000× speedup if the gap fill ever becomes the bottleneck. Out of
scope for this change; trivial follow-up if measured to matter.

## Logging

- One `WARN` line at the start of every gap-fill: `gap-on-restart:
  filling N markers from block X to block Y`.
- One `WARN` line at the end: `gap-on-restart: complete, advanced
  high_water to Y`.
- Per-block ERROR lines only on header-fetch or marker-write Err
  (matching the existing reorg-delete / `write_block` retry sites).
  Successful per-block fill is silent — at gap sizes in the millions
  per-block INFO would flood Loki.

## Out of scope

- Retroactively re-capturing the historical 1.28 M-block gap on the
  current production op-mainnet ledger. The block contents (inspector
  frames, receipts, bundle state) for that range are not in any
  notification stream; recovering attribution would require a separate
  one-shot replay tool that walks the now-Finish-stage-available
  historical state. Markers go in on the *next* gap (if one occurs);
  the historical gap stays uncovered.
- Configurable gap-fill thresholds or operator switches. The failure
  itself is the signal — same principle as the parent-state skip-mode
  design.
- Metrics (`passbook_gap_blocks_filled_total` etc.). Trivial follow-up
  if operators want a queryable counter beyond the `unattributed_deltas`
  `WHERE cause = 'block_not_delivered'` SQL above.
- Driver-side `BackfillJobFactory` integration (issue's Option 1).
  Established above: not viable in the staged-sync window that
  produces the gap; reth already covers the live-mode case.

## Risks / open items

- **Header availability assumption.** The design relies on
  `block_hash_by_number(N)` succeeding for every block in the gap
  range during staged sync. The Headers stage runs before
  Execution/Finish, and Execution-stage notifications by construction
  refer to blocks whose headers are already durable, so this should
  always hold. If a future reth refactor gates `block_hash_by_number`
  behind the Finish checkpoint, the gap-fill stalls loudly (Option 3
  fallback) rather than advancing silently — the safety floor holds.
- **Live-mode false positive.** If a non-staged-sync run ever yields a
  gap (e.g. driver bug, deeply-out-of-order notification source we do
  not yet handle), the gap-fill writes `block_not_delivered` markers
  for blocks whose full processing path *would* have worked. The
  markers themselves are durable and queryable; this is preferable to
  silent advance and the operator can replay the affected blocks via
  the (future, out-of-scope) re-index tool. Live-mode false positives
  are visible as a sudden spike of `cause = 'block_not_delivered'`
  rows post-catch-up, which is itself a fault signal worth surfacing.
- **Concurrent reorg during gap-fill.** A reorg notification arriving
  mid-gap-fill is impossible: `try_next().await` is the only suspension
  point in the loop, and the loop holds the only consumer of the
  notification stream. The gap-fill blocks the loop until it
  completes, by design.

## Deployment

1. Implement the change in passbook (per the writing-plans output).
2. Image rebuild → push (`ghcr.io/ajsutton/op-reth-passbook:<short-sha>`).
3. Bump the big-docker `op-mainnet` and `op-sepolia` image tags.
4. Operator pushes big-docker. Rolling `el` restart, no datadir wipe.
   No backfill or replay; the next gap (whenever it next occurs) is
   filled with markers instead of advancing silently.
