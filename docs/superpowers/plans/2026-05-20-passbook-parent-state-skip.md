# Passbook ExEx parent-state-unavailable skip-mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unblock the from-Bedrock pipeline backfill by making the ExEx gracefully skip the inspector frame capture (write a partial batch + `unattributed_deltas` marker, advance) when `get_parent_state()` fails, instead of stalling forever.

**Architecture:** Add one helper `build_partial_batch_skip_frames(BlockInputs) -> BlockBatch` in `passbook-core::exex` that constructs a `BlockBatch` from the already-gathered erc20 logs / system credits / gas / `account_deltas` *without* frames or frame-based reconciliation, instead emitting `unattributed_deltas` markers for watched-changed accounts. Both `process_committed_block_inner` (L1) and `OpChainExec::process_committed_block` (OP) catch `get_parent_state()` `Err` inside the `any_watched_changed` gate and route to this helper (with `frames: Vec::new()`) instead of returning `ProcessingError::ParentStateUnavailable`. The dedicated `Err(ParentStateUnavailable)` arm in `run_passbook` is removed (unreachable), and the variant itself is removed. The existing `unattributed_deltas` schema is reused as-is (markers reorg-clean via the existing `delete_blocks(by block_hash)` path).

**Tech Stack:** Rust, passbook-core/passbook-stack-optimism crates, op-reth ExEx, `cargo test`, `cargo clippy`. Spec: `/Users/aj/Documents/code/passbook/docs/superpowers/specs/2026-05-20-passbook-parent-state-skip-design.md` (commit `6f9b212`).

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `crates/passbook-core/src/exex.rs` | ExEx driver + L1 inner pipeline + types | Add `build_partial_batch_skip_frames` helper; switch L1 inner to use it on `Err`; remove `Err(ParentStateUnavailable)` arm from `run_passbook`; remove the `ParentStateUnavailable` variant; remove the unit test that referenced it |
| `crates/passbook-stack-optimism/src/op_chain.rs` | OP per-block seam | Mirror the same Err handling in `OpChainExec::process_committed_block` (route to the shared helper on `Err`) |
| `crates/passbook-core/tests/exex_integration.rs` | Integration tests | Replace `parent_state_unavailable_retries_then_succeeds` with `parent_state_unavailable_writes_partial_batch_and_marker`; ensure the parent_state=`Ok` happy path is still asserted (no frame-capture regression) |

No schema change. No new dependencies. Reuses existing `BlockInputs`, `BlockBatch`, `UnattributedDeltaRow`, `EthTransferRow`/`EthKind::System`, `Erc20TransferRow`, `GasPaymentRow`, `Direction`, `SystemCredit`, `decode_transfer`. Live-path code (`process_block`) is **not modified** — the skip helper duplicates ~25 lines of erc20-decode + system-credit-rows construction by design, to keep the live path untouched and the skip path independently testable.

---

## Task 1 — Add `build_partial_batch_skip_frames` helper (no caller yet)

Pure addition: define the helper and unit-test it in isolation. No existing code changes; no callers wired yet. This locks down the helper's contract before any seam refactor.

**Files:**
- Modify: `crates/passbook-core/src/exex.rs` (add `pub fn build_partial_batch_skip_frames`; add unit test in the existing `#[cfg(test)] mod tests` block)

- [ ] **Step 1: Write the failing unit test**

Add this test to the `#[cfg(test)] mod tests` block at the end of `crates/passbook-core/src/exex.rs` (place after an existing complete test so no `///` doc-comment is misattached):

```rust
/// Skip-mode partial batch: build_partial_batch_skip_frames must
/// produce a batch with the erc20/gas/system rows still captured, NO
/// frame-derived eth_transfers rows, and one unattributed_deltas
/// marker per watched address with a non-zero delta. No reconciliation,
/// no error path.
#[test]
fn build_partial_batch_skip_frames_emits_markers_and_keeps_non_frame_rows() {
    use crate::erc20::{RawLog, TRANSFER_TOPIC0};
    use crate::system::SystemCredit;
    let w = Address::repeat_byte(0xcc);
    let from = Address::repeat_byte(0xff);
    let token = Address::repeat_byte(0x99);
    let topic_addr = |a: Address| {
        let mut b = [0u8; 32];
        b[12..].copy_from_slice(a.as_slice());
        B256::from(b)
    };
    let log = RawLog {
        address: token,
        topics: vec![TRANSFER_TOPIC0, topic_addr(from), topic_addr(w)],
        data: U256::from(500u64).to_be_bytes::<32>().to_vec().into(),
    };
    let gas_row = GasPaymentRow {
        chain_id: 1,
        block_number: 7,
        block_hash: B256::repeat_byte(0xaa),
        tx_hash: B256::repeat_byte(0x44),
        tx_from: w,
        gas_used: 21_000,
        effective_gas_price: 1,
        l1_fee_wei: U256::ZERO,
        total_wei: U256::from(21_000u64),
    };
    let inp = BlockInputs {
        chain_id: 1,
        block_number: 7,
        block_hash: B256::repeat_byte(0xaa),
        watched: [w].into_iter().collect(),
        erc20_logs: vec![(Some(B256::repeat_byte(0x44)), 0, log)],
        // skip path: frames MUST be empty; helper builds the partial batch
        frames: vec![],
        gas: vec![gas_row.clone()],
        // watched address observed +500 in BundleState (the inflow we
        // can't trace without frames).
        account_deltas: vec![(w, 500i128)],
        // a recognised system credit to the watched address — should
        // still emit a kind=system eth_transfers row.
        system_signed: vec![SystemCredit::new(w, 1, from, "test".to_string())],
    };

    let batch = build_partial_batch_skip_frames(inp);

    // erc20 row preserved (token transfer FROM -> watched)
    assert_eq!(batch.erc20.len(), 1, "erc20 row preserved on skip path");
    assert_eq!(batch.erc20[0].address, w);
    assert!(matches!(batch.erc20[0].direction, Direction::In));
    // gas row preserved
    assert_eq!(batch.gas.len(), 1, "gas row preserved on skip path");
    assert_eq!(batch.gas[0], gas_row);
    // system-credit eth_transfers row preserved (kind=System)
    assert_eq!(batch.eth.len(), 1, "system-credit eth_transfers row preserved");
    assert!(matches!(batch.eth[0].kind, EthKind::System));
    assert_eq!(batch.eth[0].address, w);
    // unattributed marker emitted for the watched non-zero delta
    assert_eq!(batch.unattributed.len(), 1, "one marker per watched-changed address");
    assert_eq!(batch.unattributed[0].address, w);
    assert_eq!(batch.unattributed[0].observed_wei, U256::from(500u64));
    assert_eq!(batch.unattributed[0].attributed_wei, U256::ZERO);
    assert_eq!(batch.unattributed[0].residual_wei, U256::from(500u64));
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd /Users/aj/Documents/code/passbook
cargo test -p passbook-core build_partial_batch_skip_frames_emits_markers_and_keeps_non_frame_rows 2>&1 | tail -8
```
Expected: FAIL — `cannot find function build_partial_batch_skip_frames in this scope`.

- [ ] **Step 3: Implement the helper**

In `crates/passbook-core/src/exex.rs`, add this function immediately AFTER `pub fn process_block(...)` (so the live path is left intact and the skip helper sits next to its sibling):

```rust
/// Skip-mode counterpart to [`process_block`]: when the gated
/// re-execution could not obtain parent state, build a partial
/// [`BlockBatch`] that preserves everything the notification gives
/// us *without* needing the inspector frames — erc20 transfers (from
/// receipts), gas payments, recognised system credits (`kind=System`
/// `eth_transfers` rows) — and emits one [`UnattributedDeltaRow`]
/// marker per watched account whose `BundleState` delta is non-zero.
/// Reconciliation residual checks are skipped (the residual is by
/// design here: we couldn't observe the call frames that would
/// attribute the delta).
///
/// The caller MUST pass `inputs.frames` empty (the skip path doesn't
/// have frames). The natural-PK markers reorg-clean automatically via
/// the existing `delete_blocks(by block_hash)` path.
pub fn build_partial_batch_skip_frames(i: BlockInputs) -> BlockBatch {
    debug_assert!(i.frames.is_empty(), "skip path must be invoked with frames=vec![]");

    // (a) ERC20: identical logic to process_block's section (a).
    let mut erc20 = Vec::new();
    for (tx, log_index, log) in &i.erc20_logs {
        if let Some(d) = decode_transfer(log, &i.watched) {
            for (addr, dir) in d.matched {
                erc20.push(Erc20TransferRow {
                    chain_id: i.chain_id,
                    block_number: i.block_number,
                    block_hash: i.block_hash,
                    tx_hash: tx.expect("erc20 log always in a tx"),
                    log_index: *log_index,
                    token: d.token,
                    from: d.from,
                    to: d.to,
                    amount: d.amount,
                    address: addr,
                    direction: dir,
                });
            }
        }
    }

    // (b) System credits → kind=System eth_transfers rows. Mirrors
    //     process_block's (b2) row-emission (without the per-address
    //     signed sum — there's no reconciliation here).
    let mut eth = Vec::new();
    for sc in &i.system_signed {
        if !i.watched.contains(&sc.address) {
            continue;
        }
        let (direction, amount) = if sc.signed_wei >= 0 {
            (Direction::In, sc.signed_wei.unsigned_abs())
        } else {
            (Direction::Out, sc.signed_wei.unsigned_abs())
        };
        eth.push(EthTransferRow {
            chain_id: i.chain_id,
            block_number: i.block_number,
            block_hash: i.block_hash,
            tx_hash: Some(i.block_hash),
            trace_path: format!("system:{}:{:#x}", sc.source, sc.address),
            address: sc.address,
            direction,
            counterparty: sc.counterparty,
            amount_wei: U256::from(amount),
            kind: EthKind::System,
            reverted: false,
        });
    }

    // (c) Unattributed markers — one per watched-changed address with a
    //     non-zero observed delta. Records the gap so it's auditable
    //     post-sync via the unattributed_deltas table.
    let mut unattributed = Vec::new();
    for (addr, observed) in &i.account_deltas {
        if !i.watched.contains(addr) || *observed == 0 {
            continue;
        }
        unattributed.push(UnattributedDeltaRow {
            chain_id: i.chain_id,
            block_number: i.block_number,
            block_hash: i.block_hash,
            address: *addr,
            observed_wei: U256::from(observed.unsigned_abs()),
            attributed_wei: U256::ZERO,
            residual_wei: U256::from(observed.unsigned_abs()),
        });
    }

    BlockBatch {
        chain_id: i.chain_id,
        block_number: i.block_number,
        block_hash: i.block_hash,
        eth,
        erc20,
        gas: i.gas,
        unattributed,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p passbook-core build_partial_batch_skip_frames_emits_markers_and_keeps_non_frame_rows 2>&1 | tail -5
```
Expected: PASS.

Then full crate suite (no regression):
```bash
cargo test -p passbook-core 2>&1 | tail -8
```
Expected: 0 failures.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "$(printf 'passbook-core: add build_partial_batch_skip_frames helper\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 2 — L1 inner: route to the helper on `get_parent_state()` Err

Switch `process_committed_block_inner` to handle `get_parent_state()` failure with the skip helper instead of returning `ParentStateUnavailable`. Still leaves `OpChainExec` and `run_passbook` unchanged (next tasks).

**Files:**
- Modify: `crates/passbook-core/src/exex.rs` — function `process_committed_block_inner` (current lines ~540-735 around the `if any_watched_changed { ... }` block and the trailing `process_block(BlockInputs { ... })` call).

- [ ] **Step 1: Identify the current code to change**

Locate this block in `process_committed_block_inner`:

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
        // ... (for cf in captured.frames { ... } populates `frames`)
    }
```

And the trailing call at the bottom of the function:
```rust
    process_block(BlockInputs {
        chain_id,
        block_number,
        block_hash,
        watched: watched.clone(),
        erc20_logs,
        frames,
        gas,
        account_deltas,
        system_signed,
    })
```

- [ ] **Step 2: Add a `skip_frames` flag and replace the Err mapping**

Change the gated-re-exec block so an Err from `get_parent_state()` sets a local `skip_frames` flag instead of propagating `ParentStateUnavailable`. Replace just the `let parent_state = ...` line and the `let captured = ...` block; keep the `for cf in captured.frames { ... }` loop UNCHANGED (it just runs over an empty `captured.frames` when we early-skip — actually we never reach it; see below).

Replace the entire `(3) Gated re-execution` block (the `if any_watched_changed { ... }` and the empty-frames declaration above it) with:

```rust
    // ── (3) Gated re-execution → ValueInspector frames. On
    //     get_parent_state() Err (e.g. staged-pipeline backfill where
    //     the provider's best_block is still pinned at the bootstrap
    //     checkpoint), skip frame capture and fall through to the
    //     partial-batch path below. Live operation never hits Err
    //     because Finish advances per-block; the skip is structural
    //     defence so the exex never wedges on parent-state errors.
    let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
    let mut skip_frames = false;
    if any_watched_changed {
        match get_parent_state() {
            Ok(parent_state) => {
                let captured = crate::reexec::reexecute_block_frames(
                    chain_spec.clone(),
                    chain,
                    block,
                    parent_state,
                )
                .map_err(|e| {
                    tracing::error!(error = %e, block = block_number, "re-execution failed");
                    ProcessingError::Decode {
                        block: block_number,
                    }
                })?;
                for cf in captured.frames {
                    let tx_hash = captured.tx_hashes.get(cf.tx_index).copied().flatten();
                    let reverted = captured
                        .tx_reverted
                        .get(cf.tx_index)
                        .copied()
                        .unwrap_or(false);
                    let mut frame = cf.frame;
                    if cf.top_level {
                        frame.trace_path = format!("tx:{}", cf.tx_index);
                    }
                    frames.push((tx_hash, reverted, frame));
                }
            }
            Err(e) => {
                tracing::warn!(
                    block = block_number,
                    error = %e,
                    "parent state unavailable; skipping frame capture and writing unattributed marker"
                );
                skip_frames = true;
            }
        }
    }
```

- [ ] **Step 3: Route to the helper at the bottom of the function**

Replace the trailing `process_block(BlockInputs { ... })` call with a conditional routing to the new helper when frames were skipped:

```rust
    let inputs = BlockInputs {
        chain_id,
        block_number,
        block_hash,
        watched: watched.clone(),
        erc20_logs,
        frames,
        gas,
        account_deltas,
        system_signed,
    };
    if skip_frames {
        Ok(build_partial_batch_skip_frames(inputs))
    } else {
        process_block(inputs)
    }
```

- [ ] **Step 4: Build the crate**

```bash
cargo build -p passbook-core 2>&1 | tail -5
```
Expected: succeeds (still using `ProcessingError::ParentStateUnavailable` is fine — variant still exists; the constructor is gone from this function but the variant is referenced elsewhere we haven't touched yet).

- [ ] **Step 5: Run the crate unit tests**

```bash
cargo test -p passbook-core 2>&1 | tail -8
```
Expected: 0 failures. The existing `parent_state_unavailable_is_processing_error` unit test still passes (variant still exists). The helper test from Task 1 still passes. No regression in the L1 inner-tests fixtures (they all pass an `Ok` thunk through the seam — that path is unchanged).

- [ ] **Step 6: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "$(printf 'passbook-core: L1 inner routes to skip helper on get_parent_state Err\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 3 — OP arm: mirror the skip routing

Apply the same change to `OpChainExec::process_committed_block`. The OP function gathers erc20/account_deltas/gas/system_signed inline (rather than calling `process_committed_block_inner`); the partial-batch construction goes through the same `build_partial_batch_skip_frames` helper because `BlockInputs` is primitive-agnostic.

**Files:**
- Modify: `crates/passbook-stack-optimism/src/op_chain.rs` — function `OpChainExec::process_committed_block`, sections "(3) Gated re-execution" (around the `if any_watched_changed { let parent_state = ... }` block) and the trailing call that builds the final `BlockBatch`.

- [ ] **Step 1: Locate the existing OP gated-re-exec block**

In `crates/passbook-stack-optimism/src/op_chain.rs`, find this block (current behaviour: returns `ParentStateUnavailable` on `get_parent_state()` Err):

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
            for (k, tf) in captured.frames.into_iter().enumerate() {
                let _ = k;
                let tx_hash = captured.tx_hashes.get(tf.tx_index).copied().flatten();
                let reverted = captured
                    .tx_reverted
                    .get(tf.tx_index)
                    .copied()
                    .unwrap_or(false);
                let mut frame = tf.frame;
                if tf.top_level {
                    frame.trace_path = format!("tx:{}", tf.tx_index);
                }
                frames.push((tx_hash, reverted, frame));
            }
        }
```

- [ ] **Step 2: Replace with the skip-aware version**

Replace that entire block with:

```rust
        // ── (3) Gated re-execution → ValueInspector frames (shared
        //    inspector + shared pre-state overlay; OP EVM config). On
        //    get_parent_state() Err (staged-pipeline backfill: provider
        //    best_block pinned at bootstrap checkpoint), skip frame
        //    capture and fall through to the partial-batch path
        //    (build_partial_batch_skip_frames). Live operation never
        //    hits Err because Finish advances per-block; structural
        //    defence so the exex never wedges.
        let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
        let mut skip_frames = false;
        if any_watched_changed {
            match get_parent_state() {
                Ok(parent_state) => {
                    let captured = reexecute_op_block_frames(
                        chain_spec.clone(),
                        chain,
                        block,
                        parent_state,
                    )
                    .map_err(|e| {
                        tracing::error!(
                            error = %e, block = block_number,
                            "OP re-execution failed"
                        );
                        ProcessingError::Decode {
                            block: block_number,
                        }
                    })?;
                    for (k, tf) in captured.frames.into_iter().enumerate() {
                        let _ = k;
                        let tx_hash = captured.tx_hashes.get(tf.tx_index).copied().flatten();
                        let reverted = captured
                            .tx_reverted
                            .get(tf.tx_index)
                            .copied()
                            .unwrap_or(false);
                        let mut frame = tf.frame;
                        if tf.top_level {
                            frame.trace_path = format!("tx:{}", tf.tx_index);
                        }
                        frames.push((tx_hash, reverted, frame));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        block = block_number,
                        error = %e,
                        "OP: parent state unavailable; skipping frame capture and writing unattributed marker"
                    );
                    skip_frames = true;
                }
            }
        }
```

- [ ] **Step 3: Route the trailing batch construction through the helper**

The OP function ends with a `process_block(BlockInputs { ... })` call (mirrors the L1 inner). Locate that call and replace it with the same skip-aware routing as L1. The exact form:

```rust
        let inputs = passbook_core::exex::BlockInputs {
            chain_id,
            block_number,
            block_hash,
            watched: watched.clone(),
            erc20_logs,
            frames,
            gas,
            account_deltas,
            system_signed,
        };
        if skip_frames {
            Ok(passbook_core::exex::build_partial_batch_skip_frames(inputs))
        } else {
            passbook_core::exex::process_block(inputs)
        }
```

(Keep field assignments exactly matching whatever the existing call site uses — copy from the existing `process_block(BlockInputs { ... })` call verbatim, then wrap in the `if skip_frames` conditional. `BlockInputs`, `process_block`, and `build_partial_batch_skip_frames` are all already re-exported from `passbook-core::exex` — verify with `grep -n 'BlockInputs\|process_block' crates/passbook-stack-optimism/src/op_chain.rs` to see the existing import path and use the same form.)

- [ ] **Step 4: Build the workspace**

```bash
cargo build --workspace 2>&1 | tail -5
```
Expected: succeeds (variant still exists, no other code references it directly except its definition + the run_passbook arm + the unit test).

- [ ] **Step 5: Run the workspace tests**

```bash
cargo test --workspace 2>&1 | grep -E 'test result: ok|FAILED|^error' | grep -v '0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered' | tail -10
```
Expected: 0 failures. (The existing Task-10b integration test `parent_state_unavailable_retries_then_succeeds` is still there and uses the OP path — it asserts that 2 failing thunk calls return `Err(ParentStateUnavailable)` and the 3rd returns `Ok(batch)`. Under the new code, the 1st failing call returns `Ok(partial batch)` directly. **That test will now FAIL** — that's expected, Task 5 replaces it.)

If gate 5 fails only on `parent_state_unavailable_retries_then_succeeds`, that's the intended state for this task. Other tests must still pass. Confirm by name:

```bash
cargo test --workspace 2>&1 | grep -E 'test .* FAILED' | head
```
Expected: only `parent_state_unavailable_retries_then_succeeds` listed; no other failures.

- [ ] **Step 6: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-stack-optimism/src/op_chain.rs
git -C /Users/aj/Documents/code/passbook commit -m "$(printf 'passbook-stack-optimism: OP arm routes to skip helper on get_parent_state Err\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 4 — Remove the `run_passbook` `ParentStateUnavailable` retry arm

The dedicated `Err(ProcessingError::ParentStateUnavailable {..}) => { sleep; retry; continue; }` arm in `run_passbook` is now unreachable (no seam impl returns the variant). Remove it. The `Err(other)` catch-all stays for genuine processing faults.

**Files:**
- Modify: `crates/passbook-core/src/exex.rs` — inside `pub async fn run_passbook`, the per-block `match chain_exec.process_committed_block(...)` block (current line ~479).

- [ ] **Step 1: Locate and remove the arm**

Find this arm (currently inside the per-block `match` in `run_passbook`):

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

Delete it entirely. Leave the other arms (`Ok(batch) => { ... }`, `Err(ProcessingError::UnexplainedResidual {..}) => { ... }`, `Err(other) => { ... }`) untouched.

- [ ] **Step 2: Build the workspace**

```bash
cargo build --workspace 2>&1 | tail -5
```
Expected: succeeds.

- [ ] **Step 3: Clippy clean (catches unreachable-pattern / unused-variant warnings)**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```
Expected: clean. (The variant itself is still constructable in test code, so no unused-variant warning yet — that comes in Task 5.)

- [ ] **Step 4: Run the workspace tests**

```bash
cargo test --workspace 2>&1 | grep -E 'test result: ok|FAILED' | grep -v '0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered' | tail -10
```
Expected: same state as after Task 3 — only `parent_state_unavailable_retries_then_succeeds` still failing; replaced in Task 5.

- [ ] **Step 5: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add crates/passbook-core/src/exex.rs
git -C /Users/aj/Documents/code/passbook commit -m "$(printf 'passbook-core: drop unreachable ParentStateUnavailable arm from run_passbook\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 5 — Replace the integration test, remove the variant + its unit test

Update the integration test to the new behaviour, then remove the `ProcessingError::ParentStateUnavailable` variant (now unused) and the unit test that asserted on it.

**Files:**
- Modify: `crates/passbook-core/tests/exex_integration.rs` — rewrite the test `parent_state_unavailable_retries_then_succeeds` as `parent_state_unavailable_writes_partial_batch_and_marker`.
- Modify: `crates/passbook-core/src/exex.rs` — remove the `ParentStateUnavailable` variant from `ProcessingError`; delete the unit test `parent_state_unavailable_is_processing_error`.

- [ ] **Step 1: Rewrite the integration test**

In `crates/passbook-core/tests/exex_integration.rs`, locate the existing test `parent_state_unavailable_retries_then_succeeds` (currently around line 2563) and replace its entire body with the new test. The fixture reuse pattern (SELFDESTRUCT-forwarder, `genesis_state_thunk`, `process_committed_block_inner` direct invocation) follows the same structure as the existing test — preserve all the fixture-construction code; only change the per-call assertion logic.

The replacement test (same function name & attributes are fine, OR rename to `parent_state_unavailable_writes_partial_batch_and_marker` — rename is cleaner):

```rust
/// When get_parent_state() fails on a watched-account-change block,
/// the seam must:
///  - skip the inspector frame re-execution (no frame-derived
///    eth_transfers rows for the watched address);
///  - still capture erc20 transfers, gas payments, and recognised
///    system credits for the block;
///  - emit one unattributed_deltas marker per watched-changed address
///    recording the observed BundleState delta;
///  - return Ok(batch) — never stall, never error.
/// Mirrors the live-mode default (which is exercised by every other
/// integration test that passes the working genesis_state_thunk).
#[tokio::test(flavor = "multi_thread")]
async fn parent_state_unavailable_writes_partial_batch_and_marker() {
    // Reuse the existing SELFDESTRUCT-forwarder fixture (the same one
    // `reorg_replaces_rows_no_dup` and the previous retry test used):
    // it produces a watched-account balance change so the gated path
    // is exercised. Copy the fixture-construction prologue from the
    // existing `parent_state_unavailable_retries_then_succeeds`
    // verbatim — only the assertion block at the end changes.
    //
    // <PROLOGUE — copy from the existing test up to the point where
    // the thunk + process_committed_block_inner call is made>

    // Failing thunk: every call returns Err (never recovers, unlike
    // the prior K-fail-then-succeed pattern).
    let fail_thunk = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
        eyre::bail!("injected: parent state pruned (simulating staged-backfill)")
    };

    // Single call — under the new design this returns Ok(partial
    // batch) immediately, no retry.
    let batch = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        chain.as_ref(),
        &recovered, // or whatever the existing test uses for the recovered block
        &cfg,
        &EthereumStack,
        &fail_thunk,
    )
    .expect("skip path must return Ok partial batch, never error");

    // Frames-derived eth_transfers rows for the watched address must
    // be ABSENT (we couldn't capture frames). System-credit rows
    // (kind=System) for the watched address may still appear if the
    // fixture generates any — count only Internal/TopLevel kinds.
    use passbook_core::model::EthKind;
    let frame_rows_for_watched: usize = batch
        .eth
        .iter()
        .filter(|r| r.address == watched && !matches!(r.kind, EthKind::System))
        .count();
    assert_eq!(frame_rows_for_watched, 0, "no frame-derived rows on skip");

    // At least one unattributed marker for the watched-changed address.
    let markers_for_watched: Vec<_> = batch
        .unattributed
        .iter()
        .filter(|m| m.address == watched)
        .collect();
    assert!(
        !markers_for_watched.is_empty(),
        "skip path must emit ≥1 unattributed_deltas marker per watched-changed address"
    );
    assert_eq!(markers_for_watched[0].block_number, batch.block_number);
    assert_eq!(markers_for_watched[0].block_hash, batch.block_hash);
    assert_eq!(markers_for_watched[0].attributed_wei, alloy_primitives::U256::ZERO);
    assert!(
        markers_for_watched[0].residual_wei > alloy_primitives::U256::ZERO,
        "marker records non-zero residual = observed BundleState delta"
    );
}
```

Notes for the implementer:
- The `<PROLOGUE — copy from the existing test ...>` placeholder must be filled with the verbatim fixture lines from the current `parent_state_unavailable_retries_then_succeeds` function (the SELFDESTRUCT-forwarder bytecode, chain spec, block construction, EthereumStack/OpChainExec selection, `cfg.watched` setup, `handle`/`provider_factory`/`genesis_hash` setup, etc.). The variable names `chain_id`, `chain_spec`, `chain`, `recovered`, `cfg`, `watched` above use the same identifiers as the existing test; check it and align if any name differs.
- The `EthereumStack` argument is the L1 stack adapter (`process_committed_block_inner` requires a `&StackAdapter`). If the existing test uses a different adapter (e.g. `L1Adapter`), use that name. Check `grep -n 'process_committed_block_inner' crates/passbook-core/tests/exex_integration.rs` for the existing call shape.
- DELETE the old test body entirely (the `AtomicUsize` K-fail counter, the loop calling `process_committed_block_inner` multiple times asserting `ParentStateUnavailable` on the first K and `Ok` on K+1, the durable-write block-count assertion). The new design has no retry — a single call returns the partial batch.

- [ ] **Step 2: Remove the `ParentStateUnavailable` variant**

In `crates/passbook-core/src/exex.rs`, find this variant in `enum ProcessingError`:

```rust
    /// The gated re-execution needed the parent block's historical
    /// post-state but the provider could not supply it (e.g. a `--full`
    /// node has pruned it, or the pipeline has not committed it yet).
    /// `run_passbook` treats this exactly like a transient write failure:
    /// stall (retry forever with bounded backoff), never advance.
    #[error("historical parent state unavailable at block {block}: {msg}")]
    ParentStateUnavailable { block: u64, msg: String },
```

Delete the entire variant (the doc comment + the `#[error(...)]` line + the `ParentStateUnavailable { ... },` line).

- [ ] **Step 3: Delete the unit test that referenced the variant**

In `crates/passbook-core/src/exex.rs`'s `#[cfg(test)] mod tests` block, find and DELETE this test:

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

- [ ] **Step 4: Build the workspace**

```bash
cargo build --workspace 2>&1 | tail -5
```
Expected: succeeds. If any other site still references `ProcessingError::ParentStateUnavailable`, the compile fails and points it out — remove that reference too (none should remain after Tasks 2–4).

- [ ] **Step 5: Run the workspace tests**

```bash
cargo test --workspace 2>&1 | grep -E 'test result: ok|FAILED' | grep -v '0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered' | tail -10
```
Expected: **0 failures across all suites.** The replaced integration test now exercises the new skip path; the deleted unit test is gone; the helper test from Task 1 still passes; the live-path tests (with `Ok` thunks) all pass.

- [ ] **Step 6: Clippy clean**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -5
```
Expected: clean — no unused-variant / unreachable-pattern / unused-import warnings.

- [ ] **Step 7: Commit**

```bash
git -C /Users/aj/Documents/code/passbook add \
  crates/passbook-core/src/exex.rs \
  crates/passbook-core/tests/exex_integration.rs
git -C /Users/aj/Documents/code/passbook commit -m "$(printf 'passbook: rewrite Err parent-state test to skip-mode; drop ParentStateUnavailable variant\n\nCo-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>')"
```

---

## Task 6 — Final verification gates (cross-mode build + scope check)

Confirm everything works end-to-end across both build modes the Dockerfile uses.

**Files:** None modified. Verification only.

- [ ] **Step 1: Default build (no Docker features)**

```bash
cd /Users/aj/Documents/code/passbook
cargo build --release -p reth-passbook -p op-reth-passbook 2>&1 | tail -3
```
Expected: succeeds.

- [ ] **Step 2: Docker-path build (jemalloc + asm-keccak) with `--locked`**

```bash
cargo build --release --locked -p reth-passbook -p op-reth-passbook \
  --features reth-passbook/jemalloc,op-reth-passbook/jemalloc,reth-passbook/asm-keccak,op-reth-passbook/asm-keccak 2>&1 | tail -3
```
Expected: succeeds WITH `--locked` (proves `Cargo.lock` still in sync — no spurious dep changes from this work).

- [ ] **Step 3: Full workspace test suite**

```bash
cargo test --workspace 2>&1 | grep -E 'test result: ok\.|FAILED|^error\[' | grep -v '0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered' | tail -10
```
Expected: all suites `ok`, 0 failures.

- [ ] **Step 4: Clippy clean, all features**

```bash
cargo clippy --workspace -- -D warnings 2>&1 | tail -3
cargo clippy --workspace --features reth-passbook/jemalloc,op-reth-passbook/jemalloc,reth-passbook/asm-keccak,op-reth-passbook/asm-keccak -- -D warnings 2>&1 | tail -3
```
Expected: both invocations clean (no warnings under either feature set).

- [ ] **Step 5: Scope check — only the intended files changed**

```bash
git -C /Users/aj/Documents/code/passbook diff --stat <SHA_BEFORE_TASK_1>..HEAD
```
Replace `<SHA_BEFORE_TASK_1>` with `6f9b212` (the spec commit; the Task 1 commit is its child). Expected: exactly three files changed: `crates/passbook-core/src/exex.rs`, `crates/passbook-core/tests/exex_integration.rs`, `crates/passbook-stack-optimism/src/op_chain.rs`. No `Cargo.toml` / `Cargo.lock` / `Dockerfile` / other files.

If any unexpected file appears, investigate and revert before finishing.

- [ ] **Step 6: Final commit log review**

```bash
git -C /Users/aj/Documents/code/passbook log --oneline 6f9b212..HEAD
```
Expected: 5 task commits in order — `Task 1 helper`, `Task 2 L1 routing`, `Task 3 OP routing`, `Task 4 drop run_passbook arm`, `Task 5 rewrite test + drop variant`. **No push** — the user pushes (per repo convention).

---

## Self-Review

**Spec coverage:**
- Design principle "parent state unavailable ⇒ skip + marker + advance" → Tasks 1 (helper) + 2 (L1 routing) + 3 (OP routing). ✓
- Behaviour table (Live Ok, Catch-up Err, Unexpected Err all route to the same skip) → Tasks 2, 3 (single Err branch handles all cases identically). ✓
- Partial-batch contract (erc20 + gas + system + unattributed marker; no frames; no reconciliation) → Task 1 helper assertions cover every captured/skipped category. ✓
- Reorg handling (existing `delete_blocks(by block_hash)` reorg path) → no code change needed; spec note covered, no plan task required. ✓
- No schema change → confirmed, no Cargo.toml / migration / schema task in plan. ✓
- Remove `ParentStateUnavailable` variant + Task-1 unit test + Task-10b integration test → Task 5. ✓
- Remove `run_passbook` retry arm → Task 4. ✓
- Both build modes (default + jemalloc/asm-keccak) green with `--locked` → Task 6 Steps 1–2. ✓
- Clippy clean under both feature sets → Task 6 Step 4. ✓
- Scope exactly three files → Task 6 Step 5. ✓
- Out-of-scope items (retroactive backfill capture, config gates, ExEx disable) explicitly absent from the plan. ✓

**Placeholder scan:**
- One genuine ambiguity surfaced in Task 5 Step 1: the `<PROLOGUE>` placeholder. This is unavoidable without verbatim reproducing the existing test's fixture prologue here (which would be brittle — the test changes over time). Mitigated by explicit pointer to the existing test by name and a `grep` command to locate it, plus identifier names called out so the implementer knows what to align. This is the documented "copy verbatim from existing test, change only the assertion block" pattern. Acceptable; the implementer Reads the file and copies precisely.
- No `TBD` / `implement later` / `handle edge cases` / "similar to Task N" elsewhere. ✓

**Type consistency:**
- `BlockInputs`, `BlockBatch`, `UnattributedDeltaRow`, `EthTransferRow`, `Erc20TransferRow`, `GasPaymentRow`, `Direction`, `EthKind::System`, `SystemCredit`, `decode_transfer`, `process_block`, `build_partial_batch_skip_frames`, `process_committed_block_inner`, `ProcessingError::ParentStateUnavailable`, `reexecute_block_frames`, `reexecute_op_block_frames` — all consistent across tasks; signatures and field names match the current codebase as verified pre-write. ✓
- `skip_frames: bool` local in Tasks 2 and 3 — same name, same role. ✓

---

## Execution Handoff

(See top-of-plan REQUIRED SUB-SKILL note. Use **superpowers:subagent-driven-development** — the established workflow for this project.)
