//! OP arm of the [`ChainExec`](passbook_core::exex::ChainExec) seam.
//!
//! `OpChainExec` is the Optimism counterpart of `passbook-core`'s
//! `EthChainExec`. It implements the SAME [`ChainExec`] trait so the
//! SAME generic `passbook_core::exex::run_passbook` loop (reorg-first
//! delete / retry-until-success / FinishedHeight-only-after-durable-write)
//! drives both the L1 and OP binaries — the safety contract and the pure
//! `process_block` / reconcile / ledger / `ValueInspector` are NOT
//! duplicated; they are invoked from `passbook-core`.
//!
//! What is genuinely chain-specific (and therefore lives here):
//!
//! 1. The node bound: `Primitives = OpPrimitives`,
//!    `ChainSpec = OpChainSpec`.
//! 2. The re-exec EVM config: `OpEvmConfig::optimism(chain_spec)` instead
//!    of `EthEvmConfig::new` — but the inspector machinery
//!    ([`passbook_core::reexec::TaggingInspector`]) and the parent-state
//!    pre-state overlay ([`passbook_core::reexec::build_prestate_cache`])
//!    are reused VERBATIM (no fork of `ValueInspector`).
//! 3. The per-block OP L1 data fee: `reth_optimism_evm::extract_l1_info`
//!    once per L2 block, then `RethL1BlockInfo::l1_tx_data_fee` per tx,
//!    fed through `passbook_stack_optimism::build_block_l1_fee_table`
//!    into an [`OptimismStack`] adapter (Task 8.2). Deposits / the
//!    index-0 L1-info tx → `None`.
//! 4. OP receipt/tx field access: `OpReceipt` is an enum reached via the
//!    `alloy_consensus::TxReceipt` trait (`.status()`, `.logs()`,
//!    `.cumulative_gas_used()`), and `OpTransactionSigned` deposits are
//!    detected via the inherent `OpTxEnvelope::is_deposit`.
//!
//! Everything else (ERC20 decode, gas attribution, frame attribution,
//! reconciliation) is the shared `passbook-core` code, invoked here.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Transaction, TxReceipt};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, U256};

use passbook_core::attribution::{compute_gas_payment, GasInput};
use passbook_core::config::PassbookConfig;
use passbook_core::erc20::RawLog;
use passbook_core::exex::{process_block, BlockInputs, ChainExec, ProcessingError};
use passbook_core::inspector::CapturedFrame;
use passbook_core::ledger::writer::BlockBatch;
use passbook_core::reexec::{build_prestate_cache, Captured, TaggedFrame, TaggingInspector};
use passbook_core::stack::StackAdapter;
use passbook_core::system::SystemCredit;

use reth_op::chainspec::OpChainSpec;
use reth_op::primitives::RecoveredBlock;
use reth_op::provider::Chain;
use reth_op::storage::StateProviderBox;
use reth_op::{OpBlock, OpPrimitives};

use reth_optimism_evm::{extract_l1_info, OpEvmConfig, RethL1BlockInfo};

use crate::build_block_l1_fee_table;

/// OP arm of the [`ChainExec`] seam.
///
/// Stateless — a single instance is moved into `run_passbook`. The
/// per-block [`OptimismStack`](crate::OptimismStack) adapter is built
/// fresh inside [`Self::process_committed_block`] from that block's
/// extracted `L1BlockInfo` (it is inherently per-block: the L1 base/blob
/// fee scalars change every L2 block), which is why the OP arm folds the
/// L1-fee extraction into the seam rather than the old
/// `make_adapter: impl Fn() -> S` shape — consistent with how the L1 arm
/// also constructs its adapter per block.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpChainExec;

impl ChainExec for OpChainExec {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;

    fn process_committed_block(
        &self,
        chain_id: u64,
        chain_spec: Arc<OpChainSpec>,
        chain: &Chain<OpPrimitives>,
        block: &RecoveredBlock<OpBlock>,
        cfg: &PassbookConfig,
        parent_state: StateProviderBox,
    ) -> Result<BlockBatch, ProcessingError> {
        let block_number = block.header().number();
        let block_hash = block.hash();
        let base_fee = block.header().base_fee_per_gas();
        let timestamp = block.header().timestamp();
        let watched = &cfg.watched;

        // Per-block execution outcome (the committed `Chain` bundle is a
        // MULTI-block aggregate; split to THIS block only).
        let outcome =
            chain
                .execution_outcome_at_block(block_number)
                .ok_or(ProcessingError::Decode {
                    block: block_number,
                })?;

        // ── (1) ERC20: receipts + logs for THIS block. OP `OpReceipt`
        //    is an enum → use the `TxReceipt` trait accessors.
        let mut erc20_logs: Vec<(Option<B256>, u64, RawLog)> = Vec::new();
        {
            let receipts = outcome.receipts_by_block(block_number);
            let mut log_index: u64 = 0;
            for (tx_idx, receipt) in receipts.iter().enumerate() {
                let tx_hash = block
                    .body()
                    .transactions
                    .get(tx_idx)
                    .map(|t| B256::from(*t.tx_hash()));
                for log in receipt.logs() {
                    erc20_logs.push((
                        tx_hash,
                        log_index,
                        RawLog {
                            address: log.address,
                            topics: log.topics().to_vec(),
                            data: log.data.data.clone(),
                        },
                    ));
                    log_index += 1;
                }
            }
        }

        // ── (2) Gate: per-block BundleState old/new balances for
        //    watched (identical logic to the L1 arm; the `BundleState`
        //    is primitive-agnostic).
        let mut account_deltas: Vec<(alloy_primitives::Address, i128)> = Vec::new();
        let mut any_watched_changed = false;
        for (addr, acct) in outcome.bundle_accounts_iter() {
            if !watched.contains(&addr) {
                continue;
            }
            let old_bal = acct
                .original_info
                .as_ref()
                .map(|i| i.balance)
                .unwrap_or(U256::ZERO);
            let new_bal = acct.info.as_ref().map(|i| i.balance).unwrap_or(U256::ZERO);
            let old_nonce = acct.original_info.as_ref().map(|i| i.nonce).unwrap_or(0);
            let new_nonce = acct.info.as_ref().map(|i| i.nonce).unwrap_or(0);
            if old_bal != new_bal || old_nonce != new_nonce {
                any_watched_changed = true;
            }
            let delta = i128::try_from(new_bal).map_err(|_| ProcessingError::Decode {
                block: block_number,
            })? - i128::try_from(old_bal).map_err(|_| ProcessingError::Decode {
                block: block_number,
            })?;
            account_deltas.push((addr, delta));
        }

        // ── (3) Gated re-execution → ValueInspector frames (shared
        //    inspector + shared pre-state overlay; OP EVM config).
        let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
        if any_watched_changed {
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

        // ── (3b) Per-block OP L1 data fee table (Task 8.2 glue). One
        //    `extract_l1_info` per L2 block; per-tx `l1_tx_data_fee`.
        //    `extract_l1_info` fails only on an empty block (no L1-info
        //    tx) — then there are no fees to record.
        let l1_adapter = {
            let txs_meta: Vec<(bool, Vec<u8>)> = block
                .body()
                .transactions
                .iter()
                .map(|t| (t.is_deposit(), t.encoded_2718()))
                .collect();
            match extract_l1_info(block.body()) {
                Ok(mut l1_info) => build_block_l1_fee_table(txs_meta, |raw: &[u8]| {
                    l1_info
                        .l1_tx_data_fee(chain_spec.as_ref(), timestamp, raw, false)
                        .ok()
                        .filter(|v| !v.is_zero())
                }),
                Err(_) => crate::OptimismStack::from_fees(vec![None; txs_meta.len()]),
            }
        };

        // ── (4) Gas: per tx whose sender ∈ watched (even if reverted).
        let mut gas = Vec::new();
        {
            let receipts = outcome.receipts_by_block(block_number);
            let mut prev_cumulative: u64 = 0;
            for (tx_idx, (sender, tx)) in block.transactions_with_sender().enumerate() {
                let receipt = match receipts.get(tx_idx) {
                    Some(r) => r,
                    None => break,
                };
                let gas_used = receipt
                    .cumulative_gas_used()
                    .saturating_sub(prev_cumulative);
                prev_cumulative = receipt.cumulative_gas_used();
                if !watched.contains(sender) {
                    continue;
                }
                let effective_gas_price = tx.effective_gas_price(base_fee);
                let l1_fee = l1_adapter.l1_data_fee_wei(tx_idx);
                gas.push(compute_gas_payment(GasInput {
                    chain_id,
                    block_number,
                    block_hash,
                    tx_hash: B256::from(*tx.tx_hash()),
                    tx_from: *sender,
                    gas_used,
                    effective_gas_price,
                    l1_fee_wei: l1_fee,
                }));
            }
        }

        // ── (5) system_signed: recognised OP non-call credits.
        //   **Deposit mints**: an OP deposit tx (`OpTxEnvelope::Deposit`)
        //   can `mint` ETH to its recipient with NO value-CALL frame —
        //   `TxDeposit { mint: u128, to: TxKind, .. }` (op-alloy 0.23.1
        //   `transaction/deposit.rs`). If the mint recipient ∈ watched
        //   that minted wei is a recognised `kind=system` credit so
        //   reconciliation nets it (no residual / stall).
        //
        //   **Fee vaults**: per non-deposit tx, the pinned op-revm
        //   `reward_beneficiary` (rev 27bf919,
        //   `op-revm/src/handler.rs:298`) credits THREE predeploy vaults
        //   with NO captured CALL frame:
        //     • SequencerFeeVault (block coinbase, `0x..11`) gets the
        //       priority fee `(effective_gas_price − base_fee) × gas_used`
        //       (the mainnet `reward_beneficiary` delegate);
        //     • BaseFeeVault (`0x..19`) gets `base_fee × gas_used`;
        //     • L1FeeVault (`0x..1a`) gets the per-tx L1 data cost.
        //   These are a spec §(c) recognised system category ("OP …  fee
        //   vaults … produce no residual"). Previously unhandled, so a
        //   watched fee-vault address residual-stalled — issue #5. We now
        //   extract them from the SAME per-tx (gas_used, effective price,
        //   L1 fee) data already gathered for gas attribution + the
        //   per-block L1-fee table, mirroring the L1 arm's `block_reward`
        //   priority-fee design. The Isthmus operator-fee vault (`0x..1b`)
        //   is left as a documented narrow gap (defaults to zero on
        //   effectively all chains; not reconstructable without re-running
        //   the EVM at the pinned API).
        let system_signed = {
            // `as_deposit()` is INHERENT on `OpTxEnvelope` (op-alloy
            // `transaction/envelope.rs:436`); `OpTransactionSigned` is a
            // type alias for it, so no extra op-alloy dependency edge is
            // needed. Extract `(mint, to, from)` per deposit, then run
            // the pure recogniser.
            let deposits: Vec<(
                u128,
                alloy_primitives::TxKind,
                alloy_primitives::Address,
                B256,
            )> = block
                .body()
                .transactions
                .iter()
                .filter_map(|tx| {
                    let h = B256::from(*tx.tx_hash());
                    tx.as_deposit()
                        .map(|d| d.inner())
                        .map(|d| (d.mint, d.to, d.from, h))
                })
                .collect();
            let mut creds = deposit_mint_credits(&deposits, watched);

            // Per non-deposit tx: (effective_gas_price, gas_used, l1_fee).
            // Deposit txs pay no priority/base/L1 fee to the vaults (the
            // op-revm handler short-circuits deposits before
            // `reward_beneficiary`), so they contribute nothing here. The
            // block coinbase IS the SequencerFeeVault predeploy on OP.
            let base_fee_u128 = u128::from(base_fee.unwrap_or(0));
            let coinbase = block.header().beneficiary();
            let mut vault_txs: Vec<(u128, u64, U256)> = Vec::new();
            {
                let receipts = outcome.receipts_by_block(block_number);
                let mut prev_cumulative: u64 = 0;
                for (tx_idx, tx) in block.body().transactions.iter().enumerate() {
                    let receipt = match receipts.get(tx_idx) {
                        Some(r) => r,
                        None => break,
                    };
                    let gas_used = receipt
                        .cumulative_gas_used()
                        .saturating_sub(prev_cumulative);
                    prev_cumulative = receipt.cumulative_gas_used();
                    if tx.is_deposit() {
                        continue;
                    }
                    let l1_fee = l1_adapter.l1_data_fee_wei(tx_idx).unwrap_or(U256::ZERO);
                    vault_txs.push((tx.effective_gas_price(base_fee), gas_used, l1_fee));
                }
            }
            creds.extend(fee_vault_credits(
                watched,
                coinbase,
                base_fee_u128,
                &vault_txs,
            ));
            creds
        };

        // SHARED pure orchestrator — invoked, never forked.
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
    }
}

/// Pure recognition of OP **deposit-mint** system credits.
///
/// An OP deposit transaction (`OpTxEnvelope::Deposit`,
/// op-alloy-consensus `TxDeposit`) carries a `mint: u128` minting that
/// many wei to the deposit recipient (`to: TxKind`) with NO captured
/// value-CALL frame. The OP seam extracts `(mint, to, from)` per deposit
/// tx via the INHERENT `OpTxEnvelope::as_deposit()` (so no extra
/// op-alloy dependency edge is needed) and feeds them here. For each
/// deposit whose recipient ∈ `watched` with a non-zero mint, this yields
/// a recognised `kind=system` credit so reconciliation nets it (spec
/// §(b)/(c)) — no residual / stall.
///
/// The `source` tag embeds the **originating deposit tx hash**
/// (`deposit_mint:<tx_hash>`) so two deposits minting to the *same*
/// watched address in one block no longer collide: exex.rs derives the
/// `eth_transfers` `trace_path` from `(source, address)`, and a bare
/// `"deposit_mint"` tag would make both rows share the natural PK
/// `(chain_id, block_hash, tx_hash=block_hash, trace_path)` so
/// `INSERT OR REPLACE` would silently drop one (issue #6 / I2). The
/// deposit tx hash is unique per deposit and deterministic across
/// replays, so the disambiguated `trace_path` stays stable & idempotent.
/// Kept type-free (`&[(mint, TxKind, from, tx_hash)]`) so it is
/// unit-testable without an OP block, exactly mirroring the L1
/// `l1_system_credits` design.
fn deposit_mint_credits(
    deposits: &[(
        u128,
        alloy_primitives::TxKind,
        alloy_primitives::Address,
        B256,
    )],
    watched: &std::collections::HashSet<alloy_primitives::Address>,
) -> Vec<SystemCredit> {
    let mut creds: Vec<SystemCredit> = Vec::new();
    for (mint, to, from, tx_hash) in deposits {
        if *mint == 0 {
            continue;
        }
        if let alloy_primitives::TxKind::Call(to) = to {
            if watched.contains(to) {
                creds.push(SystemCredit::new(
                    *to,
                    i128::try_from(*mint).unwrap_or(i128::MAX),
                    *from,
                    format!("deposit_mint:{tx_hash:#x}"),
                ));
            }
        }
    }
    creds
}

/// The three OP fee-vault predeploys credited (no CALL frame) by the
/// op-revm `reward_beneficiary` handler per non-deposit tx. Addresses are
/// the canonical predeploys (op-revm `constants.rs`,
/// kona `predeploys.rs`).
mod fee_vault {
    use alloy_primitives::{address, Address};
    /// SequencerFeeVault — the OP block coinbase; receives the priority
    /// fee. Production uses the actual block coinbase (so a non-standard
    /// coinbase still reconciles); this canonical constant documents the
    /// predeploy and is used by the recogniser's unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub const SEQUENCER_FEE_VAULT: Address =
        address!("0x4200000000000000000000000000000000000011");
    /// BaseFeeVault — receives `base_fee × gas_used` per tx.
    pub const BASE_FEE_VAULT: Address = address!("0x4200000000000000000000000000000000000019");
    /// L1FeeVault — receives the per-tx L1 data cost.
    pub const L1_FEE_VAULT: Address = address!("0x420000000000000000000000000000000000001a");
}

/// Pure recognition of OP **fee-vault** system credits (spec §(c)).
///
/// For every non-deposit tx the op-revm `reward_beneficiary` handler
/// (`op-revm/src/handler.rs:298`, pinned rev 27bf919) credits, with NO
/// captured CALL frame:
///
/// - the **block coinbase** (the SequencerFeeVault predeploy on OP) with
///   the priority fee `(effective_gas_price − base_fee) × gas_used`
///   (the delegated mainnet `reward_beneficiary`);
/// - the **BaseFeeVault** (`0x..19`) with `base_fee × gas_used`;
/// - the **L1FeeVault** (`0x..1a`) with that tx's L1 data cost.
///
/// Each total whose recipient ∈ `watched` becomes one netted
/// `kind=system` [`SystemCredit`] (one row per vault, stable `source`
/// tag) so reconciliation nets it to zero instead of residual-stalling
/// (issue #5). `coinbase` is passed explicitly (rather than assuming the
/// `SEQUENCER_FEE_VAULT` constant) so a non-standard coinbase still
/// reconciles correctly. Kept type-free (`&[(eff_price, gas_used,
/// l1_fee)]`) so it is unit-testable without an OP block, mirroring
/// [`deposit_mint_credits`] and the L1 `l1_system_credits` design.
fn fee_vault_credits(
    watched: &std::collections::HashSet<alloy_primitives::Address>,
    coinbase: alloy_primitives::Address,
    base_fee_per_gas: u128,
    txs: &[(u128, u64, U256)], // (effective_gas_price, gas_used, l1_fee_wei) per non-deposit tx
) -> Vec<SystemCredit> {
    let mut priority_total = U256::ZERO; // → coinbase / SequencerFeeVault
    let mut base_total = U256::ZERO; // → BaseFeeVault
    let mut l1_total = U256::ZERO; // → L1FeeVault
    for (effective_gas_price, gas_used, l1_fee) in txs {
        let prio = effective_gas_price.saturating_sub(base_fee_per_gas);
        priority_total += U256::from(prio) * U256::from(*gas_used);
        base_total += U256::from(base_fee_per_gas) * U256::from(*gas_used);
        l1_total += *l1_fee;
    }

    let mut creds: Vec<SystemCredit> = Vec::new();
    let mut push = |addr: alloy_primitives::Address, total: U256, source: &str| {
        if total.is_zero() || !watched.contains(&addr) {
            return;
        }
        creds.push(SystemCredit::new(
            addr,
            i128::try_from(total).unwrap_or(i128::MAX),
            addr,
            source,
        ));
    };
    push(coinbase, priority_total, "sequencer_fee_vault");
    push(fee_vault::BASE_FEE_VAULT, base_total, "base_fee_vault");
    push(fee_vault::L1_FEE_VAULT, l1_total, "l1_fee_vault");
    creds
}

/// OP re-execution driver — the structural analogue of
/// `passbook_core::reexec::reexecute_block_frames`, differing ONLY in
/// the concrete EVM config (`OpEvmConfig::optimism`) and the OP receipt
/// accessor (`TxReceipt::status`). The value-attribution inspector
/// ([`TaggingInspector`]) and the parent-state pre-state overlay
/// ([`build_prestate_cache`]) are reused VERBATIM from `passbook-core`
/// (no `ValueInspector` fork).
fn reexecute_op_block_frames(
    chain_spec: Arc<OpChainSpec>,
    chain: &Chain<OpPrimitives>,
    block: &RecoveredBlock<OpBlock>,
    parent_state: StateProviderBox,
) -> eyre::Result<Captured> {
    use reth_op::evm::primitives::block::BlockExecutor;
    use reth_op::evm::primitives::evm::EvmFactoryExt;
    use reth_op::evm::primitives::tracing::TracingCtx;
    use reth_op::evm::primitives::ConfigureEvm;
    use revm::database::State;

    let block_number = block.header().number();

    // 1. Pre-block state = REAL parent post-state + in-chain overlay
    //    (the SHARED, primitive-agnostic builder).
    let cache = build_prestate_cache(chain, block_number, parent_state);

    // 2. Real OP EVM config from the chain spec (NOT the node's).
    let evm_config = OpEvmConfig::optimism(chain_spec);
    let mut state = State::builder()
        .with_database(cache)
        .with_bundle_update()
        .build();

    let evm_env = evm_config.evm_env(block.header())?;

    // 2a. OP pre-execution system changes (e.g. L1-block info / Canyon
    //     create2-deployer) so the replayed state matches consensus.
    {
        let exec_ctx = evm_config.context_for_block(block.sealed_block())?;
        let mut executor = evm_config.create_executor(
            evm_config.evm_with_env(&mut state, evm_env.clone()),
            exec_ctx,
        );
        executor.apply_pre_execution_changes()?;
    }

    // 3. Per-tx inspector tracer (drives nested-frame inspector hooks).
    let this_outcome = chain
        .execution_outcome_at_block(block_number)
        .ok_or_else(|| eyre::eyre!("no execution outcome for block {block_number}"))?;
    let receipts = this_outcome.receipts_by_block(block_number);
    let mut frames: Vec<TaggedFrame> = Vec::new();
    let mut tx_hashes: Vec<Option<B256>> = Vec::new();
    let mut tx_reverted: Vec<bool> = Vec::new();
    for (i, tx) in block.body().transactions.iter().enumerate() {
        tx_hashes.push(Some(B256::from(*tx.tx_hash())));
        tx_reverted.push(receipts.get(i).map(|r| !r.status()).unwrap_or(false));
    }

    let mut tracer =
        evm_config
            .evm_factory()
            .create_tracer(&mut state, evm_env, TaggingInspector::default());
    let collected: Vec<(usize, Vec<(CapturedFrame, bool)>)> = {
        let mut idx = 0usize;
        tracer
            .try_trace_many(
                block.transactions_recovered(),
                |mut ctx: TracingCtx<'_, _, _>| {
                    let insp = ctx.take_inspector();
                    let tags = insp.tags.clone();
                    let fs: Vec<(CapturedFrame, bool)> = insp
                        .inner
                        .into_frames()
                        .into_iter()
                        .enumerate()
                        .map(|(k, f)| (f, tags.get(k).copied().unwrap_or(false)))
                        .collect();
                    let this = idx;
                    idx += 1;
                    Ok::<_, eyre::Error>((this, fs))
                },
            )
            .commit_last_tx()
            .collect::<Result<_, _>>()?
    };

    for (tx_index, fs) in collected {
        for (frame, top_level) in fs {
            frames.push(TaggedFrame {
                frame,
                tx_index,
                top_level,
            });
        }
    }
    Ok(Captured {
        frames,
        tx_hashes,
        tx_reverted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_block_l1_fee_table;

    /// Static type-level proof the OP seam arm wires the OP node bound:
    /// `run_passbook`'s `Node::Types: NodeTypes<Primitives =
    /// C::Primitives, ChainSpec = C::ChainSpec>` is satisfied by `OpNode`
    /// iff these associated types are exactly `OpPrimitives` /
    /// `OpChainSpec` (the same pair `OpNode: NodeTypes` declares).
    /// B1 (OP arm): the deposit-mint recogniser maps a deposit `mint`
    /// to a watched recipient into a `kind=system` `SystemCredit` so
    /// reconciliation nets it (no residual / stall), and correctly
    /// EXCLUDES: a mint to an unwatched address, a zero-mint deposit,
    /// and a `TxKind::Create` deposit (no recipient). This is the OP
    /// counterpart of the L1 `system::tests` recognition proofs; a live
    /// OP end-to-end block test remains infeasible at the pinned revs
    /// (no OP `Chain<OpPrimitives>` harness in `reth-exex-test-utils`),
    /// documented in docs/reth-pin.md + docs/validation.md.
    #[test]
    fn op_deposit_mint_to_watched_is_recognized_system_credit() {
        use alloy_primitives::{Address, TxKind};
        let w = Address::repeat_byte(0xD7);
        let other = Address::repeat_byte(0x11);
        let from = Address::repeat_byte(0xFF);
        let h0 = B256::repeat_byte(0xA0);
        let h1 = B256::repeat_byte(0xA1);
        let h2 = B256::repeat_byte(0xA2);
        let h3 = B256::repeat_byte(0xA3);
        let watched = [w].into_iter().collect();
        let deposits = vec![
            (5_000_000_000_000_000u128, TxKind::Call(w), from, h0), // watched ⇒ Some
            (9_000_000_000_000_000u128, TxKind::Call(other), from, h1), // unwatched ⇒ skip
            (0u128, TxKind::Call(w), from, h2),                    // zero mint ⇒ skip
            (1_000u128, TxKind::Create, from, h3),                 // no recipient ⇒ skip
        ];
        let creds = deposit_mint_credits(&deposits, &watched);
        assert_eq!(creds.len(), 1, "only the watched, non-zero, Call mint");
        assert_eq!(creds[0].address, w);
        assert_eq!(creds[0].signed_wei, 5_000_000_000_000_000i128);
        assert_eq!(creds[0].counterparty, from);
        assert_eq!(creds[0].source, format!("deposit_mint:{h0:#x}"));
    }

    /// Issue #6 (I2): two deposit txs minting to the SAME watched address
    /// in ONE block must yield two DISTINCT `SystemCredit`s whose `source`
    /// tags differ (by originating deposit tx hash). exex.rs derives the
    /// `eth_transfers` `trace_path` as `system:<source>:<address>`; with a
    /// bare `"deposit_mint"` tag both rows shared the natural PK
    /// `(chain_id, block_hash, tx_hash=block_hash, trace_path)` so
    /// `INSERT OR REPLACE` silently destroyed one — under-reporting the
    /// deposit-mint rows (spec line 193: never lose an entry; §(c)
    /// queryable system rows). Distinct sources ⇒ distinct trace_paths ⇒
    /// both rows persist.
    #[test]
    fn op_multiple_deposit_mints_to_same_watched_addr_dont_collide() {
        use alloy_primitives::{Address, TxKind};
        use std::collections::HashSet;
        let w = Address::repeat_byte(0xD7);
        let from = Address::repeat_byte(0xFF);
        let h_a = B256::repeat_byte(0x01);
        let h_b = B256::repeat_byte(0x02);
        let watched: HashSet<Address> = [w].into_iter().collect();
        let deposits = vec![
            (1_000u128, TxKind::Call(w), from, h_a),
            (2_000u128, TxKind::Call(w), from, h_b),
        ];
        let creds = deposit_mint_credits(&deposits, &watched);
        assert_eq!(creds.len(), 2, "both same-address mints recognised");
        // Both credit the same watched address with their own amount …
        assert_eq!(creds[0].address, w);
        assert_eq!(creds[1].address, w);
        assert_eq!(creds[0].signed_wei, 1_000i128);
        assert_eq!(creds[1].signed_wei, 2_000i128);
        // … but the per-deposit `source` tags differ, so the derived
        // `trace_path` (system:<source>:<address>) no longer collides.
        assert_ne!(
            creds[0].source, creds[1].source,
            "per-deposit source must disambiguate same-address mints (#6)"
        );
        assert_eq!(creds[0].source, format!("deposit_mint:{h_a:#x}"));
        assert_eq!(creds[1].source, format!("deposit_mint:{h_b:#x}"));
    }

    /// Issue #5 (OP arm): the three OP fee-vault predeploys credited by
    /// the op-revm `reward_beneficiary` handler with NO call frame
    /// (SequencerFeeVault = coinbase priority fee, BaseFeeVault =
    /// `base_fee × gas_used`, L1FeeVault = Σ per-tx L1 data cost) are
    /// recognised as `kind=system` `SystemCredit`s when watched, so a
    /// watched fee vault nets to zero instead of a spec-violating
    /// residual stall. Only watched vaults yield rows; a zero total
    /// yields none; deposit txs are excluded by the caller (none passed).
    #[test]
    fn op_fee_vault_credits_recognized_as_system_for_watched_vaults() {
        use alloy_primitives::Address;
        let base_fee: u128 = 7;
        // Two non-deposit txs.
        //  tx0: eff price 1 gwei, 21000 gas, L1 fee 500
        //  tx1: eff price 2 gwei,  50000 gas, L1 fee 800
        let txs = vec![
            (1_000_000_000u128, 21_000u64, U256::from(500u64)),
            (2_000_000_000u128, 50_000u64, U256::from(800u64)),
        ];
        let priority = (1_000_000_000u128 - 7) * 21_000 + (2_000_000_000u128 - 7) * 50_000;
        let base = 7u128 * 21_000 + 7u128 * 50_000;
        let l1 = 500u128 + 800u128;

        // Watch ALL THREE vaults (coinbase == SequencerFeeVault predeploy).
        let coinbase = fee_vault::SEQUENCER_FEE_VAULT;
        let watched: std::collections::HashSet<Address> = [
            coinbase,
            fee_vault::BASE_FEE_VAULT,
            fee_vault::L1_FEE_VAULT,
        ]
        .into_iter()
        .collect();
        let creds = fee_vault_credits(&watched, coinbase, base_fee, &txs);
        assert_eq!(creds.len(), 3, "one netted row per watched vault");

        let by_src = |s: &str| creds.iter().find(|c| c.source == s).expect("row present");
        let seq = by_src("sequencer_fee_vault");
        assert_eq!(seq.address, coinbase);
        assert_eq!(seq.signed_wei, priority as i128);
        let bfv = by_src("base_fee_vault");
        assert_eq!(bfv.address, fee_vault::BASE_FEE_VAULT);
        assert_eq!(bfv.signed_wei, base as i128);
        let l1fv = by_src("l1_fee_vault");
        assert_eq!(l1fv.address, fee_vault::L1_FEE_VAULT);
        assert_eq!(l1fv.signed_wei, l1 as i128);

        // Unwatched vaults ⇒ no rows; only the watched one is emitted.
        let only_base: std::collections::HashSet<Address> =
            [fee_vault::BASE_FEE_VAULT].into_iter().collect();
        let creds = fee_vault_credits(&only_base, coinbase, base_fee, &txs);
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].source, "base_fee_vault");

        // Zero base fee + zero priority + zero L1 ⇒ no rows at all.
        let creds = fee_vault_credits(
            &watched,
            coinbase,
            0,
            &[(0u128, 21_000u64, U256::ZERO)],
        );
        assert!(
            creds.is_empty(),
            "no vault row when every fee total is zero"
        );
    }

    /// Reconciliation-shaped proof (issue #5): a watched fee vault's full
    /// observed BundleState balance delta in a block is exactly the sum
    /// of the recognised fee-vault `SystemCredit`s, so the shared pure
    /// `process_block` reconciler nets it to ZERO residual (no stall),
    /// AND emits a queryable `kind=system` row. This is the OP
    /// counterpart of the L1 `block_reward` zero-residual integration
    /// proof (a live OP `Chain<OpPrimitives>` harness remains infeasible
    /// at the pinned revs — see docs/reth-pin.md).
    #[test]
    fn watched_fee_vault_nets_to_zero_residual_via_process_block() {
        use alloy_primitives::Address;
        use passbook_core::exex::{process_block, BlockInputs};

        let base_fee: u128 = 7;
        let txs = vec![
            (1_000_000_000u128, 21_000u64, U256::from(500u64)),
            (2_000_000_000u128, 50_000u64, U256::from(800u64)),
        ];
        let priority = (1_000_000_000u128 - 7) * 21_000 + (2_000_000_000u128 - 7) * 50_000;
        let base = 7u128 * 21_000 + 7u128 * 50_000;
        let l1 = 500u128 + 800u128;

        let coinbase = fee_vault::SEQUENCER_FEE_VAULT;
        let watched: std::collections::HashSet<Address> = [
            coinbase,
            fee_vault::BASE_FEE_VAULT,
            fee_vault::L1_FEE_VAULT,
        ]
        .into_iter()
        .collect();
        let system_signed = fee_vault_credits(&watched, coinbase, base_fee, &txs);

        // Observed per-vault balance delta == exactly the fee inflow the
        // op-revm handler applied as an in-EVM state write (no call frame).
        let account_deltas = vec![
            (coinbase, priority as i128),
            (fee_vault::BASE_FEE_VAULT, base as i128),
            (fee_vault::L1_FEE_VAULT, l1 as i128),
        ];

        let batch = process_block(BlockInputs {
            chain_id: 10,
            block_number: 42,
            block_hash: B256::repeat_byte(0xAB),
            watched: watched.clone(),
            erc20_logs: vec![],
            frames: vec![],
            gas: vec![],
            account_deltas,
            system_signed,
        })
        .expect("zero residual: fee-vault credits net the observed delta");

        // Three queryable kind=system rows, one per vault, no residual.
        assert_eq!(batch.eth.len(), 3);
        assert!(batch.unattributed.is_empty());

        // BEFORE the fix (system_signed empty) the SAME deltas stall.
        let stalled = process_block(BlockInputs {
            chain_id: 10,
            block_number: 42,
            block_hash: B256::repeat_byte(0xAB),
            watched,
            erc20_logs: vec![],
            frames: vec![],
            gas: vec![],
            account_deltas: vec![(fee_vault::BASE_FEE_VAULT, base as i128)],
            system_signed: vec![],
        });
        assert!(
            stalled.is_err(),
            "regression guard: an unrecognised fee-vault delta MUST residual-stall"
        );
    }

    #[test]
    fn op_chain_exec_binds_op_primitives_and_chainspec() {
        fn assert_chain_exec<C: ChainExec>() {}
        assert_chain_exec::<OpChainExec>();
        // Compile-time: the assoc types resolve to the OP set.
        fn same<T>(_: std::marker::PhantomData<T>) {}
        same::<<OpChainExec as ChainExec>::Primitives>(std::marker::PhantomData::<OpPrimitives>);
        same::<<OpChainExec as ChainExec>::ChainSpec>(std::marker::PhantomData::<OpChainSpec>);
    }

    /// The OP backend's per-block L1-fee table construction (the
    /// chain-specific logic Task 8.5 introduces) against a SYNTHETIC
    /// `l1_tx_data_fee`-shaped closure: deposits / the index-0 L1-info
    /// deposit tx must map to `None`; a non-deposit tx whose computed L1
    /// data fee is a real positive value maps to `Some(fee)`; a
    /// non-deposit tx whose fee computes to ZERO is recorded as `None`
    /// (Passbook records "not present", not `Some(0)` — the
    /// `.filter(|v| !v.is_zero())` rule in `process_committed_block`).
    #[test]
    fn op_l1_fee_table_deposit_and_zero_rules() {
        // (is_deposit, raw_2718). tx0 = L1-info deposit (idx 0).
        let txs = vec![
            (true, vec![0x7e, 0x00]),  // deposit ⇒ None
            (false, vec![0x02, 0xaa]), // normal, fee 1500 ⇒ Some
            (false, vec![0x02, 0x00]), // normal, fee 0      ⇒ None
            (true, vec![0x7e, 0x01]),  // another deposit    ⇒ None
        ];
        // Mirrors the op_chain.rs closure: l1_tx_data_fee(...).ok()
        // .filter(|v| !v.is_zero()). Deposits never reach this (the
        // table owns the deposit→None rule).
        let table = build_block_l1_fee_table(txs, |raw: &[u8]| {
            let fee = if raw == [0x02, 0xaa] {
                U256::from(1500u64)
            } else {
                U256::ZERO
            };
            Some(fee).filter(|v: &U256| !v.is_zero())
        });
        assert_eq!(table.l1_data_fee_wei(0), None, "L1-info deposit ⇒ None");
        assert_eq!(
            table.l1_data_fee_wei(1),
            Some(U256::from(1500u64)),
            "non-deposit positive fee ⇒ Some"
        );
        assert_eq!(
            table.l1_data_fee_wei(2),
            None,
            "non-deposit zero fee ⇒ None (not Some(0))"
        );
        assert_eq!(table.l1_data_fee_wei(3), None, "deposit ⇒ None");
        // Out-of-range tx index ⇒ None (defensive positional lookup).
        assert_eq!(table.l1_data_fee_wei(99), None);
    }
}
