//! `op-reth-passbook` — the OP-Stack (Optimism) Passbook node binary.
//!
//! This is a **drop-in** op-reth node: the stock `reth_op::cli::Cli` with
//! one extra clap arg group ([`PassbookArgs`], the `--passbook.addresses`
//! / `--passbook.db-path` flags from Task 8.3) layered on, AND op-reth's
//! own [`RollupArgs`] (`--rollup.*`) preserved verbatim. Every op-reth
//! subcommand / flag is preserved — Passbook only adds behaviour.
//!
//! It wires the SAME chain-agnostic capture core as the L1
//! `reth-passbook` binary: both call the single generic
//! [`run_passbook`](passbook_core::exex::run_passbook), differing ONLY in
//! the [`ChainExec`](passbook_core::exex::ChainExec) seam arm —
//! `EthChainExec` for L1, [`OpChainExec`] for OP. The reorg-first /
//! retry-until-success / FinishedHeight-only-after-durable-write safety
//! contract, the pure `process_block` reconciliation, the ledger and the
//! `ValueInspector` are all shared, never duplicated.
//!
//! Three precisely-specified modes (identical semantics to Task 8.4 L1):
//!
//! 1. **No / empty addresses** (`!cfg.enabled()`): launch a STOCK
//!    `OpNode` with **no** ExEx and **no** `passbook` RPC namespace —
//!    byte-for-byte upstream op-reth. Drop-in safety #1.
//! 2. **Malformed address**: `PassbookConfig::from_parts` returns `Err`,
//!    propagated out of the `run` closure ⇒ startup ABORTS loudly.
//!    Drop-in safety #2.
//! 3. **Valid addresses** (`cfg.enabled()`): open the durable ledger,
//!    build the single shared `Arc<Mutex<Ledger>>`, register the
//!    read-only `passbook` JSON-RPC namespace and install the Passbook
//!    ExEx writer (with the OP `ChainExec` arm) — both sharing that
//!    exact ledger handle. Drop-in safety #3.
//!
//! ## chain_id / `Ledger::open` ordering
//!
//! Identical to L1 (see `reth-passbook`): the op-reth `builder` exposes
//! the fully-resolved chain spec at `builder.config().chain`
//! (`Arc<OpChainSpec>`, which impls `EthChainSpec`) *before* launch, so
//! `chain_id` + `Ledger::open` resolve ONCE up front and one
//! `Arc<Mutex<Ledger>>` is shared by the RPC reader and the ExEx writer.

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[cfg(all(feature = "jemalloc", unix))]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

use std::sync::{Arc, Mutex};

use clap::Parser;
use passbook_core::cli::PassbookArgs;
use passbook_core::config::PassbookConfig;
use passbook_core::exex::run_passbook;
use passbook_core::ledger::Ledger;
use passbook_core::rpc::{PassbookApiServer, PassbookRpc};
use passbook_stack_optimism::OpChainExec;

use reth_op::chainspec::EthChainSpec;
use reth_op::cli::chainspec::OpChainSpecParser;
use reth_op::cli::Cli;
use reth_op::node::args::RollupArgs;
use reth_op::node::OpNode;

/// Combined Passbook + op-reth `Ext`.
///
/// The op-reth `Cli`'s default `Ext` is `RollupArgs`; flattening it here
/// (alongside `PassbookArgs`) keeps every `--rollup.*` flag working
/// exactly as stock op-reth while also surfacing `--passbook.*` on the
/// `node` subcommand.
#[derive(Debug, Clone, clap::Args)]
struct OpExt {
    #[command(flatten)]
    passbook: PassbookArgs,
    #[command(flatten)]
    rollup: RollupArgs,
}

fn main() -> eyre::Result<()> {
    // The stock op-reth CLI, parameterised with the OP chain-spec parser
    // and our combined `OpExt` arg group. The `--passbook.*` flags appear
    // on the node command's `--help` exactly like native op-reth flags;
    // `--rollup.*` are preserved via the flattened `RollupArgs`.
    Cli::<OpChainSpecParser, OpExt>::parse().run(async move |builder, ext: OpExt| {
        let rollup = ext.rollup.clone();

        // MALFORMED address ⇒ `from_parts` Err ⇒ `?` propagates ⇒
        // process exits non-zero before any node starts. (Drop-in
        // safety #2.)
        let cfg = PassbookConfig::from_parts(
            ext.passbook.addresses.clone(),
            ext.passbook.db_path.clone(),
        )?;

        // No / empty addresses ⇒ behave EXACTLY like stock op-reth:
        // a plain `OpNode`, no ExEx, no `passbook` RPC namespace.
        // (Drop-in safety #1.)
        if !cfg.enabled() {
            let handle = builder.node(OpNode::new(rollup)).launch().await?;
            return handle.wait_for_node_exit().await;
        }

        // Enabled: resolve chain id from the builder's fully
        // configured OP chain spec (available BEFORE launch;
        // `OpChainSpec: EthChainSpec` ⇒ `chain_id()`).
        let chain_id = builder.config().chain.chain_id();

        // Open the durable ledger ONCE; one shared handle for both
        // the read-only RPC reader and the sole ExEx writer.
        let ledger = Arc::new(Mutex::new(Ledger::open(&cfg.db_path, chain_id)?));

        let rpc_ledger = ledger.clone();
        let exex_cfg = cfg.clone();
        let exex_ledger = ledger.clone();

        let handle = builder
            .node(OpNode::new(rollup))
            // Read-only `passbook` JSON-RPC namespace (Task 7.1),
            // sharing the ledger handle. Registered ONLY when
            // enabled. (Drop-in safety #3.)
            .extend_rpc_modules(move |ctx| {
                ctx.modules
                    .merge_configured(PassbookApiServer::into_rpc(PassbookRpc {
                        ledger: rpc_ledger.clone(),
                        chain_id,
                    }))?;
                Ok(())
            })
            // The Passbook ExEx writer. The `install_exex` closure
            // returns `Ok(fut)` where `fut` is the long-running
            // `run_passbook` future. The chain-specific seam is the
            // OP arm `OpChainExec` (per-block `OptimismStack` via
            // `build_block_l1_fee_table` — see
            // `passbook-stack-optimism`). The SAME generic
            // `run_passbook` serves both this OP binary and the L1
            // binary; only the `ChainExec` arm differs.
            .install_exex("passbook", move |ctx| {
                let cfg = exex_cfg.clone();
                let ledger = exex_ledger.clone();
                async move { Ok(run_passbook(ctx, cfg, ledger, OpChainExec)) }
            })
            .launch()
            .await?;

        handle.wait_for_node_exit().await
    })
}
