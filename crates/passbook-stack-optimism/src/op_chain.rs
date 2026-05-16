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

        // ── (5) system_signed (OP adapter currently records none).
        let system_signed = l1_adapter.system_credits();

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
