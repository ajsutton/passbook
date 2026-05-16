//! Shared block re-execution machinery for `ValueInspector` frame capture.
//!
//! This module is the SINGLE source of the value-attribution inspector
//! ([`crate::inspector::ValueInspector`] + the [`TaggingInspector`]
//! top-level/internal wrapper) and the parent-state pre-state overlay.
//! Both the L1 (`reth-ethereum`) and OP (`reth-op`) arms of the
//! [`ChainExec`](crate::exex::ChainExec) seam reuse these — the inspector
//! is **never duplicated/forked**. Only the thin per-chain tracer driver
//! differs, because the concrete `ConfigureEvm` (`EthEvmConfig` vs
//! `OpEvmConfig`) and the receipt/tx field accessors are genuinely
//! different types L1 vs OP and cannot be unified at this reth version
//! without naming both EVM stacks (which would force `reth-op` into
//! `passbook-core`, breaking the OP-free boundary).
//!
//! The shared, generic pieces:
//! - [`TaggingInspector`] — the EVM-`CTX`-generic `revm::Inspector`
//!   wrapper that marks each captured frame top-level vs internal.
//! - [`Captured`] / [`TaggedFrame`] — the neutral capture result.
//! - [`build_prestate_cache`] — the parent-state + in-chain-overlay
//!   `CacheDB` builder, generic over `N: NodePrimitives` (the overlay
//!   reads only the primitive-agnostic `BundleState`), so the L1 and OP
//!   drivers share the exact same pre-state semantics (no `EmptyDB`
//!   fallback — see docs/reth-pin.md, Task 6.4 "parent-state
//!   re-execution").
//!
//! The L1 driver [`reexecute_block_frames`] lives here (behaviour
//! identical to Task 6.4). The OP driver lives in
//! `passbook-stack-optimism`, reusing all of the above verbatim.

use alloy_primitives::{B256, U256};
use reth_ethereum::node::api::NodePrimitives;
use reth_ethereum::provider::Chain;
use reth_ethereum::storage::StateProviderBox;
use reth_ethereum::evm::revm::database::StateProviderDatabase;
use revm::database::CacheDB;
use revm::state::AccountInfo;

use crate::inspector::CapturedFrame;

/// Pre-state database: real parent-block `StateProvider` (read
/// fallthrough — **never** `EmptyDB`) with in-chain blocks' writes
/// layered on top in the `CacheDB`. Shared by the L1 and OP drivers.
pub type PreStateDb = CacheDB<StateProviderDatabase<StateProviderBox>>;

/// One captured value-bearing frame, tagged with its originating tx and
/// whether it is the tx's top-level call (vs an internal frame).
pub struct TaggedFrame {
    pub frame: CapturedFrame,
    pub tx_index: usize,
    pub top_level: bool,
}

/// Per-block capture result: the tagged frames plus per-tx hash / revert
/// status (parallel to block tx order).
pub struct Captured {
    pub frames: Vec<TaggedFrame>,
    pub tx_hashes: Vec<Option<B256>>,
    pub tx_reverted: Vec<bool>,
}

/// `ValueInspector` wrapper that marks each captured frame as top-level
/// (the transaction's depth-0 call) or internal. Cloned/reset by the
/// tracer between transactions, so call depth is per-tx. EVM-`CTX`
/// generic — identical behaviour for the L1 and OP EVMs.
#[derive(Default, Clone)]
pub struct TaggingInspector {
    pub inner: crate::inspector::ValueInspector,
    /// `top_level` flag parallel to inner-captured frames (this tx).
    pub tags: Vec<bool>,
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

/// Build the re-execution pre-state `CacheDB`: the REAL post-state of the
/// committed chain's parent block (`parent_state`, read fallthrough —
/// never `EmptyDB`) with the cumulative in-chain `BundleState` writes for
/// blocks `< block_number` layered on top, so block N re-executes against
/// `(parent-of-chain state + in-chain blocks < N)`.
///
/// Generic over `N: NodePrimitives` — the overlay touches only the
/// primitive-agnostic `BundleState` (`acct.info`, slot `present_value`,
/// `bundle.contracts`), so the L1 and OP drivers share IDENTICAL
/// pre-state semantics.
pub fn build_prestate_cache<N>(
    chain: &Chain<N>,
    block_number: u64,
    parent_state: StateProviderBox,
) -> PreStateDb
where
    N: NodePrimitives,
{
    let mut cache: PreStateDb =
        CacheDB::new(StateProviderDatabase::new(parent_state));

    if block_number > 0 {
        if let Some(prior) = chain.execution_outcome_at_block(block_number - 1)
        {
            let bundle = prior.state();
            for (addr, acct) in bundle.state.iter() {
                if let Some(post) = &acct.info {
                    let mut info: AccountInfo = post.clone();
                    if info.code.is_none()
                        && info.code_hash != revm::primitives::KECCAK_EMPTY
                    {
                        if let Some(bc) = bundle.contracts.get(&info.code_hash)
                        {
                            info.code = Some(bc.clone());
                        }
                    }
                    cache.insert_account_info(*addr, info);
                }
                for (slot, sv) in acct.storage.iter() {
                    cache
                        .insert_account_storage(
                            *addr,
                            *slot,
                            sv.present_value,
                        )
                        .ok();
                }
            }
        }
    }
    cache
}

/// L1 (Ethereum) re-execution driver. Behaviour identical to Task 6.4;
/// it builds an `EthEvmConfig` from the chain spec and replays every tx
/// through the per-tx inspector tracer (drives nested-frame inspector
/// hooks, unlike `BlockExecutor::execute_block`'s plain `transact` — see
/// docs/reth-pin.md, Task 6.4, "internal-frame capture"). The OP driver
/// in `passbook-stack-optimism` is the structurally-identical analogue
/// with `OpEvmConfig` + OP receipt/tx accessors, reusing
/// [`TaggingInspector`] / [`build_prestate_cache`] verbatim.
pub fn reexecute_block_frames(
    chain_spec: std::sync::Arc<reth_ethereum::chainspec::ChainSpec>,
    chain: &Chain<reth_ethereum::EthPrimitives>,
    block: &reth_ethereum::primitives::RecoveredBlock<reth_ethereum::Block>,
    parent_state: StateProviderBox,
) -> eyre::Result<Captured> {
    use alloy_consensus::BlockHeader;
    use reth_ethereum::evm::primitives::block::BlockExecutor;
    use reth_ethereum::evm::primitives::evm::EvmFactoryExt;
    use reth_ethereum::evm::primitives::tracing::TracingCtx;
    use reth_ethereum::evm::primitives::ConfigureEvm;
    use reth_ethereum::evm::EthEvmConfig;
    use revm::database::State;

    let block_number = block.header().number();

    // 1. Pre-block state = REAL parent post-state + in-chain overlay
    //    (shared, primitive-agnostic builder).
    let cache = build_prestate_cache(chain, block_number, parent_state);

    // 2. Real EVM config from the chain spec (NOT the node's, which is a
    //    panicking no-op under the ExEx test harness).
    let evm_config = EthEvmConfig::new(chain_spec);
    let mut state = State::builder()
        .with_database(cache)
        .with_bundle_update()
        .build();

    let evm_env = evm_config.evm_env(block.header())?;

    // 2a. Apply chain-specific pre-execution system changes (e.g. Cancun
    //     beacon-root) so the replayed state matches consensus.
    {
        let exec_ctx = evm_config.context_for_block(block.sealed_block())?;
        let mut executor = evm_config.create_executor(
            evm_config.evm_with_env(&mut state, evm_env.clone()),
            exec_ctx,
        );
        executor.apply_pre_execution_changes()?;
    }

    // 3. Per-tx inspector tracer. This-block receipts give per-tx
    //    reverted status.
    let this_outcome = chain
        .execution_outcome_at_block(block_number)
        .ok_or_else(|| {
            eyre::eyre!("no execution outcome for block {block_number}")
        })?;
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
                        .map(|(k, f)| {
                            (f, tags.get(k).copied().unwrap_or(false))
                        })
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
