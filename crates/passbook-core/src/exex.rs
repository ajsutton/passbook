use crate::config::PassbookConfig;
use crate::erc20::{decode_transfer, RawLog};
use crate::inspector::CapturedFrame;
use crate::ledger::writer::BlockBatch;
use crate::ledger::writer::{delete_blocks, write_block, write_unattributed};
use crate::ledger::Ledger;
use crate::model::UnattributedDeltaRow;
use crate::model::{Direction, Erc20TransferRow, EthKind, EthTransferRow, GasPaymentRow};
use crate::reconcile::{reconcile_account, ReconcileInput};
use crate::stack::StackAdapter;
use crate::system::SystemCredit;
use alloy_primitives::{Address, B256, U256};
use futures::TryStreamExt;
use reth_ethereum::chainspec::{ChainSpec, EthChainSpec};
use reth_ethereum::exex::{ExExContext, ExExEvent};
use reth_ethereum::node::api::NodePrimitives;
use reth_ethereum::node::api::{FullNodeComponents, NodeTypes};
use reth_ethereum::primitives::RecoveredBlock;
use reth_ethereum::provider::Chain;
use reth_ethereum::storage::{StateProviderBox, StateProviderFactory};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Backoff floor / ceiling for the retry-until-success writer loop.
const BACKOFF_START: Duration = Duration::from_millis(200);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Lazily resolves the historical post-state of the committed chain's
/// parent block (pre-state of the chain's first block). A `ChainExec`
/// arm calls this ONLY inside the `any_watched_changed` gate — never for
/// the ~all blocks that touch no watched account — so a `--full` node
/// mid-pipeline is not stalled on historical state it does not need.
pub type ParentStateFn<'a> = dyn Fn() -> eyre::Result<StateProviderBox> + 'a;

pub struct BlockInputs {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub watched: HashSet<Address>,
    pub erc20_logs: Vec<(Option<B256>, u64, RawLog)>, // (tx_hash, log_index, log)
    pub frames: Vec<(Option<B256>, bool, CapturedFrame)>, // (tx_hash, reverted, frame)
    pub gas: Vec<GasPaymentRow>,
    pub account_deltas: Vec<(Address, i128)>, // watched accounts touched
    /// Recognised non-call system balance changes for watched addresses
    /// (L1 withdrawals/block-reward, OP deposit mints). Computed at the
    /// chain seam; netted into reconciliation AND recorded as
    /// `kind = system` `eth_transfers` rows.
    pub system_signed: Vec<SystemCredit>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[allow(dead_code)] // constructed in Task 6.3
    #[error("erc20 decode failure at block {block}")]
    Decode { block: u64 },
    #[error("unexplained reconciliation residual for {address} at block {block}: {residual}")]
    UnexplainedResidual {
        block: u64,
        address: Address,
        residual: i128,
    },
    /// A reconciliation input/sum exceeded `i128`. We refuse to clamp it to
    /// `i128::MAX` (which could mask a genuine discrepancy — issue #10);
    /// instead the block is failed exactly like an unexplained residual.
    #[error("reconcile arithmetic out of i128 range for {address} at block {block}")]
    ReconcileOverflow { block: u64, address: Address },
    /// The gated re-execution needed the parent block's historical
    /// post-state but the provider could not supply it (e.g. a `--full`
    /// node has pruned it, or the pipeline has not committed it yet).
    /// `run_passbook` treats this exactly like a transient write failure:
    /// stall (retry forever with bounded backoff), never advance.
    #[error("historical parent state unavailable at block {block}: {msg}")]
    ParentStateUnavailable { block: u64, msg: String },
}

/// Pure: deterministic transform of one block's inputs into a durable batch.
/// Any unexplained residual ⇒ Err (caller must NOT advance / emit FinishedHeight).
pub fn process_block(i: BlockInputs) -> Result<BlockBatch, ProcessingError> {
    // (a) ERC20
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
    // (b) native frames
    let mut eth = Vec::new();
    let mut eth_in: std::collections::HashMap<Address, U256> = Default::default();
    let mut eth_out: std::collections::HashMap<Address, U256> = Default::default();
    for (tx, reverted, f) in &i.frames {
        // Each frame carries its OWN (tx_hash, reverted), and
        // `attribute_eth_frames` takes a single tx_hash/reverted for the
        // whole slice, so we still attribute one frame at a time — but
        // borrow it as a 1-element slice (`from_ref`) instead of cloning
        // the `CapturedFrame` into a fresh array (issue #13).
        let rows = crate::attribution::attribute_eth_frames(
            i.chain_id,
            i.block_number,
            i.block_hash,
            *tx,
            *reverted,
            std::slice::from_ref(f),
            &i.watched,
        );
        for r in &rows {
            // Reverted movements never committed to the BundleState
            // (revm rolls them back) so they MUST NOT be summed into
            // reconciliation — counting them produces a spurious
            // residual and a permanent false stall on a valid block
            // (issue #2). The reverted-subtree inspector drop already
            // removes these frames at the source; this is a belt-and-
            // braces guard (issue #2, fix option (b)) for any frame
            // still flagged `reverted` (the row is still emitted with
            // `reverted = true` for the audit trail, just not counted).
            if *reverted {
                continue;
            }
            match r.direction {
                Direction::In => *eth_in.entry(r.address).or_default() += r.amount_wei,
                Direction::Out => *eth_out.entry(r.address).or_default() += r.amount_wei,
            }
        }
        eth.extend(rows);
    }
    // gas per watched address
    let mut gas_paid: std::collections::HashMap<Address, U256> = Default::default();
    for g in &i.gas {
        *gas_paid.entry(g.address).or_default() += g.total_wei;
    }

    // (b2) recognised system credits → kind=system eth_transfers rows
    //   (spec §(b)/(c): L1 withdrawals/block-reward, OP deposit mints).
    //   Also fold the signed total per address for reconciliation so a
    //   recognised system event produces ZERO residual (never a stall).
    let mut sys: std::collections::HashMap<Address, i128> = Default::default();
    for sc in &i.system_signed {
        if !i.watched.contains(&sc.address) {
            continue;
        }
        *sys.entry(sc.address).or_default() += sc.signed_wei;
        let (direction, amount) = if sc.signed_wei >= 0 {
            (Direction::In, sc.signed_wei.unsigned_abs())
        } else {
            (Direction::Out, sc.signed_wei.unsigned_abs())
        };
        // System events are block-scoped (no originating tx). The
        // `eth_transfers` natural PK is
        // `(chain_id, block_hash, tx_hash, trace_path)`; SQLite treats
        // NULLs in a PRIMARY KEY as DISTINCT, so a NULL `tx_hash` would
        // break the INSERT-OR-REPLACE idempotency the restart-resume
        // contract relies on. We therefore key the row's tx-slot by the
        // (unique-per-block) `block_hash` and disambiguate multiple
        // system rows in one block by the per-source/per-address
        // `trace_path` — stable across replays, collision-free.
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

    // (c) reconciliation — every touched watched address must balance
    for (addr, observed) in &i.account_deltas {
        if !i.watched.contains(addr) {
            continue;
        }
        match reconcile_account(ReconcileInput {
            chain_id: i.chain_id,
            block_number: i.block_number,
            block_hash: i.block_hash,
            address: *addr,
            observed_delta: *observed,
            eth_in: eth_in.get(addr).copied().unwrap_or(U256::ZERO),
            eth_out: eth_out.get(addr).copied().unwrap_or(U256::ZERO),
            gas_paid: gas_paid.get(addr).copied().unwrap_or(U256::ZERO),
            system_signed: sys.get(addr).copied().unwrap_or(0),
        }) {
            Ok(None) => {}
            Ok(Some(_row)) => {
                return Err(ProcessingError::UnexplainedResidual {
                    block: i.block_number,
                    address: *addr,
                    residual: *observed,
                });
            }
            Err(crate::reconcile::ReconcileOverflow) => {
                return Err(ProcessingError::ReconcileOverflow {
                    block: i.block_number,
                    address: *addr,
                });
            }
        }
    }
    Ok(BlockBatch {
        chain_id: i.chain_id,
        block_number: i.block_number,
        block_hash: i.block_hash,
        eth,
        erc20,
        gas: i.gas,
        unattributed: Vec::new(),
    })
}

/// The CHAIN-SPECIFIC seam.
///
/// Everything that differs L1 vs OP — the node primitives / chain-spec
/// types, which `ConfigureEvm` to build for re-execution, and how one
/// committed block's execution is extracted (receipts→ERC20 logs, bundle
/// account deltas, gas via `compute_gas_payment`, and the
/// `ValueInspector` frames from a re-exec against the real parent state),
/// including the per-block OP L1 data fee — is confined to a single
/// implementation of this trait. The chain-AGNOSTIC core
/// ([`process_block`], reconcile, ledger, the [`run_passbook`] safety
/// contract) stays shared and is invoked, never duplicated, by every
/// impl.
///
/// - L1: `passbook_core::chain::EthChainExec` (in this crate;
///   `reth-ethereum` only). Behaviour-identical to the Task 6.4 wiring —
///   it delegates to the unchanged [`process_committed_block_inner`].
/// - OP: `passbook_stack_optimism::OpChainExec` (depends on `reth-op`,
///   keeping `passbook-core` OP-free), building the per-block
///   `OptimismStack` via `build_block_l1_fee_table`.
///
/// `process_committed_block` returns a fully-formed [`BlockBatch`]: each
/// impl assembles the neutral `BlockInputs` for its chain and runs the
/// SHARED pure [`process_block`] orchestrator (invoked, not forked), so
/// reconciliation/attribution stay single-sourced.
pub trait ChainExec: Send + Sync + 'static {
    /// Node execution primitives (`EthPrimitives` / `OpPrimitives`).
    type Primitives: NodePrimitives;
    /// Node chain spec (`ChainSpec` / `OpChainSpec`); both impl
    /// `EthChainSpec` so `run_passbook` can read `chain_id()` generically.
    type ChainSpec: EthChainSpec + Send + Sync + 'static;

    /// Assemble + reconcile ONE committed block. `parent_state` is the
    /// real historical post-state of the committed chain's parent block
    /// (obtained generically by [`run_passbook`]); the impl wraps it for
    /// re-execution exactly as documented on
    /// [`process_committed_block_inner`].
    fn process_committed_block(
        &self,
        chain_id: u64,
        chain_spec: Arc<Self::ChainSpec>,
        chain: &Chain<Self::Primitives>,
        block: &RecoveredBlock<<Self::Primitives as NodePrimitives>::Block>,
        cfg: &PassbookConfig,
        parent_state: StateProviderBox,
    ) -> Result<BlockBatch, ProcessingError>;
}

/// The Passbook ExEx driver. Node-generic AND chain-generic: ONE body
/// usable by both `EthereumNode` and `OpNode` via the [`ChainExec`] seam
/// (`C`). The reorg-first delete / retry-until-success /
/// FinishedHeight-only-after-durable-write safety contract is identical
/// for every chain and lives ONLY here.
///
/// Durability contract:
/// - Reorgs are handled FIRST (delete reverted block rows).
/// - Each committed block is processed and durably written before the
///   next; an unexplained residual STALLS the loop (retry forever with
///   bounded exponential backoff) — the node never advances past a block
///   we cannot fully reconcile.
/// - `FinishedHeight` is emitted ONLY after every block in the committed
///   chain is durably written — never for an incomplete block.
pub async fn run_passbook<Node, C>(
    mut ctx: ExExContext<Node>,
    cfg: PassbookConfig,
    ledger: Arc<Mutex<Ledger>>,
    chain_exec: C,
) -> eyre::Result<()>
where
    C: ChainExec,
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = C::Primitives, ChainSpec = C::ChainSpec>,
{
    let chain_id = ctx.config.chain.chain_id();

    while let Some(notification) = ctx.notifications.try_next().await? {
        // Reorg handling FIRST: drop every row for the reverted blocks.
        // A DB failure here MUST NOT propagate out of the loop: doing so
        // ends the future AND leaves the reverted block's rows in the
        // ledger as orphaned/incorrect data (the spec's explicit
        // anti-requirement). Retry forever with bounded backoff — the
        // delete is idempotent, so replaying it is always safe.
        if let Some(reverted) = notification.reverted_chain() {
            let hashes: Vec<B256> = reverted.blocks_iter().map(|b| b.hash()).collect();
            let mut backoff = BACKOFF_START;
            loop {
                // Scope the MutexGuard so it is dropped BEFORE the await
                // below (a guard held across `.await` is `!Send` and would
                // also needlessly hold the ledger lock during the backoff).
                //
                // A POISONED mutex (some other holder — e.g. an RPC reader,
                // issue #8 — panicked) must NOT panic the writer: that would
                // crash the ExEx task with no retry/stall (spec lines
                // 211-213) and let an RPC failure kill block processing
                // (spec line 216). The single `Connection` is the only
                // shared state and a SQLite write is one atomic
                // `INSERT OR REPLACE`/`DELETE` transaction — a panicked
                // reader leaves no half-applied writer state — so the guard
                // is safely recovered (`into_inner`) and the loop continues
                // exactly as for any other transient fault.
                let res = delete_blocks(
                    ledger.lock().unwrap_or_else(|e| e.into_inner()).conn_mut(),
                    chain_id,
                    &hashes,
                );
                match res {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "ExEx reorg delete failed, retrying (not advancing)"
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(BACKOFF_CAP);
                    }
                }
            }
        }

        if let Some(chain) = notification.committed_chain() {
            for block in chain.blocks_iter() {
                let mut backoff = BACKOFF_START;
                loop {
                    // Real historical post-state of the committed chain's
                    // parent block = pre-state of the chain's first block.
                    // Obtained generically (Node::Provider: FullProvider:
                    // StateProviderFactory for ANY primitive set), then
                    // handed to the chain-specific seam.
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
                    match chain_exec.process_committed_block(
                        chain_id,
                        ctx.config.chain.clone(),
                        chain.as_ref(),
                        block,
                        &cfg,
                        parent_state,
                    ) {
                        Ok(batch) => {
                            // The durable write is the point of the whole
                            // loop. A transient SQLITE_BUSY past the 30s
                            // busy_timeout, disk-full, or an I/O error MUST
                            // stall (retry forever), never `?` out of the
                            // loop and terminate indexing. INSERT OR REPLACE
                            // on natural PKs makes the replay idempotent.
                            // Scope the MutexGuard so it is dropped BEFORE
                            // any await below (held-across-await is `!Send`).
                            // A poisoned mutex is recovered, never panicked
                            // on — see the reorg-delete site above (#8).
                            let res = write_block(
                                ledger.lock().unwrap_or_else(|e| e.into_inner()).conn_mut(),
                                &batch,
                            );
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
                        }
                        Err(ProcessingError::UnexplainedResidual {
                            block: bn,
                            address,
                            residual,
                        }) => {
                            let row = UnattributedDeltaRow {
                                chain_id,
                                block_number: bn,
                                block_hash: block.hash(),
                                address,
                                observed_wei: U256::from(residual.unsigned_abs()),
                                attributed_wei: U256::ZERO,
                                residual_wei: U256::from(residual.unsigned_abs()),
                            };
                            // Best-effort diagnostic row for the health
                            // query. A failure here must NOT `?` out of the
                            // loop (that would terminate indexing on the
                            // very block we are meant to be stalling on);
                            // just log and fall through to the same stall
                            // (sleep + retry) as the residual itself.
                            // Poisoned mutex recovered, not panicked (#8).
                            if let Err(e) = write_unattributed(
                                ledger.lock().unwrap_or_else(|e| e.into_inner()).conn_mut(),
                                &row,
                            ) {
                                tracing::error!(
                                    error = %e,
                                    block = bn,
                                    %address,
                                    "ExEx failed to write diagnostic residual row, retrying"
                                );
                            }
                            tracing::error!(
                                block = bn,
                                %address,
                                residual,
                                "ExEx stalled, not advancing"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(BACKOFF_CAP);
                            continue;
                        }
                        Err(other) => {
                            tracing::error!(
                                error = %other,
                                "ExEx block processing failed, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(BACKOFF_CAP);
                            continue;
                        }
                    }
                }
            }
            // Every block durably written ⇒ safe to advance. A send
            // failure here is NOT a durability concern (the batch is
            // already committed): per reth's ExEx contract a closed event
            // channel means the receiver — the node itself — is gone, so
            // there is nothing left to stall for. Returning ends this
            // future cleanly as the node shuts down.
            ctx.events
                .send(ExExEvent::FinishedHeight(chain.tip().num_hash()))?;
        }
    }
    Ok(())
}

/// Node-agnostic Ethereum per-committed-block pipeline: assemble
/// `BlockInputs` from the committed block's execution (ERC20 logs +
/// receipts, per-block BundleState deltas, gas, and — gated on a watched
/// balance/nonce change — `ValueInspector` frames from a re-execution)
/// and run the SHARED pure [`process_block`]. Wired and asserted by
/// integration test Task 6.4; invoked by
/// [`crate::chain::EthChainExec`] (Task 8.5 — the L1 arm of the
/// [`ChainExec`] seam). Behaviour is UNCHANGED from Task 6.4 — this is
/// the L1 implementation the seam delegates to verbatim.
///
/// Takes the resolved `chain_id` + chain spec + the **real parent-block
/// state provider** explicitly so it is unit-testable /
/// integration-testable without an `ExExContext`.
///
/// `parent_state` MUST be the historical post-state of the committed
/// chain's parent block (`chain.first().parent_hash`) — i.e. the pre-state
/// of the chain's first block. Re-execution wraps it in
/// `StateProviderDatabase` and layers in-chain blocks `< block.number` on
/// top; the READ fallback always reaches this real state, never `EmptyDB`.
#[doc(hidden)]
// Faithful per-block pipeline signature (resolved chain id + spec + parent
// state provider passed explicitly so it is testable without an
// ExExContext); grouping these into a struct would only obscure the call
// site.
#[allow(clippy::too_many_arguments)]
pub fn process_committed_block_inner<S: StackAdapter>(
    chain_id: u64,
    chain_spec: std::sync::Arc<ChainSpec>,
    chain: &reth_ethereum::provider::Chain,
    block: &reth_ethereum::primitives::RecoveredBlock<reth_ethereum::Block>,
    cfg: &PassbookConfig,
    adapter: &S,
    parent_state: reth_ethereum::storage::StateProviderBox,
) -> Result<BlockBatch, ProcessingError> {
    use alloy_consensus::{BlockHeader, Transaction};

    let block_number = block.header().number();
    let block_hash = block.hash();
    let base_fee = block.header().base_fee_per_gas();
    let watched = &cfg.watched;

    // Per-block execution outcome (the committed `Chain` bundle is a
    // MULTI-block aggregate; split to THIS block only so deltas/receipts
    // are for this block).
    let outcome =
        chain
            .execution_outcome_at_block(block_number)
            .ok_or(ProcessingError::Decode {
                block: block_number,
            })?;

    // ── (1) ERC20: receipts + logs for THIS block (always, no tracing).
    let mut erc20_logs: Vec<(Option<B256>, u64, RawLog)> = Vec::new();
    {
        let receipts = outcome.receipts_by_block(block_number);
        let mut log_index: u64 = 0;
        for (tx_idx, receipt) in receipts.iter().enumerate() {
            let tx_hash = block.body().transactions.get(tx_idx).map(|t| *t.tx_hash());
            for log in &receipt.logs {
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

    // ── (2) Gate: per-block BundleState old/new balances for watched.
    //   account_deltas := signed wei delta for every watched account that
    //   changed this block; `changed` gates the re-execution path.
    let mut account_deltas: Vec<(Address, i128)> = Vec::new();
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

    // ── (3) Gated re-execution → ValueInspector frames.
    let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
    if any_watched_changed {
        let captured =
            crate::reexec::reexecute_block_frames(chain_spec.clone(), chain, block, parent_state)
                .map_err(|e| {
                tracing::error!(error = %e, block = block_number, "re-execution failed");
                ProcessingError::Decode {
                    block: block_number,
                }
            })?;
        // Tag top-level (depth-0 / call originating from a tx) frames so
        // attribution records kind=TopLevel; internal frames keep the
        // inspector's sequence path → kind=Internal.
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

    // ── (4) Gas: per tx whose sender ∈ watched (even if reverted).
    let mut gas: Vec<GasPaymentRow> = Vec::new();
    {
        let receipts = outcome.receipts_by_block(block_number);
        let mut prev_cumulative: u64 = 0;
        for (tx_idx, (sender, tx)) in block.transactions_with_sender().enumerate() {
            let receipt = match receipts.get(tx_idx) {
                Some(r) => r,
                None => break,
            };
            let gas_used = receipt.cumulative_gas_used.saturating_sub(prev_cumulative);
            prev_cumulative = receipt.cumulative_gas_used;
            if !watched.contains(sender) {
                continue;
            }
            let effective_gas_price = tx.effective_gas_price(base_fee);
            let l1_fee = adapter.l1_data_fee_wei(tx_idx);
            gas.push(crate::attribution::compute_gas_payment(
                crate::attribution::GasInput {
                    chain_id,
                    block_number,
                    block_hash,
                    tx_hash: *tx.tx_hash(),
                    tx_from: *sender,
                    gas_used,
                    effective_gas_price,
                    l1_fee_wei: l1_fee,
                },
            ));
        }
    }

    // ── (5) system_signed: recognised non-call credits.
    //   L1: beacon-chain withdrawals (post-Shanghai `body().withdrawals`,
    //   amount GWEI→wei via `Withdrawal::amount_wei`) + the post-merge
    //   block "reward" = Σ priority fee credited to the block
    //   `beneficiary` (no captured CALL frame). Pre-merge fixed block
    //   rewards are out of scope (forward-only on post-merge networks).
    let system_signed = {
        let beneficiary = block.header().beneficiary();
        let base_fee_u128 = u128::from(base_fee.unwrap_or(0));

        // Withdrawals: post-Shanghai blocks carry `Withdrawals`; each
        // entry credits `address` with `amount` GWEI.
        let withdrawals: Vec<(Address, U256)> = block
            .body()
            .withdrawals
            .as_ref()
            .map(|ws| ws.iter().map(|w| (w.address, w.amount_wei())).collect())
            .unwrap_or_default();

        // Per-tx (effective_gas_price, gas_used) for the beneficiary
        // priority-fee sum (the post-merge block "reward").
        let mut tx_fees: Vec<(u128, u64)> = Vec::new();
        {
            let receipts = outcome.receipts_by_block(block_number);
            let mut prev_cumulative: u64 = 0;
            for (tx_idx, tx) in block.body().transactions.iter().enumerate() {
                let receipt = match receipts.get(tx_idx) {
                    Some(r) => r,
                    None => break,
                };
                let gas_used = receipt.cumulative_gas_used.saturating_sub(prev_cumulative);
                prev_cumulative = receipt.cumulative_gas_used;
                tx_fees.push((tx.effective_gas_price(base_fee), gas_used));
            }
        }

        crate::system::l1_system_credits(
            watched,
            &withdrawals,
            beneficiary,
            base_fee_u128,
            &tx_fees,
        )
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};

    #[test]
    fn clean_block_produces_batch() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id: 1,
            block_number: 10,
            block_hash: B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![],
            frames: vec![],
            gas: vec![],
            account_deltas: vec![(w, 0i128)],
            system_signed: vec![],
        };
        let r = process_block(inp).expect("clean");
        assert_eq!(r.block_number, 10);
        assert!(r.unattributed.is_empty());
    }

    /// Issue #4 (C3): a watched→watched ERC20 transfer must yield TWO
    /// erc20 rows in the batch (In for `to`, Out for `from`), each with
    /// distinct `address`. `process_block` is the producer the
    /// PK-collision bug truncated downstream.
    #[test]
    fn watched_to_watched_erc20_produces_two_rows() {
        use crate::erc20::{RawLog, TRANSFER_TOPIC0};
        let from = Address::repeat_byte(0xa1);
        let to = Address::repeat_byte(0xb2);
        let token = Address::repeat_byte(0x99);
        let topic_addr = |a: Address| {
            let mut b = [0u8; 32];
            b[12..].copy_from_slice(a.as_slice());
            B256::from(b)
        };
        let log = RawLog {
            address: token,
            topics: vec![TRANSFER_TOPIC0, topic_addr(from), topic_addr(to)],
            data: U256::from(777).to_be_bytes::<32>().to_vec().into(),
        };
        let inp = BlockInputs {
            chain_id: 1,
            block_number: 11,
            block_hash: B256::repeat_byte(3),
            watched: [from, to].into_iter().collect(),
            erc20_logs: vec![(Some(B256::repeat_byte(0x44)), 0, log)],
            frames: vec![],
            gas: vec![],
            account_deltas: vec![],
            system_signed: vec![],
        };
        let batch = process_block(inp).expect("clean");
        assert_eq!(batch.erc20.len(), 2, "both directional rows emitted");
        let mut dirs: Vec<(Address, Direction)> = batch
            .erc20
            .iter()
            .map(|r| (r.address, r.direction))
            .collect();
        dirs.sort_by_key(|(a, _)| *a);
        let mut expect = vec![(to, Direction::In), (from, Direction::Out)];
        expect.sort_by_key(|(a, _)| *a);
        assert_eq!(dirs, expect);
        // Same PK columns, different address — the exact collision case.
        assert_eq!(batch.erc20[0].tx_hash, batch.erc20[1].tx_hash);
        assert_eq!(batch.erc20[0].log_index, batch.erc20[1].log_index);
        assert_ne!(batch.erc20[0].address, batch.erc20[1].address);
    }

    /// Issue #6 (I2): two recognised system credits to the SAME watched
    /// address in one block (e.g. two OP deposit-mints) must yield TWO
    /// `eth_transfers` rows with DISTINCT `trace_path`. The rows share the
    /// natural PK columns `(chain_id, block_hash, tx_hash=block_hash)`, so
    /// a non-disambiguated `source` collapsed both into the same
    /// `trace_path` and `INSERT OR REPLACE` silently dropped one (spec
    /// line 193: never lose an entry; §(c): system rows queryable). With
    /// per-occurrence `source` tags the two trace_paths differ and both
    /// rows persist; reconciliation still nets the summed credit to zero.
    #[test]
    fn multiple_system_credits_same_address_distinct_trace_paths() {
        use crate::system::SystemCredit;
        let w = Address::repeat_byte(0xD7);
        let from = Address::repeat_byte(0xFF);
        let tx_a = B256::repeat_byte(0x01);
        let tx_b = B256::repeat_byte(0x02);
        let inp = BlockInputs {
            chain_id: 1,
            block_number: 42,
            block_hash: B256::repeat_byte(0x99),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![],
            frames: vec![],
            gas: vec![],
            // The watched address observes the summed +3000 credit.
            account_deltas: vec![(w, 3_000i128)],
            system_signed: vec![
                SystemCredit::new(w, 1_000, from, format!("deposit_mint:{tx_a:#x}")),
                SystemCredit::new(w, 2_000, from, format!("deposit_mint:{tx_b:#x}")),
            ],
        };
        let batch = process_block(inp).expect("recognised system credits net to zero");
        assert!(
            batch.unattributed.is_empty(),
            "summed system credit must reconcile to zero (no residual)"
        );
        let sys: Vec<&EthTransferRow> = batch
            .eth
            .iter()
            .filter(|r| matches!(r.kind, EthKind::System) && r.address == w)
            .collect();
        assert_eq!(sys.len(), 2, "both deposit-mint rows must be emitted (#6)");
        // Same PK tx-slot (block_hash), different trace_path — the exact
        // collision case the bug truncated under INSERT OR REPLACE.
        assert_eq!(sys[0].tx_hash, sys[1].tx_hash);
        assert_eq!(sys[0].tx_hash, Some(B256::repeat_byte(0x99)));
        assert_ne!(
            sys[0].trace_path, sys[1].trace_path,
            "distinct trace_path required so neither row is lost"
        );
        let mut tps: Vec<&str> = sys.iter().map(|r| r.trace_path.as_str()).collect();
        tps.sort_unstable();
        assert_eq!(
            tps,
            vec![
                format!("system:deposit_mint:{tx_a:#x}:{w:#x}"),
                format!("system:deposit_mint:{tx_b:#x}:{w:#x}"),
            ]
        );
    }

    /// Issue #8 (I4): a POISONED ledger mutex must NOT panic the writer.
    /// `run_passbook` accesses the shared `Mutex<Ledger>` exclusively via
    /// `lock().unwrap_or_else(|e| e.into_inner())` so a panicked OTHER
    /// holder (e.g. an RPC reader) cannot crash the ExEx task — the guard
    /// is recovered and the connection is still usable. Pre-fix the writer
    /// used `lock().unwrap()` which panics here.
    #[test]
    fn poisoned_ledger_lock_recovers_instead_of_panicking() {
        let m: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("holder panicked while holding the lock");
        })
        .join();
        assert!(m.lock().is_err(), "precondition: the mutex is poisoned");
        // The exact idiom run_passbook uses for every ledger access: it
        // must yield a usable guard, never panic, on a poisoned mutex.
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        *g += 1;
        assert_eq!(*g, 1, "recovered guard is fully usable");
    }

    #[test]
    fn unexplained_residual_is_processing_error() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id: 1,
            block_number: 10,
            block_hash: B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![],
            frames: vec![],
            gas: vec![],
            account_deltas: vec![(w, 999i128)],
            system_signed: vec![],
        };
        let err = process_block(inp).unwrap_err();
        assert!(matches!(err, ProcessingError::UnexplainedResidual { .. }));
    }

    /// Issue #10: an inflow whose magnitude exceeds `i128::MAX` must NOT be
    /// silently clamped to `i128::MAX` (which could collapse a genuine
    /// discrepancy into an apparently balanced block and let processing
    /// advance past an unreconciled state). `process_block` must surface it
    /// as a hard `ReconcileOverflow` so `run_passbook`'s catch-all arm
    /// stalls the block (no advance / no FinishedHeight), exactly like an
    /// unexplained residual.
    #[test]
    fn out_of_range_inflow_is_reconcile_overflow_not_silent_clamp() {
        use crate::inspector::{CapturedFrame, FrameKind};
        let w = Address::repeat_byte(0xcc);
        let other = Address::repeat_byte(0x11);
        // value = i128::MAX + 1: representable in U256, NOT in i128.
        let huge = U256::from(i128::MAX) + U256::from(1u8);
        let inp = BlockInputs {
            chain_id: 1,
            block_number: 11,
            block_hash: B256::repeat_byte(4),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![],
            frames: vec![(
                Some(B256::repeat_byte(7)),
                false, // not reverted ⇒ counted in reconciliation
                CapturedFrame {
                    from: other,
                    to: w,
                    value: huge,
                    kind: FrameKind::Call,
                    trace_path: "0".to_string(),
                },
            )],
            gas: vec![],
            // observed delta == the huge inflow; pre-fix the clamp made
            // attributed == i128::MAX, yielding a bogus residual / wrap
            // instead of an honest overflow signal.
            account_deltas: vec![(w, 0i128)],
            system_signed: vec![],
        };
        let err = process_block(inp).unwrap_err();
        assert!(
            matches!(err, ProcessingError::ReconcileOverflow { block: 11, address } if address == w),
            "out-of-range inflow must be a hard ReconcileOverflow, got {err:?}"
        );
    }

    #[test]
    fn parent_state_unavailable_is_processing_error() {
        let e = ProcessingError::ParentStateUnavailable {
            block: 42,
            msg: "pruned".to_string(),
        };
        assert!(matches!(e, ProcessingError::ParentStateUnavailable { block: 42, .. }));
        assert!(format!("{e}").contains("historical parent state unavailable at block 42"));
    }
}
