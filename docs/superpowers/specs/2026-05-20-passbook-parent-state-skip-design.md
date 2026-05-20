# Design: graceful parent-state-unavailable handling in the passbook ExEx

**Status:** Approved (brainstormed 2026-05-20).
**Goal:** Unblock the from-Bedrock backfill so it captures every available signal even for watched-account-change blocks the staged-sync provider cannot serve parent state for, instead of wedging the node forever on the first such block.

## Context (why this change exists)

Today's `run_passbook` driver calls the per-block gated re-execution through `ChainExec::process_committed_block(... &get_parent_state)`. When a watched account changes in a block, the seam calls `get_parent_state()` (= `ctx.provider().history_by_block_hash(parent_hash)`) and re-executes the block with a `ValueInspector` to capture native ETH internal-call frames.

During a **from-Bedrock staged-pipeline backfill** on a `--full` node, `history_by_block_hash` rejects the call with `ProviderError::BlockNotExecuted { executed: <Finish-stage block> }`. Per `crates/storage/provider/src/providers/database/provider.rs:1794`: *"The best block number is tracked via the finished stage."* The Finish stage does not advance until the entire staged pipeline reaches tip, so `best_block_number()` is pinned at the bootstrap checkpoint (Bedrock genesis) for the whole multi-week backfill. The error is **structural to staged sync** — not a pruning issue, not fixable by config, archive-independent (re-verified in the pinned reth source 2026-05-20). Confirmed in production: op-mainnet hard-stuck at the first watched-change block 114,804,381 for ~24 h with this exact error.

The current `ProcessingError::ParentStateUnavailable` arm in `run_passbook` retries forever with bounded backoff. That arm worked for transient cases (the Task 10b integration test) but is the wrong policy here: the failure is permanent for this block until the pipeline completes (weeks), and meanwhile the ExEx notification buffer fills → `poll_execute_ready` backpressure halts the Execution stage → the whole node stops syncing.

The accepted strategy (2026-05-20): **continue the from-Bedrock sync** (datadir already ~18 % through), accept the loss of internal-call-frame attribution for the handful of watched-change blocks that fall in the backfill window, and capture everything else (ERC20, gas, system credits, durable diagnostic markers) so the resulting ledger is *almost* complete from Bedrock. Once the node finishes pipeline catch-up and goes live, `Finish` advances per-block, `get_parent_state()` succeeds, and full capture resumes automatically.

## Design principle

> **If the parent state isn't available, skip the inspector frames and capture everything else. Record a durable marker. Advance.**

Pure structural behaviour. **No detection threshold, no config flag, no operator switch.** In normal live operation `get_parent_state()` succeeds (the Finish stage advances per-block); the skip branch is unreachable. In staged backfill it fails; the skip branch is the only sensible response (don't wedge the node). If a "live" call ever fails for a genuinely unexpected reason, skipping is still correct — better to record + advance than to deadlock the pipeline.

## Behaviour

| State | `get_parent_state()` | Outcome |
|---|---|---|
| Live (Finish ≈ tip) | `Ok(state)` | Full frame capture + reconciliation. Current behaviour, unchanged. |
| Staged catch-up (Finish ≪ block) | `Err(BlockNotExecuted)` | Partial batch + unattributed marker, advance. |
| Unexpected fault in live mode | `Err(anything)` | Same skip path (defensive — never wedge the node on parent-state errors). |

The third row is intentional: the failure itself is the signal, not its cause.

## Partial-batch contract (what gets written on a skipped block)

When `get_parent_state()` returns `Err` for a watched-change block, `process_committed_block_inner` (L1) and `OpChainExec::process_committed_block` (OP) construct the `BlockBatch` directly, bypassing `process_block`'s frame-based reconciliation:

| Captured (same as normal) | Skipped |
|---|---|
| ERC20 transfers — from receipts | Native ETH internal-call frames (the inspector-only data) |
| Gas payments — from receipts | Frame-based reconciliation residual check |
| System credits (L1 withdrawals / block reward / OP deposit mints) → `eth_transfers` `kind=system` rows | |
| **`unattributed_deltas` marker** — one row per watched address that changed in this block: `observed_wei = \|bundle_delta\|`, `attributed_wei = 0`, `residual_wei = \|bundle_delta\|` | |

The marker uses the *existing* `unattributed_deltas` schema (currently used for `UnexplainedResidual` diagnostics — same row shape, same semantics: "watched account moved by X, attribution incomplete"). Operators can query the marker to know which backfill blocks had the internal-tx gap.

## Components touched

1. **`crates/passbook-core/src/exex.rs`**
   - `process_committed_block_inner`: replace the `get_parent_state().map_err(... ParentStateUnavailable)?` line with a `match`. On `Err`: skip the `reexecute_block_frames` call, build a partial `BlockBatch` directly (erc20 + gas + system-credit `eth_transfers` rows + `unattributed_deltas` markers from `account_deltas` filtered by `watched`), return `Ok(batch)`. Do not invoke `process_block`'s reconcile loop on the skip path (the residual is by design).
   - `run_passbook`: remove the dedicated `Err(ProcessingError::ParentStateUnavailable { .. }) => { sleep; retry; }` arm — no longer reachable. The `Err(other)` catch-all stays for genuine processing faults.
   - `ProcessingError::ParentStateUnavailable` variant: **remove** (no longer constructed anywhere). Update the Task 1 unit test (`parent_state_unavailable_is_processing_error`) — either delete it or repurpose to assert the variant is gone.
2. **`crates/passbook-stack-optimism/src/op_chain.rs`** — same change to `OpChainExec::process_committed_block`. Includes the OP-specific system-credit + L1 data-fee logic in the partial batch (the OP system credits computation already needs no parent state).
3. **Integration tests (`crates/passbook-core/tests/exex_integration.rs`)**
   - **Replace** Task-10b's `parent_state_unavailable_retries_then_succeeds` with `parent_state_unavailable_writes_partial_batch_and_marker`: drive a watched-change block with a `get_parent_state` thunk that returns `Err`; assert the returned `BlockBatch` contains zero frame-derived `eth_transfers` rows for the watched address, at least one `unattributed_deltas` row per watched-changed address with `observed_wei = |delta|`, the erc20/gas/system rows are populated correctly, and no error is returned.
   - Verify a `Ok(parent_state)` thunk on the same fixture still captures frames (no regression on the live path).

## Data / schema

**No schema change.** `unattributed_deltas` already exists, with the right shape and primary key. Reorg handling is automatic: the existing `delete_blocks(reverted_hashes)` in the reorg path deletes rows by `block_hash` across all tables including `unattributed_deltas`, so markers reorg-cleanly with no new code.

## Reorgs

A skip-marked block that later gets reverted in a reorg: `delete_blocks` removes all rows for that `block_hash` — including the `unattributed_deltas` marker — through the existing reorg-first delete in `run_passbook`. Re-execution of the canonical block flows through the same per-block path; if `get_parent_state` still fails on the canonical block (same backfill region), a new marker is written. If it succeeds (e.g., the node has caught up), full capture occurs. No new code.

## Out of scope

- Retroactively re-capturing skipped backfill blocks once the node is live. Backfill internal-tx loss is accepted; markers tell us which blocks if we ever want to manually investigate.
- Configurable thresholds / detection / operator flags. None; the failure is the signal.
- Disabling the ExEx during catch-up. Unnecessary with this change.
- Switching the bootstrap to a snapshot. Decided against — the from-Bedrock sync (already ~18 % along) continues; the skip mode preserves most of the value during the remaining backfill.

## Deployment

1. Implement the change in passbook (per the writing-plans output).
2. Image rebuild → push (`ghcr.io/ajsutton/op-reth-passbook:<short-sha>`).
3. Bump the big-docker `op-mainnet` and `op-sepolia` image tags. Keep op-mainnet's `--config=/cfg/reth.toml` (small Execution-stage batches): it was the OOM mitigation; the skip mode addresses a different fault (exex stall on parent state) but doesn't change the underlying batch-size memory pressure of running the ExEx, so the conservative thresholds stay.
4. Operator pushes big-docker. Rolling `el` restart, no datadir wipe. op-mainnet's `el` resumes-with-head at the current ledger high-water (114,804,380), hits the previously stuck block 114,804,381, writes the partial batch + marker, advances, catches up to the Execution frontier in minutes, and the from-Bedrock backfill continues.

## Risks / open items

- **None blocking.** The change is mechanically small (one branch in two seam impls, one arm removed in the driver, one variant removed, one test rewritten).
- **Live-mode silent skip if something genuinely breaks parent-state availability.** The third behaviour row (defensive skip in live mode) means a future live-only fault would silently produce markers instead of stalling. Mitigation: the `unattributed_deltas` row is durable and queryable; operators can monitor "unattributed markers post node-live" as a fault signal.
- The Loki ingestion gap for op-mainnet (logs absent since 2026-05-19 14:52) is **separate** and not addressed by this change; once the exex is unblocked and the node logs resume (or Loki/promtail is fixed) we regain visibility.
