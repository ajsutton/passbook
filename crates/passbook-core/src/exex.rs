use alloy_primitives::{Address, B256, U256};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use futures::TryStreamExt;
use reth_ethereum::exex::{ExExContext, ExExEvent};
use reth_ethereum::node::api::{FullNodeComponents, NodeTypes};
use reth_ethereum::chainspec::{EthChainSpec, ChainSpec};
use reth_ethereum::EthPrimitives;
use crate::config::PassbookConfig;
use crate::stack::StackAdapter;
use crate::ledger::Ledger;
use crate::ledger::writer::{delete_blocks, write_block, write_unattributed};
use crate::model::UnattributedDeltaRow;
use crate::erc20::{decode_transfer, RawLog};
use crate::inspector::CapturedFrame;
use crate::model::{Direction, Erc20TransferRow, GasPaymentRow};
use crate::reconcile::{reconcile_account, ReconcileInput};
use crate::ledger::writer::BlockBatch;

/// Backoff floor / ceiling for the retry-until-success writer loop.
const BACKOFF_START: Duration = Duration::from_millis(200);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

pub struct BlockInputs {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub watched: HashSet<Address>,
    pub erc20_logs: Vec<(Option<B256>, u64, RawLog)>,      // (tx_hash, log_index, log)
    pub frames: Vec<(Option<B256>, bool, CapturedFrame)>,  // (tx_hash, reverted, frame)
    pub gas: Vec<GasPaymentRow>,
    pub account_deltas: Vec<(Address, i128)>,              // watched accounts touched
    pub system_signed: Vec<(Address, i128)>,              // recognised system credits
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[allow(dead_code)] // constructed in Task 6.3
    #[error("erc20 decode failure at block {block}")]
    Decode { block: u64 },
    #[error("unexplained reconciliation residual for {address} at block {block}: {residual}")]
    UnexplainedResidual { block: u64, address: Address, residual: i128 },
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
                    chain_id: i.chain_id, block_number: i.block_number,
                    block_hash: i.block_hash,
                    tx_hash: tx.expect("erc20 log always in a tx"),
                    log_index: *log_index, token: d.token, from: d.from, to: d.to,
                    amount: d.amount, address: addr, direction: dir });
            }
        }
    }
    // (b) native frames
    let mut eth = Vec::new();
    let mut eth_in: std::collections::HashMap<Address, U256> = Default::default();
    let mut eth_out: std::collections::HashMap<Address, U256> = Default::default();
    for (tx, reverted, f) in &i.frames {
        let fr = [f.clone()];
        let rows = crate::attribution::attribute_eth_frames(
            i.chain_id, i.block_number, i.block_hash, *tx, *reverted, &fr, &i.watched);
        for r in &rows {
            match r.direction {
                Direction::In  => *eth_in.entry(r.address).or_default()  += r.amount_wei,
                Direction::Out => *eth_out.entry(r.address).or_default() += r.amount_wei,
            }
        }
        eth.extend(rows);
    }
    // gas per watched address
    let mut gas_paid: std::collections::HashMap<Address, U256> = Default::default();
    for g in &i.gas { *gas_paid.entry(g.address).or_default() += g.total_wei; }

    // (c) reconciliation — every touched watched address must balance
    let sys: std::collections::HashMap<Address, i128> =
        i.system_signed.iter().copied().collect();
    for (addr, observed) in &i.account_deltas {
        if !i.watched.contains(addr) { continue; }
        if let Some(_row) = reconcile_account(ReconcileInput {
            chain_id: i.chain_id, block_number: i.block_number,
            block_hash: i.block_hash, address: *addr, observed_delta: *observed,
            eth_in: eth_in.get(addr).copied().unwrap_or(U256::ZERO),
            eth_out: eth_out.get(addr).copied().unwrap_or(U256::ZERO),
            gas_paid: gas_paid.get(addr).copied().unwrap_or(U256::ZERO),
            system_signed: sys.get(addr).copied().unwrap_or(0),
        }) {
            return Err(ProcessingError::UnexplainedResidual {
                block: i.block_number, address: *addr, residual: *observed });
        }
    }
    Ok(BlockBatch {
        chain_id: i.chain_id, block_number: i.block_number, block_hash: i.block_hash,
        eth, erc20, gas: i.gas, unattributed: Vec::new() })
}

/// The Passbook ExEx driver. Node-generic: one body usable by both
/// `EthereumNode` and (later) `OpNode` via reth-ethereum's re-exported
/// upstream ExEx / node-api surface (proven by the spike compile-gate).
///
/// Durability contract:
/// - Reorgs are handled FIRST (delete reverted block rows).
/// - Each committed block is processed and durably written before the
///   next; an unexplained residual STALLS the loop (retry forever with
///   bounded exponential backoff) — the node never advances past a block
///   we cannot fully reconcile.
/// - `FinishedHeight` is emitted ONLY after every block in the committed
///   chain is durably written — never for an incomplete block.
pub async fn run_passbook<Node, S>(
    mut ctx: ExExContext<Node>,
    cfg: PassbookConfig,
    ledger: Arc<Mutex<Ledger>>,
    make_adapter: impl Fn() -> S + Send + Sync + 'static,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>,
    S: StackAdapter,
{
    let chain_id = ctx.config.chain.chain_id();

    while let Some(notification) = ctx.notifications.try_next().await? {
        // Reorg handling FIRST: drop every row for the reverted blocks.
        if let Some(reverted) = notification.reverted_chain() {
            let hashes: Vec<B256> =
                reverted.blocks_iter().map(|b| b.hash()).collect();
            delete_blocks(ledger.lock().unwrap().conn_mut(), chain_id, &hashes)?;
        }

        if let Some(chain) = notification.committed_chain() {
            for block in chain.blocks_iter() {
                let mut backoff = BACKOFF_START;
                loop {
                    match process_one_committed_block(
                        &ctx, chain.as_ref(), block, &cfg, &make_adapter,
                    )
                    .await
                    {
                        Ok(batch) => {
                            write_block(
                                ledger.lock().unwrap().conn_mut(), &batch,
                            )?;
                            break;
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
                            write_unattributed(
                                ledger.lock().unwrap().conn_mut(), &row,
                            )?;
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
            // Every block durably written ⇒ safe to advance.
            ctx.events
                .send(ExExEvent::FinishedHeight(chain.tip().num_hash()))?;
        }
    }
    Ok(())
}

/// Per-committed-block pipeline: assemble `BlockInputs` from the committed
/// block's execution (ERC20 logs + receipts, per-block BundleState deltas,
/// gas, and — gated on a watched balance/nonce change — `ValueInspector`
/// frames from a re-execution) and run the pure `process_block`. Wired and
/// asserted by integration test Task 6.4.
///
/// Re-execution runs against the **real historical post-state of the
/// committed chain's parent block** (a `reth` `StateProvider` obtained from
/// the node provider via `history_by_block_hash(chain.first().parent_hash)`
/// and wrapped in `StateProviderDatabase`), with the earlier in-chain
/// blocks' `BundleState` writes layered on top so block N re-executes
/// against `(parent-of-chain state + in-chain blocks < N)`. Crucially the
/// READ fallback always reaches real state (never `EmptyDB`): any account /
/// slot / contract code a production tx merely *reads* but the block does
/// not modify is served from the real parent provider, so re-execution
/// cannot diverge from canonical. This is pruning-independent and needs no
/// archive node — the parent of the just-committed tip is the previous
/// committed block, which `reth` keeps in plain/latest state on any full
/// node. The chain spec for the EVM is taken from `ctx.config.chain` (NOT
/// `ctx.evm_config()`, which is a panicking no-op under the ExEx test
/// harness — see docs/reth-pin.md, Task 6.4 section).
///
/// `chain` / `block` are the concrete reth v2.2.0 execution-types
/// `Chain` (`EthPrimitives`) and `RecoveredBlock<Block>` produced by
/// `notification.committed_chain()` / `chain.blocks_iter()`.
async fn process_one_committed_block<Node, S>(
    ctx: &ExExContext<Node>,
    chain: &reth_ethereum::provider::Chain,
    block: &reth_ethereum::primitives::RecoveredBlock<reth_ethereum::Block>,
    cfg: &PassbookConfig,
    make_adapter: &(impl Fn() -> S + Send + Sync + 'static),
) -> Result<BlockBatch, ProcessingError>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>,
    S: StackAdapter,
{
    use alloy_consensus::BlockHeader;
    use reth_ethereum::chainspec::EthChainSpec;
    use reth_ethereum::storage::StateProviderFactory;
    let chain_id = ctx.config.chain.chain_id();

    // Real historical post-state of the committed chain's parent block =
    // pre-state of the chain's first block. On a full node this is the
    // previous committed block, retained in plain/latest state (no archive
    // node, pruning-independent). The re-exec layers earlier in-chain
    // blocks' writes for blocks > the chain's first.
    let parent_hash = chain.first().header().parent_hash();
    let parent_state = ctx
        .provider()
        .history_by_block_hash(parent_hash)
        .map_err(|e| {
            tracing::error!(error = %e, %parent_hash, "no historical state at chain parent");
            ProcessingError::Decode {
                block: block.header().number(),
            }
        })?;

    process_committed_block_inner(
        chain_id,
        ctx.config.chain.clone(),
        chain,
        block,
        cfg,
        &make_adapter(),
        parent_state,
    )
}

/// Node-agnostic core of [`process_one_committed_block`]: takes the
/// resolved `chain_id` + chain spec + the **real parent-block state
/// provider** explicitly so it is unit-testable / integration-testable
/// without an `ExExContext` (and re-usable verbatim by Tasks 8.4/8.5).
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
    let outcome = chain
        .execution_outcome_at_block(block_number)
        .ok_or(ProcessingError::Decode { block: block_number })?;

    // ── (1) ERC20: receipts + logs for THIS block (always, no tracing).
    let mut erc20_logs: Vec<(Option<B256>, u64, RawLog)> = Vec::new();
    {
        let receipts = outcome.receipts_by_block(block_number);
        let mut log_index: u64 = 0;
        for (tx_idx, receipt) in receipts.iter().enumerate() {
            let tx_hash = block
                .body()
                .transactions
                .get(tx_idx)
                .map(|t| *t.tx_hash());
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
        let delta = i128::try_from(new_bal)
            .map_err(|_| ProcessingError::Decode { block: block_number })?
            - i128::try_from(old_bal)
                .map_err(|_| ProcessingError::Decode { block: block_number })?;
        account_deltas.push((addr, delta));
    }

    // ── (3) Gated re-execution → ValueInspector frames.
    let mut frames: Vec<(Option<B256>, bool, CapturedFrame)> = Vec::new();
    if any_watched_changed {
        let captured = reexec::reexecute_block_frames(
            chain_spec.clone(),
            chain,
            block,
            parent_state,
        )
        .map_err(|e| {
            tracing::error!(error = %e, block = block_number, "re-execution failed");
            ProcessingError::Decode { block: block_number }
        })?;
        // Tag top-level (depth-0 / call originating from a tx) frames so
        // attribution records kind=TopLevel; internal frames keep the
        // inspector's sequence path → kind=Internal.
        for cf in captured.frames {
            let tx_hash = captured
                .tx_hashes
                .get(cf.tx_index)
                .copied()
                .flatten();
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
            let gas_used = receipt
                .cumulative_gas_used
                .saturating_sub(prev_cumulative);
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

    // ── (5) system_signed: recognised non-call credits (L1: empty).
    let system_signed = adapter.system_credits();

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

/// Block re-execution against the **real parent-block state** for
/// `ValueInspector` frame capture.
///
/// The pre-block state is the historical post-state of the committed
/// chain's parent block — a `reth` `StateProvider` wrapped in
/// `StateProviderDatabase` — with the earlier in-chain blocks' `BundleState`
/// post-writes layered on top so block N re-executes against
/// `(parent-of-chain state + in-chain blocks < N)`. The CacheDB overlay
/// holds only the in-chain writes; every other account / slot / contract a
/// tx merely **reads** falls through to the real provider (never `EmptyDB`),
/// so re-execution cannot diverge from canonical (contract code a tx calls
/// but the block does not modify is present, etc).
///
/// Each transaction is replayed through the EVM's **per-transaction
/// inspector tracer** (`EvmFactoryExt::create_tracer` →
/// `TxTracer::try_trace_many`). This is the same primitive reth's own
/// `trace_block` uses; unlike the `BlockExecutor::execute_block` path
/// (whose `transact` does **not** drive inspector hooks for nested call
/// frames), the tracer's `transact` routes through revm's `inspect_run` so
/// every internal value-bearing CALL/CALLCODE/CREATE/CREATE2/SELFDESTRUCT
/// frame — including a plain value `CALL` to a codeless EOA — fires
/// `Inspector::call`/`create`/`selfdestruct` (see docs/reth-pin.md, Task
/// 6.4 section, "internal-frame capture").
mod reexec {
    use super::*;
    use alloy_consensus::BlockHeader;
    use reth_ethereum::evm::primitives::block::BlockExecutor;
    use reth_ethereum::evm::primitives::evm::EvmFactoryExt;
    use reth_ethereum::evm::primitives::tracing::TracingCtx;
    use reth_ethereum::evm::primitives::ConfigureEvm;
    use reth_ethereum::evm::EthEvmConfig;
    use reth_ethereum::evm::revm::database::StateProviderDatabase;
    use reth_ethereum::chainspec::ChainSpec;
    use reth_ethereum::storage::StateProviderBox;
    use revm::database::{CacheDB, State};
    use revm::state::AccountInfo;
    use std::sync::Arc;

    /// Pre-state database: real parent-block `StateProvider` (read
    /// fallthrough — never `EmptyDB`) with in-chain blocks' writes layered
    /// on top in the `CacheDB`.
    type PreStateDb = CacheDB<StateProviderDatabase<StateProviderBox>>;

    pub(super) struct TaggedFrame {
        pub frame: CapturedFrame,
        pub tx_index: usize,
        pub top_level: bool,
    }
    pub(super) struct Captured {
        pub frames: Vec<TaggedFrame>,
        pub tx_hashes: Vec<Option<B256>>,
        pub tx_reverted: Vec<bool>,
    }

    /// `ValueInspector` wrapper that marks each captured frame as
    /// top-level (the transaction's depth-0 call) or internal. Cloned/reset
    /// by the tracer between transactions, so call depth is per-tx.
    #[derive(Default, Clone)]
    struct TaggingInspector {
        inner: crate::inspector::ValueInspector,
        /// `top_level` flag parallel to inner-captured frames (this tx).
        tags: Vec<bool>,
        depth: i64,
    }

    impl TaggingInspector {
        #[inline]
        fn enter(&mut self) -> bool {
            let top_level = self.depth == 0;
            self.depth += 1;
            top_level
        }
        #[inline]
        fn record(&mut self, before: usize, top_level: bool) {
            if self.inner.frame_count() > before {
                self.tags.push(top_level);
            }
        }
    }

    impl<CTX> revm::Inspector<CTX> for TaggingInspector {
        fn call(
            &mut self,
            ctx: &mut CTX,
            inputs: &mut revm::interpreter::CallInputs,
        ) -> Option<revm::interpreter::CallOutcome> {
            let before = self.inner.frame_count();
            let top_level = self.enter();
            let out = self.inner.call(ctx, inputs);
            self.record(before, top_level);
            out
        }
        fn call_end(
            &mut self,
            _ctx: &mut CTX,
            _inputs: &revm::interpreter::CallInputs,
            _outcome: &mut revm::interpreter::CallOutcome,
        ) {
            self.depth -= 1;
        }
        fn create(
            &mut self,
            _ctx: &mut CTX,
            _inputs: &mut revm::interpreter::CreateInputs,
        ) -> Option<revm::interpreter::CreateOutcome> {
            self.enter();
            None
        }
        fn create_end(
            &mut self,
            ctx: &mut CTX,
            inputs: &revm::interpreter::CreateInputs,
            outcome: &mut revm::interpreter::CreateOutcome,
        ) {
            let before = self.inner.frame_count();
            let top_level = self.depth == 1;
            self.inner.create_end(ctx, inputs, outcome);
            self.record(before, top_level);
            self.depth -= 1;
        }
        fn selfdestruct(
            &mut self,
            contract: alloy_primitives::Address,
            target: alloy_primitives::Address,
            value: U256,
        ) {
            let before = self.inner.frame_count();
            <crate::inspector::ValueInspector as revm::Inspector<CTX>>::selfdestruct(
                &mut self.inner,
                contract,
                target,
                value,
            );
            self.record(before, false);
        }
    }

    pub(super) fn reexecute_block_frames(
        chain_spec: Arc<ChainSpec>,
        chain: &reth_ethereum::provider::Chain,
        block: &reth_ethereum::primitives::RecoveredBlock<reth_ethereum::Block>,
        parent_state: StateProviderBox,
    ) -> eyre::Result<Captured> {
        let block_number = block.header().number();

        // 1. Pre-block state = REAL post-state of the committed chain's
        //    parent block (the `parent_state` provider) wrapped so that any
        //    account / slot / contract code a tx merely READS but the block
        //    does not modify is served from real state — never `EmptyDB`.
        let mut cache: PreStateDb = CacheDB::new(StateProviderDatabase::new(parent_state));

        // 1a. Layer the earlier in-chain blocks' writes on top: for a
        //     multi-block committed `Chain`, block N must re-exec against
        //     (parent-of-chain state + in-chain blocks < N). The cumulative
        //     `BundleState` up to `block_number - 1` is exactly those
        //     writes; `execution_outcome_at_block` returns `None` when the
        //     current block is the chain's first (parent == real provider,
        //     no overlay needed).
        if block_number > 0 {
            if let Some(prior) = chain.execution_outcome_at_block(block_number - 1) {
                let bundle = prior.state();
                for (addr, acct) in bundle.state.iter() {
                    // Post-state account info (`acct.info`); pull bytecode
                    // out of `bundle.contracts` when only the hash is set.
                    if let Some(post) = &acct.info {
                        let mut info: AccountInfo = post.clone();
                        if info.code.is_none()
                            && info.code_hash != revm::primitives::KECCAK_EMPTY
                        {
                            if let Some(bc) = bundle.contracts.get(&info.code_hash) {
                                info.code = Some(bc.clone());
                            }
                        }
                        cache.insert_account_info(*addr, info);
                    }
                    // Post-state storage (`present_value`).
                    for (slot, sv) in acct.storage.iter() {
                        cache
                            .insert_account_storage(*addr, *slot, sv.present_value)
                            .ok();
                    }
                }
            }
        }

        // 2. Real EVM config from the chain spec (NOT the node's, which is
        //    a panicking no-op under the ExEx test harness).
        let evm_config = EthEvmConfig::new(chain_spec);
        let mut state = State::builder()
            .with_database(cache)
            .with_bundle_update()
            .build();

        let evm_env = evm_config.evm_env(block.header())?;

        // 2a. Apply chain-specific pre-execution system changes (e.g.
        //     Cancun beacon-root) so the replayed state matches consensus.
        {
            let exec_ctx = evm_config.context_for_block(block.sealed_block())?;
            let mut executor = evm_config.create_executor(
                evm_config.evm_with_env(&mut state, evm_env.clone()),
                exec_ctx,
            );
            executor.apply_pre_execution_changes()?;
        }

        // 3. Per-tx inspector tracer (drives nested-frame inspector hooks,
        //    unlike BlockExecutor::execute_block's plain `transact`).
        //    This-block receipts give per-tx reverted status.
        let this_outcome = chain
            .execution_outcome_at_block(block_number)
            .ok_or_else(|| eyre::eyre!("no execution outcome for block {block_number}"))?;
        let receipts = this_outcome.receipts_by_block(block_number);
        let mut frames: Vec<TaggedFrame> = Vec::new();
        let mut tx_hashes: Vec<Option<B256>> = Vec::new();
        let mut tx_reverted: Vec<bool> = Vec::new();
        for (i, tx) in block.body().transactions.iter().enumerate() {
            tx_hashes.push(Some(*tx.tx_hash()));
            tx_reverted.push(receipts.get(i).map(|r| !r.success).unwrap_or(false));
        }

        let mut tracer = evm_config.evm_factory().create_tracer(
            &mut state,
            evm_env,
            TaggingInspector::default(),
        );
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
                frames.push(TaggedFrame { frame, tx_index, top_level });
            }
        }
        Ok(Captured { frames, tx_hashes, tx_reverted })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};

    #[test]
    fn clean_block_produces_batch() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id:1, block_number:10, block_hash:B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![], frames: vec![], gas: vec![],
            account_deltas: vec![(w, 0i128)], system_signed: vec![],
        };
        let r = process_block(inp).expect("clean");
        assert_eq!(r.block_number, 10);
        assert!(r.unattributed.is_empty());
    }

    #[test]
    fn unexplained_residual_is_processing_error() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id:1, block_number:10, block_hash:B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![], frames: vec![], gas: vec![],
            account_deltas: vec![(w, 999i128)], system_signed: vec![],
        };
        let err = process_block(inp).unwrap_err();
        assert!(matches!(err, ProcessingError::UnexplainedResidual { .. }));
    }
}
