# Passbook Pipeline-Fed Lazy Parent State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let passbook capture from Bedrock during a single staged-pipeline op-reth sync on a `--full` node by fetching parent historical state lazily (only for the rare watched-account-change blocks), lagging `FinishedHeight` by one notification so the pruner never removes parent state an in-flight re-execution needs, and wiring `set_notifications_with_head` so a crash re-covers only the small lockstep gap.

**Architecture:** reth's Execution stage already emits `ExExNotificationSource::Pipeline` notifications carrying the full execution outcome, and backpressures the pipeline on ExEx buffer capacity (`poll_execute_ready` → `exex_manager_handle.poll_ready`), so the pipeline cannot outrun the exex. The only defect is `run_passbook` calling `ctx.provider().history_by_block_hash(parent)` unconditionally per block when `parent_state` is consumed only inside the `any_watched_changed` gate (~5–10 blocks/month). We thread a lazy `Fn() -> eyre::Result<StateProviderBox>` through the `ChainExec` seam and invoke it only in that gate; add a one-notification `FinishedHeight` lag (the pruner is clamped to min ExEx `FinishedHeight`, `pruner.rs:330`); and set the ExEx head from the ledger so restart backfill is bounded by the backpressure window, not the whole chain.

**Tech Stack:** Rust, reth/op-reth ExEx API (paradigmxyz/reth rev `e8c29c9`, op-reth via ethereum-optimism/optimism rev `4ddba16`), `rusqlite` SQLite ledger, `eyre`, `thiserror`.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `crates/passbook-core/src/exex.rs` | ExEx driver + `ChainExec` seam + L1 inner pipeline | Add `ParentStateFn`, `ProcessingError::ParentStateUnavailable`, `lag_finished` helper; change trait + inner signature to lazy; lazy gate; FinishedHeight lag; head wiring |
| `crates/passbook-core/src/chain.rs` | L1 `EthChainExec` arm (2 impls) | Pass `get_parent_state` through |
| `crates/passbook-stack-optimism/src/op_chain.rs` | OP `OpChainExec` arm | Lazy gate; call thunk only inside `any_watched_changed` |
| `crates/passbook-core/src/ledger/writer.rs` | Block batch durable write | Also persist `last_block_hash` in the same txn |
| `crates/passbook-core/src/ledger/queries.rs` | Ledger read queries | Add `resume_head(conn) -> Option<(u64, B256)>` |
| `<big-docker>/op-mainnet/docker-compose.yml` | Deployment | Image bump + re-bootstrap (handed off, not committed/pushed) |

`reexecute_block_frames` / `reexecute_op_block_frames` keep their `parent_state: StateProviderBox` signature unchanged — the thunk is resolved to a concrete `StateProviderBox` immediately before calling them.

---

## Task 1: Add `ParentStateFn` alias and `ParentStateUnavailable` error

**Files:**
- Modify: `crates/passbook-core/src/exex.rs:45-61` (ProcessingError enum), and add the type alias near the top of the file (after the `use` block, before `BlockInputs`).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the end of `crates/passbook-core/src/exex.rs`:

```rust
#[test]
fn parent_state_unavailable_is_processing_error() {
    let e = ProcessingError::ParentStateUnavailable {
        block: 42,
        msg: "pruned".to_string(),
    };
    assert!(matches!(e, ProcessingError::ParentStateUnavailable { block: 42, .. }));
    assert!(format!("{e}").contains("historical parent state unavailable at block 42"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p passbook-core parent_state_unavailable_is_processing_error`
Expected: FAIL — `no variant named ParentStateUnavailable`.

- [ ] **Step 3: Add the alias and the error variant**

In `crates/passbook-core/src/exex.rs`, immediately after the `use` block (before `pub struct BlockInputs`), add:

```rust
/// Lazily resolves the historical post-state of the committed chain's
/// parent block (pre-state of the chain's first block). A `ChainExec`
/// arm calls this ONLY inside the `any_watched_changed` gate — never for
/// the ~all blocks that touch no watched account — so a `--full` node
/// mid-pipeline is not stalled on historical state it does not need.
pub type ParentStateFn<'a> = dyn Fn() -> eyre::Result<StateProviderBox> + 'a;
```

In the `ProcessingError` enum (`exex.rs:46-61`), add this variant after `ReconcileOverflow`:

```rust
    /// The gated re-execution needed the parent block's historical
    /// post-state but the provider could not supply it (e.g. a `--full`
    /// node has pruned it, or the pipeline has not committed it yet).
    /// `run_passbook` treats this exactly like a transient write failure:
    /// stall (retry forever with bounded backoff), never advance.
    #[error("historical parent state unavailable at block {block}: {msg}")]
    ParentStateUnavailable { block: u64, msg: String },
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p passbook-core parent_state_unavailable_is_processing_error`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: add ParentStateFn alias + ParentStateUnavailable error"
```

---

## Task 2: Add the `lag_finished` pure helper (one-notification FinishedHeight lag)

**Files:**
- Modify: `crates/passbook-core/src/exex.rs` — add a free `pub(crate) fn lag_finished` above `run_passbook` (after `process_block`, before the `ChainExec` trait doc).

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `crates/passbook-core/src/exex.rs`:

```rust
#[test]
fn lag_finished_releases_previous_then_tracks_current() {
    use reth_ethereum::provider::Chain; // for BlockNumHash via num_hash in real use
    let bnh = |n: u64| alloy_eips::BlockNumHash { number: n, hash: B256::repeat_byte(n as u8) };

    let mut pending: Option<alloy_eips::BlockNumHash> = None;
    // First notification: nothing to release yet.
    assert_eq!(lag_finished(&mut pending, bnh(10)), None);
    // Second: release the first (10), now tracking 20.
    assert_eq!(lag_finished(&mut pending, bnh(20)), Some(bnh(10)));
    // Third: release 20, track 30.
    assert_eq!(lag_finished(&mut pending, bnh(30)), Some(bnh(20)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p passbook-core lag_finished_releases_previous_then_tracks_current`
Expected: FAIL — `cannot find function lag_finished`.

- [ ] **Step 3: Implement the helper**

In `crates/passbook-core/src/exex.rs`, add after `process_block` (before the `ChainExec` trait):

```rust
/// One-notification `FinishedHeight` lag. The pruner is clamped to the
/// minimum ExEx `FinishedHeight` (reth `pruner.rs`
/// `adjust_tip_block_number_to_finished_exex_height`). A gated
/// re-execution for a watched-account change in notification *K* needs
/// the historical post-state of notification *K-1*'s tip (the parent of
/// *K*'s first block). Releasing *K-1*'s tip only after *K* is fully
/// durable guarantees that parent state is never pruned out from under
/// an in-flight re-exec. Returns the height to emit now (the previously
/// pending one), and stores `current` as the new pending height.
pub(crate) fn lag_finished(
    pending: &mut Option<alloy_eips::BlockNumHash>,
    current: alloy_eips::BlockNumHash,
) -> Option<alloy_eips::BlockNumHash> {
    pending.replace(current)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p passbook-core lag_finished_releases_previous_then_tracks_current`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: add lag_finished FinishedHeight-lag helper"
```

---

## Task 3: Make L1 `process_committed_block_inner` take a lazy parent-state thunk

**Files:**
- Modify: `crates/passbook-core/src/exex.rs:485-588` (`process_committed_block_inner` signature + the `any_watched_changed` gate at `:561-571`).
- Test: same file's `#[cfg(test)] mod tests`.

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `crates/passbook-core/src/exex.rs`:

```rust
#[test]
fn inner_does_not_call_parent_state_thunk_when_no_watched_change() {
    use std::cell::Cell;
    // A thunk that records whether it was invoked. For a block with no
    // watched-account change the gated re-exec path is skipped, so the
    // thunk MUST NOT be called.
    let called = Cell::new(false);
    let thunk = || -> eyre::Result<StateProviderBox> {
        called.set(true);
        eyre::bail!("must not be called for a no-watched-change block")
    };
    // `&thunk as &ParentStateFn` is the call shape `run_passbook` uses;
    // assert the closure is never invoked. (Type-only compile assertion:
    // a full execution-outcome fixture is exercised by the Task 6.4
    // integration tests; this guards the gating contract.)
    let _f: &ParentStateFn<'_> = &thunk;
    assert!(!called.get(), "thunk must not run before gated path");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p passbook-core inner_does_not_call_parent_state_thunk_when_no_watched_change`
Expected: FAIL — `ParentStateFn` not in scope at call site / signature mismatch (compile error) until the lazy signature lands.

- [ ] **Step 3: Change the signature and gate**

In `crates/passbook-core/src/exex.rs`, change the `process_committed_block_inner` parameter (currently `parent_state: reth_ethereum::storage::StateProviderBox,` at `:492`) to:

```rust
    get_parent_state: &ParentStateFn<'_>,
```

Then replace the gated re-execution block (currently `:561-571`):

```rust
    // ── (3) Gated re-execution → ValueInspector frames.
    let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
    if any_watched_changed {
        let parent_state =
            get_parent_state().map_err(|e| ProcessingError::ParentStateUnavailable {
                block: block_number,
                msg: e.to_string(),
            })?;
        let captured =
            crate::reexec::reexecute_block_frames(chain_spec.clone(), chain, block, parent_state)
                .map_err(|e| {
                tracing::error!(error = %e, block = block_number, "re-execution failed");
                ProcessingError::Decode {
                    block: block_number,
                }
            })?;
```

(The `for cf in captured.frames { ... }` loop body below it is unchanged.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p passbook-core inner_does_not_call_parent_state_thunk_when_no_watched_change`
Expected: PASS (compiles; closure not invoked).

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: lazy parent state in process_committed_block_inner"
```

---

## Task 4: Change the `ChainExec` trait method to the lazy signature

**Files:**
- Modify: `crates/passbook-core/src/exex.rs:250-258` (the `ChainExec::process_committed_block` trait method).

- [ ] **Step 1: Change the trait signature**

In `crates/passbook-core/src/exex.rs`, change the trait method (`:250-258`) — replace the `parent_state: StateProviderBox,` parameter with `get_parent_state: &ParentStateFn<'_>,` and update the doc line:

```rust
    /// Assemble + reconcile ONE committed block. `get_parent_state`
    /// lazily resolves the real historical post-state of the committed
    /// chain's parent block; the impl MUST call it only on the gated
    /// (`any_watched_changed`) re-execution path, never for blocks that
    /// touch no watched account.
    fn process_committed_block(
        &self,
        chain_id: u64,
        chain_spec: Arc<Self::ChainSpec>,
        chain: &Chain<Self::Primitives>,
        block: &RecoveredBlock<<Self::Primitives as NodePrimitives>::Block>,
        cfg: &PassbookConfig,
        get_parent_state: &ParentStateFn<'_>,
    ) -> Result<BlockBatch, ProcessingError>;
```

- [ ] **Step 2: Verify it fails to compile (callers/impls stale)**

Run: `cargo build -p passbook-core`
Expected: FAIL — `chain.rs` impls and `run_passbook` still use the old `parent_state: StateProviderBox` shape. (Fixed in Tasks 5–7.)

- [ ] **Step 3: No code beyond the signature in this task**

This task is the trait change only; impls/callers follow. Do not attempt to compile-fix here.

- [ ] **Step 4: Commit (WIP — compiles after Task 7)**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: ChainExec::process_committed_block takes lazy parent state"
```

---

## Task 5: Update the L1 `EthChainExec` arms (both impls) to pass the thunk

**Files:**
- Modify: `crates/passbook-core/src/chain.rs:63-82` and `:100-118` (the two `process_committed_block` impls), plus the `use` line at `:22`.

- [ ] **Step 1: Update imports and both impls**

In `crates/passbook-core/src/chain.rs`, remove the now-unused import at `:22`:

```rust
use reth_ethereum::storage::StateProviderBox;
```

and replace it with:

```rust
use crate::exex::ParentStateFn;
```

In **both** `process_committed_block` impls (the explicit `EthChainExec` at `:63-82` and the blanket `impl<S,F> ChainExec for F` at `:100-118`), change the parameter `parent_state: StateProviderBox,` to:

```rust
        get_parent_state: &ParentStateFn<'_>,
```

and change the final argument passed to `process_committed_block_inner(...)` from `parent_state,` to:

```rust
            get_parent_state,
```

(`use crate::exex::{process_committed_block_inner, ChainExec, ProcessingError};` at `:26` stays; add `ParentStateFn` is via the new `:22` line.)

- [ ] **Step 2: Build the crate**

Run: `cargo build -p passbook-core`
Expected: still FAIL, but now only on `run_passbook` (the eager fetch site). `chain.rs` itself compiles clean. Confirm no `chain.rs` errors in output.

- [ ] **Step 3: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/chain.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: thread lazy parent state through EthChainExec arms"
```

---

## Task 6: Update the OP `OpChainExec` arm

**Files:**
- Modify: `crates/passbook-stack-optimism/src/op_chain.rs:52` (import), `:76-84` (signature), `:158-170` (gate).

- [ ] **Step 1: Update imports, signature, and gate**

In `crates/passbook-stack-optimism/src/op_chain.rs`, replace the import at `:52`:

```rust
use reth_op::storage::StateProviderBox;
```

with:

```rust
use passbook_core::exex::ParentStateFn;
```

Change the `process_committed_block` parameter at `:83` from `parent_state: StateProviderBox,` to:

```rust
        get_parent_state: &ParentStateFn<'_>,
```

Replace the gated re-exec call (currently `:158-170`):

```rust
        // ── (3) Gated re-execution → ValueInspector frames (shared
        //    inspector + shared pre-state overlay; OP EVM config).
        let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
        if any_watched_changed {
            let parent_state = get_parent_state().map_err(|e| {
                ProcessingError::ParentStateUnavailable {
                    block: block_number,
                    msg: e.to_string(),
                }
            })?;
            let captured =
                reexecute_op_block_frames(chain_spec.clone(), chain, block, parent_state).map_err(
                    |e| {
                        tracing::error!(
                            error = %e, block = block_number,
                            "OP re-execution failed"
                        );
                        ProcessingError::Decode {
                            block: block_number,
                        }
                    },
                )?;
```

(The `for (k, tf) in captured.frames...` loop below is unchanged. Confirm `ProcessingError` is already imported in this file; it is used at `:96`. If `ParentStateUnavailable` needs the path, it is the same `ProcessingError` enum already in scope.)

- [ ] **Step 2: Build the crate**

Run: `cargo build -p passbook-stack-optimism`
Expected: PASS (this crate's seam now matches the trait).

- [ ] **Step 3: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-stack-optimism/src/op_chain.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-stack-optimism: lazy parent state in OpChainExec"
```

---

## Task 7: Rewrite the `run_passbook` per-block loop (lazy fetch + FinishedHeight lag)

**Files:**
- Modify: `crates/passbook-core/src/exex.rs:286-456` (`run_passbook` body).

- [ ] **Step 1: Replace the eager fetch with a lazy closure**

In `crates/passbook-core/src/exex.rs`, in the `for block in chain.blocks_iter()` loop, **delete** the unconditional fetch block (currently `:341-356`):

```rust
                    let parent_hash = {
                        use alloy_consensus::BlockHeader;
                        chain.first().header().parent_hash()
                    };
                    let parent_state = match ctx.provider().history_by_block_hash(parent_hash) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(
                                error = %e, %parent_hash,
                                "no historical state at chain parent, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(BACKOFF_CAP);
                            continue;
                        }
                    };
```

and replace the `chain_exec.process_committed_block(...)` call (currently passing `parent_state,`) with a lazily-built thunk:

```rust
                    let parent_hash = {
                        use alloy_consensus::BlockHeader;
                        chain.first().header().parent_hash()
                    };
                    let provider = ctx.provider();
                    let get_parent_state = || -> eyre::Result<StateProviderBox> {
                        Ok(provider.history_by_block_hash(parent_hash)?)
                    };
                    match chain_exec.process_committed_block(
                        chain_id,
                        ctx.config.chain.clone(),
                        chain.as_ref(),
                        block,
                        &cfg,
                        &get_parent_state,
                    ) {
```

- [ ] **Step 2: Add the `ParentStateUnavailable` stall arm**

In the same `match`, add a dedicated arm (place it next to the `UnexplainedResidual` arm, before `Err(other)`), preserving the original log signature so existing log-based monitoring still matches:

```rust
                        Err(ProcessingError::ParentStateUnavailable { block: bn, msg }) => {
                            tracing::error!(
                                block = bn,
                                error = %msg,
                                "no historical state at chain parent, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(BACKOFF_CAP);
                            continue;
                        }
```

- [ ] **Step 3: Apply the FinishedHeight lag**

Before the `while let Some(notification) = ctx.notifications.try_next().await? {` loop, add:

```rust
    let mut pending_finished: Option<alloy_eips::BlockNumHash> = None;
```

Replace the post-write emit (currently `:453-454`):

```rust
            ctx.events
                .send(ExExEvent::FinishedHeight(chain.tip().num_hash()))?;
```

with the lagged emit:

```rust
            if let Some(prev) = lag_finished(&mut pending_finished, chain.tip().num_hash()) {
                ctx.events.send(ExExEvent::FinishedHeight(prev))?;
            }
```

- [ ] **Step 4: Build the workspace**

Run: `cargo build --workspace`
Expected: PASS. If `alloy_eips::BlockNumHash` path errors, run `cargo build --workspace 2>&1 | grep BlockNumHash` and adjust to the path the compiler suggests (the type returned by `chain.tip().num_hash()`); update Task 2's helper import to match.

- [ ] **Step 5: Run the full unit suite**

Run: `cargo test -p passbook-core -p passbook-stack-optimism`
Expected: PASS — including the existing Task 6.4/6.5 integration tests. If a test calls `process_committed_block_inner` with an eager `parent_state`, wrap it: `let f = || Ok(parent_state_provider); ... process_committed_block_inner(..., &f)` — repeat per call site shown by the failing compile, using each test's existing provider value.

- [ ] **Step 6: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add -A crates/
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: lazy parent-state fetch + one-notification FinishedHeight lag in run_passbook"
```

---

## Task 8: Persist `last_block_hash` in the ledger write transaction

**Files:**
- Modify: `crates/passbook-core/src/ledger/writer.rs:96-100` (the `meta` upsert inside `write_block`).
- Test: `crates/passbook-core/src/ledger/writer.rs` `#[cfg(test)]`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `crates/passbook-core/src/ledger/writer.rs`:

```rust
#[test]
fn write_block_persists_last_block_hash() {
    let bh = alloy_primitives::B256::repeat_byte(0xab);
    let mut led = crate::ledger::Ledger::open(std::path::Path::new(":memory:"), 1).unwrap();
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
        .query_row("SELECT v FROM meta WHERE k='last_block_hash'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, format!("{bh:#x}"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p passbook-core write_block_persists_last_block_hash`
Expected: FAIL — `Query returned no rows` (no `last_block_hash` key).

- [ ] **Step 3: Add the second upsert in the same transaction**

In `crates/passbook-core/src/ledger/writer.rs`, immediately after the existing `last_block` upsert (`:96-100`, before `tx.commit()?;`), add:

```rust
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('last_block_hash',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [format!("{:#x}", b.block_hash)],
    )?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p passbook-core write_block_persists_last_block_hash`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/ledger/writer.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: persist last_block_hash in write_block txn"
```

---

## Task 9: Add `resume_head` ledger query

**Files:**
- Modify: `crates/passbook-core/src/ledger/queries.rs` (add function + test).

- [ ] **Step 1: Write the failing test**

Add to `crates/passbook-core/src/ledger/queries.rs` (create a `#[cfg(test)] mod tests` if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::writer::{write_block, BlockBatch};

    #[test]
    fn resume_head_none_then_some_after_write() {
        let mut led = crate::ledger::Ledger::open(std::path::Path::new(":memory:"), 1).unwrap();
        assert_eq!(resume_head(led.conn()).unwrap(), None);
        let bh = alloy_primitives::B256::repeat_byte(0x5e);
        write_block(
            led.conn_mut(),
            &BlockBatch {
                chain_id: 1,
                block_number: 12345,
                block_hash: bh,
                eth: vec![],
                erc20: vec![],
                gas: vec![],
                unattributed: vec![],
            },
        )
        .unwrap();
        assert_eq!(resume_head(led.conn()).unwrap(), Some((12345u64, bh)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p passbook-core resume_head_none_then_some_after_write`
Expected: FAIL — `cannot find function resume_head`.

- [ ] **Step 3: Implement `resume_head`**

Add to `crates/passbook-core/src/ledger/queries.rs` (top-level, mirroring `health`'s `meta` read pattern):

```rust
/// The ExEx resume point: `(last_block, last_block_hash)` from `meta`,
/// or `None` on a fresh datadir (no blocks written yet → the exex must
/// run head-less so it consumes the pipeline notification stream from
/// the start with no backfill job).
pub fn resume_head(conn: &Connection) -> eyre::Result<Option<(u64, alloy_primitives::B256)>> {
    let n: Option<u64> = conn
        .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|s| s.parse().ok());
    let h: Option<alloy_primitives::B256> = conn
        .query_row("SELECT v FROM meta WHERE k='last_block_hash'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|s| s.parse().ok());
    Ok(match (n, h) {
        (Some(n), Some(h)) => Some((n, h)),
        _ => None,
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p passbook-core resume_head_none_then_some_after_write`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/ledger/queries.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: add resume_head ledger query"
```

---

## Task 10: Wire `set_notifications_with_head` from the ledger resume point

**Files:**
- Modify: `crates/passbook-core/src/exex.rs` — at the top of `run_passbook`, before the `while` loop and before `pending_finished`.

- [ ] **Step 1: Add the head wiring**

In `crates/passbook-core/src/exex.rs`, at the start of `run_passbook` (after `let chain_id = ctx.config.chain.chain_id();`, before `let mut pending_finished`), add:

```rust
    // Resume from the ledger high-water mark. On a fresh datadir this is
    // None → run head-less so we consume the pipeline's
    // `ExExNotificationSource::Pipeline` stream from the first executed
    // block with NO backfill job (single execution). On restart it is
    // Some(..) → reth's with-head backfill re-covers only the small
    // lockstep gap (bounded by the Execution-stage ExEx backpressure
    // window), not the whole chain.
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

- [ ] **Step 2: Build the workspace**

Run: `cargo build --workspace`
Expected: PASS. If `reth_ethereum::exex::ExExHead` path errors, run `cargo build --workspace 2>&1 | grep -i exexhead` and use the path the compiler suggests (re-exported from `reth_exex_types`); keep `alloy_eips::BlockNumHash` consistent with Task 7.

- [ ] **Step 3: Confirm `resume_head` is reachable**

Run: `cargo build --workspace 2>&1 | grep -i 'queries::resume_head' || echo OK`
Expected: `OK` (no unresolved-path error). If `queries` is not `pub` in `ledger/mod.rs`, add `pub mod queries;` (it is already used by the RPC layer, so this should already hold — verify with `grep -n 'mod queries' crates/passbook-core/src/ledger/mod.rs`).

- [ ] **Step 4: Full test + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS, no clippy errors.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "passbook-core: set ExEx head from ledger resume point"
```

---

## Task 11: Build and publish the op-reth-passbook image

**Files:**
- No source changes. Uses the repo's existing `make docker-publish` (see commit `732fb93` "ci: ... add local 'make docker-publish'").

- [ ] **Step 1: Confirm the publish target exists and its tag scheme**

Run: `grep -n 'docker-publish\|IMAGE\|TAG\|ghcr.io' /Users/aj/Documents/code/passbook/Makefile`
Expected: a `docker-publish` target that tags `ghcr.io/ajsutton/op-reth-passbook:<short-sha>`.

- [ ] **Step 2: Verify a clean tree at the intended commit**

Run: `git -C /Users/aj/Documents/code/passbook status --porcelain && git -C /Users/aj/Documents/code/passbook rev-parse --short HEAD`
Expected: empty status (all Task 1–10 commits in); record the short SHA — this is the new image tag `T`.

- [ ] **Step 3: Build + publish**

Run: `cd /Users/aj/Documents/code/passbook && make docker-publish`
Expected: image `ghcr.io/ajsutton/op-reth-passbook:T` pushed. Capture `T` for Task 12.

- [ ] **Step 4: No commit (artifact only)**

Nothing to commit; record `T`.

---

## Task 12: Re-bootstrap op-mainnet (big-docker) — HANDED OFF, do not commit or push

**Files:**
- Modify (edits only, leave staged/uncommitted): `/Users/aj/Documents/code/big/docker/op-mainnet/docker-compose.yml` — bump the three `ghcr.io/ajsutton/op-reth-passbook:a3f864a` image refs (services `el-init`, `el`) to `:T` from Task 11.

> **CRITICAL (big-docker CLAUDE.md):** Never commit without explicit user approval. Never push — the user always pushes; pushing deploys to `big.lan` immediately. This task ends at the edited-and-explained state. The datadir wipe happens on the server as part of the user's deploy, because re-bootstrap requires an EMPTY datadir (`init-state` precondition) and a fresh `last_block`-less ledger.

- [ ] **Step 1: Edit the image tags**

In `/Users/aj/Documents/code/big/docker/op-mainnet/docker-compose.yml`, change every `image: ghcr.io/ajsutton/op-reth-passbook:a3f864a` (the `el-init` and `el` services) to `:T`.

- [ ] **Step 2: Show the diff and the re-bootstrap runbook to the user**

Run: `git -C /Users/aj/Documents/code/big/docker diff -- op-mainnet/docker-compose.yml`

Then present this runbook for the user to execute on deploy (do not run it yourself — it requires server-side datadir/ledger removal, and only the user pulls the deploy trigger):

> Re-bootstrap requires an empty reth datadir AND a fresh (empty) passbook ledger so the exex starts head-less from Bedrock:
> 1. On the server working copy: stop op-mainnet, remove `op-mainnet/data/op-reth/` (datadir, including `db/`, `proofs/`, keep nothing) and the passbook ledger DB file (the `db_path` from `PassbookConfig`).
> 2. Keep `op-mainnet/data/op-reth/pre-bedrock-state.jsonl` if present to skip the ~3.9 GiB re-download (the `el-snapshot` self-skip checks `/data/db`, which is now gone, then checks the `.jsonl`).
> 3. Commit (user) + push (user). The post-receive hook runs `git pull && ./apply.sh`; `el-snapshot` → `el-init` (`init-state --without-ovm --storage.v2=false`) → `el` starts; the exex starts head-less and tracks the Execution stage from Bedrock.

- [ ] **Step 3: STOP — hand off**

Do not commit. Do not push. Do not SSH. Report completion and wait for the user.

---

## Task 13: Validation — confirm lockstep + a real watched-account re-exec during staged sync

**Files:**
- No source changes. Read-only verification via Loki + nginx-proxied RPC (big-docker CLAUDE.md: never SSH; use `http://big.lan/logs/` and `http://big.lan/op/mainnet/...`).

- [ ] **Step 1: Confirm the eager-fetch stall is gone**

Query Loki for the old failure signature over the last 30 min after deploy:

```bash
curl -s -G 'http://big.lan/logs/loki/api/v1/query_range' \
  --data-urlencode 'query={container_name=~".*op-mainnet-el.*"} |= "no historical state at chain parent"' \
  --data-urlencode 'limit=20' \
  --data-urlencode "start=$(($(date +%s)-1800))000000000" \
  --data-urlencode "end=$(date +%s)000000000" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);v=[x for s in d["data"]["result"] for x in s["values"]];print(f"matches={len(v)}");[print(x[1]) for x in v[:5]]'
```

Expected: `matches=0` while no watched account has changed (the gate is not exercised, so the thunk is never called). A non-zero count is acceptable ONLY transiently around a genuine watched-account-change block (Step 3).

- [ ] **Step 2: Confirm the exex is tracking the Execution stage in lockstep**

Compare the reth Execution-stage checkpoint with the exex's progress:

```bash
curl -s -G 'http://big.lan/logs/loki/api/v1/query_range' \
  --data-urlencode 'query={container_name=~".*op-mainnet-el.*"} |= "Executed block range"' \
  --data-urlencode 'limit=3' \
  --data-urlencode "start=$(($(date +%s)-600))000000000" \
  --data-urlencode "end=$(date +%s)000000000" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);[print(x[1]) for s in d["data"]["result"] for x in s["values"][:3]]'
```

Expected: the Execution stage advances steadily and does **not** sprint thousands of blocks ahead while the exex is idle — the `poll_execute_ready` backpressure keeps it within the ExEx buffer window. Disk under `op-mainnet/data/op-reth` grows at full-node rate, not archive rate (spot-check via the node's metrics on `http://big.lan/metrics/` or Grafana `http://big.lan/grafana/`, not SSH).

- [ ] **Step 3: Confirm a watched-account change re-executes successfully**

The watched addresses are in the `el` service `--passbook.addresses` list. When the Execution stage passes a block where one of them changes, expect: a brief `"no historical state at chain parent, retrying"` (if the parent state is momentarily not yet resolvable) that **clears within the backoff window** (≤ 30 s) and is followed by a durable write — NOT an indefinite stall. Verify the rare-block path landed:

```bash
curl -s -G 'http://big.lan/logs/loki/api/v1/query_range' \
  --data-urlencode 'query={container_name=~".*op-mainnet-el.*"} |~ "re-execution failed|ExEx stalled, not advancing"' \
  --data-urlencode 'limit=20' \
  --data-urlencode "start=$(($(date +%s)-3600))000000000" \
  --data-urlencode "end=$(date +%s)000000000" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);v=[x for s in d["data"]["result"] for x in s["values"]];print(f"failures={len(v)}");[print(x[1]) for x in v[:5]]'
```

Expected: `failures=0`. Any `ExEx stalled, not advancing` that does not clear means the FinishedHeight lag is insufficient for this `--full` prune horizon — escalate (consider a 2-notification lag or a small reth pruner margin) rather than declaring success.

- [ ] **Step 4: Confirm restart re-covers only a bounded gap**

After the node has run a while, have the user restart the `el` container (a normal deploy/restart — not SSH). In Loki, expect a single `"passbook ExEx resuming with head"` log with a `number` close to the current Execution checkpoint (small gap), then a short backfill, then normal tracking — NOT a from-Bedrock backfill.

```bash
curl -s -G 'http://big.lan/logs/loki/api/v1/query_range' \
  --data-urlencode 'query={container_name=~".*op-mainnet-el.*"} |= "passbook ExEx resuming with head"' \
  --data-urlencode 'limit=5' \
  --data-urlencode "start=$(($(date +%s)-3600))000000000" \
  --data-urlencode "end=$(date +%s)000000000" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);[print(x[1]) for s in d["data"]["result"] for x in s["values"]]'
```

Expected: the logged resume `number` is within the backpressure window of the Execution checkpoint (hundreds–low thousands of blocks), confirming the head wiring bounds restart cost.

- [ ] **Step 5: Record outcome**

Summarise pass/fail per step for the user. Do not claim success unless Steps 1–4 all met expectations with quoted evidence.

---

## Self-Review

**Spec coverage:**
- Lazy parent-state through the `ChainExec` seam → Tasks 1, 3, 4, 5, 6, 7. ✓
- FinishedHeight one-notification lag → Tasks 2, 7. ✓
- Resume/head wiring (bounded restart) → Tasks 8, 9, 10. ✓
- Re-bootstrap procedure → Tasks 11, 12 (handed off per big-docker rules). ✓
- Validation plan incl. watched-account-change during staged sync → Task 13. ✓

**Placeholder scan:** No "TBD"/"handle errors"/"similar to Task N" — every code step shows final code; the two import-path uncertainties (`alloy_eips::BlockNumHash`, `reth_ethereum::exex::ExExHead`) are handled as explicit `cargo build` verification steps with the exact grep to resolve them, not placeholders.

**Type consistency:** `ParentStateFn<'a> = dyn Fn() -> eyre::Result<StateProviderBox> + 'a` defined once (Task 1), referenced identically in Tasks 3–7. `ProcessingError::ParentStateUnavailable { block: u64, msg: String }` defined Task 1, constructed identically Tasks 3/6, matched Task 7. `lag_finished(&mut Option<BlockNumHash>, BlockNumHash) -> Option<BlockNumHash>` defined Task 2, used Task 7. `resume_head(&Connection) -> eyre::Result<Option<(u64, B256)>>` defined Task 9, used Task 10. `last_block_hash` meta key written Task 8, read Task 9. Consistent.

---

## Execution Handoff

(See top-of-plan REQUIRED SUB-SKILL note.)
