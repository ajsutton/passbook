//! Chain-specific arms of the [`ChainExec`](crate::exex::ChainExec) seam.
//!
//! This module holds the **L1 (Ethereum)** implementation only — it needs
//! nothing beyond the `reth-ethereum` facade `passbook-core` already
//! depends on, so `passbook-core` stays entirely OP-free. The **OP**
//! implementation lives in `passbook-stack-optimism` (`OpChainExec`),
//! which depends on `reth-op`; both binaries select the right arm and
//! drive the SAME generic [`run_passbook`](crate::exex::run_passbook).
//!
//! `EthChainExec` is intentionally a thin delegator: the real L1 per-block
//! pipeline is the **unchanged** Task 6.4
//! [`process_committed_block_inner`](crate::exex::process_committed_block_inner)
//! (still public, still called verbatim by the 5 integration tests). The
//! seam therefore preserves L1 behaviour exactly while making the shared
//! [`run_passbook`] loop chain-generic.

use std::sync::Arc;

use crate::exex::ParentStateFn;
use reth_ethereum::chainspec::ChainSpec;
use reth_ethereum::primitives::RecoveredBlock;
use reth_ethereum::provider::Chain;
use reth_ethereum::{Block, EthPrimitives};

use crate::config::PassbookConfig;
use crate::exex::{process_committed_block_inner, ChainExec, ProcessingError};
use crate::ledger::writer::BlockBatch;
use crate::stack::StackAdapter;

/// L1 arm of the [`ChainExec`] seam.
///
/// Holds the per-block L1 [`StackAdapter`] factory (`make_adapter` — for
/// vanilla Ethereum this is `|| EthereumStack::default()`, which always
/// reports `None` for the OP L1 data fee). Stateless apart from that
/// closure, so a single instance is moved into `run_passbook`.
pub struct EthChainExec<S, F>
where
    S: StackAdapter,
    F: Fn() -> S + Send + Sync + 'static,
{
    make_adapter: F,
}

impl<S, F> EthChainExec<S, F>
where
    S: StackAdapter,
    F: Fn() -> S + Send + Sync + 'static,
{
    /// Build the L1 seam arm from a per-block adapter factory.
    pub fn new(make_adapter: F) -> Self {
        Self { make_adapter }
    }
}

impl<S, F> ChainExec for EthChainExec<S, F>
where
    S: StackAdapter,
    F: Fn() -> S + Send + Sync + 'static,
{
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;

    fn process_committed_block(
        &self,
        chain_id: u64,
        chain_spec: Arc<ChainSpec>,
        chain: &Chain<EthPrimitives>,
        block: &RecoveredBlock<Block>,
        cfg: &PassbookConfig,
        get_parent_state: &ParentStateFn<'_>,
    ) -> Result<BlockBatch, ProcessingError> {
        // Verbatim Task 6.4 L1 pipeline — behaviour-preserving.
        process_committed_block_inner(
            chain_id,
            chain_spec,
            chain,
            block,
            cfg,
            &(self.make_adapter)(),
            get_parent_state,
        )
    }
}

/// Ergonomic blanket arm: ANY `Fn() -> S` (S: [`StackAdapter`]) is itself
/// an L1 [`ChainExec`]. This keeps the established `make_adapter`
/// call-shape (`|| EthereumStack::default()`, `|| L1Adapter`) working
/// verbatim — the 5 Task 6.4/6.5 integration tests pass an
/// `|| L1Adapter` closure straight into `run_passbook` and remain
/// **unchanged** (proof the L1 path is behaviour-identical). The OP
/// binary instead supplies `OpChainExec`.
impl<S, F> ChainExec for F
where
    S: StackAdapter,
    F: Fn() -> S + Send + Sync + 'static,
{
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;

    fn process_committed_block(
        &self,
        chain_id: u64,
        chain_spec: Arc<ChainSpec>,
        chain: &Chain<EthPrimitives>,
        block: &RecoveredBlock<Block>,
        cfg: &PassbookConfig,
        get_parent_state: &ParentStateFn<'_>,
    ) -> Result<BlockBatch, ProcessingError> {
        process_committed_block_inner(
            chain_id,
            chain_spec,
            chain,
            block,
            cfg,
            &self(),
            get_parent_state,
        )
    }
}
