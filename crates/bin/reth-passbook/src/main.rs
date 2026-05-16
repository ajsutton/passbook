//! `reth-passbook` — the L1 (Ethereum) Passbook node binary.
//!
//! This is a **drop-in** reth Ethereum node: it is the stock
//! `reth_ethereum::cli::Cli` with one extra clap arg group
//! ([`PassbookArgs`], the `--passbook.addresses` / `--passbook.db-path`
//! flags from Task 8.3) layered on. Every reth subcommand / flag is
//! preserved verbatim — Passbook only adds behaviour, never removes any.
//!
//! Three precisely-specified modes (verified in Task 8.4):
//!
//! 1. **No / empty addresses** (`!cfg.enabled()`): launch a STOCK
//!    `EthereumNode` with **no** ExEx and **no** `passbook` RPC namespace —
//!    byte-for-byte the behaviour of upstream reth. Drop-in safety #1.
//! 2. **Malformed address**: `PassbookConfig::from_parts` returns `Err`,
//!    which is propagated out of the `run` closure ⇒ node startup ABORTS
//!    loudly (a watched-set typo must never silently degrade to "watch
//!    nothing"). Drop-in safety #2.
//! 3. **Valid addresses** (`cfg.enabled()`): open the durable ledger, build
//!    the single shared `Arc<Mutex<Ledger>>`, register the read-only
//!    `passbook` JSON-RPC namespace and install the Passbook ExEx writer —
//!    both sharing that exact same ledger handle. Drop-in safety #3.
//!
//! ## chain_id / `Ledger::open` ordering
//!
//! `Ledger::open(path, chain_id)` needs the chain id *before* the ExEx
//! context exists. The reth `builder` exposes the fully-resolved chain spec
//! at `builder.config().chain` (an `Arc<ChainSpec>`) *before* launch, and
//! `ChainSpec: EthChainSpec` provides `chain_id()` (the trait must be in
//! scope — see `docs/reth-pin.md`). We therefore resolve `chain_id` and
//! `Ledger::open` ONCE up front, on the main task, and move clones of the
//! resulting `Arc<Mutex<Ledger>>` into both the RPC hook and the ExEx
//! closure. The RPC reader and the ExEx writer thus share the SAME ledger
//! handle with no `OnceCell`/oneshot indirection.

use std::sync::{Arc, Mutex};

use clap::Parser;
use passbook_core::config::PassbookConfig;
use passbook_core::cli::PassbookArgs;
use passbook_core::exex::run_passbook;
use passbook_core::ledger::Ledger;
use passbook_core::rpc::{PassbookApiServer, PassbookRpc};
use passbook_stack_ethereum::EthereumStack;
use reth_ethereum::chainspec::EthChainSpec;
use reth_ethereum::cli::chainspec::EthereumChainSpecParser;
use reth_ethereum::cli::Cli;
use reth_ethereum::node::EthereumNode;

fn main() -> eyre::Result<()> {
    // The stock reth Ethereum CLI, parameterised with the standard
    // Ethereum chain-spec parser and our extra `PassbookArgs` arg group.
    // `PassbookArgs` is `#[derive(clap::Args)]`, so the `--passbook.*`
    // flags appear on the node command's `--help` exactly like native
    // reth flags (Task 8.3).
    Cli::<EthereumChainSpecParser, PassbookArgs>::parse().run(
        async move |builder, args: PassbookArgs| {
            // Parse + validate the watched set. A MALFORMED address makes
            // `from_parts` return Err; `?` propagates it out of this
            // closure and `Cli::run` returns that Err ⇒ the process exits
            // non-zero before any node starts. Loud failure by design — a
            // watched-set typo must never silently degrade to "watch
            // nothing". (Drop-in safety #2.)
            let cfg = PassbookConfig::from_parts(args.addresses.clone(), args.db_path.clone())?;

            // No / empty addresses ⇒ behave EXACTLY like stock reth: a
            // plain `EthereumNode`, no ExEx, no `passbook` RPC namespace.
            // (Drop-in safety #1.)
            if !cfg.enabled() {
                let handle = builder.node(EthereumNode::default()).launch().await?;
                return handle.wait_for_node_exit().await;
            }

            // Enabled: resolve the chain id from the builder's fully
            // configured chain spec (available BEFORE launch). `chain_id`
            // is needed by `Ledger::open` (which must run before the ExEx
            // context exists) and by `PassbookRpc`.
            let chain_id = builder.config().chain.chain_id();

            // Open the durable ledger ONCE and build the single shared
            // handle. Both the read-only RPC reader and the sole ExEx
            // writer use clones of THIS exact `Arc<Mutex<Ledger>>`.
            let ledger = Arc::new(Mutex::new(Ledger::open(&cfg.db_path, chain_id)?));

            let rpc_ledger = ledger.clone();
            let exex_cfg = cfg.clone();
            let exex_ledger = ledger.clone();

            let handle = builder
                .node(EthereumNode::default())
                // Read-only `passbook` JSON-RPC namespace (Task 7.1),
                // sharing the ledger handle. Registered ONLY when enabled,
                // so a stock node never exposes it. (Drop-in safety #3.)
                .extend_rpc_modules(move |ctx| {
                    ctx.modules.merge_configured(
                        PassbookApiServer::into_rpc(PassbookRpc {
                            ledger: rpc_ledger.clone(),
                            chain_id,
                        }),
                    )?;
                    Ok(())
                })
                // The Passbook ExEx writer. The `install_exex` closure
                // returns `Ok(fut)` where `fut` is the long-running
                // `run_passbook` future; `make_adapter` is the established
                // `|| EthereumStack::default()` (L1: a fresh stateless
                // adapter per block — see `exex.rs`).
                .install_exex("passbook", move |ctx| {
                    let cfg = exex_cfg.clone();
                    let ledger = exex_ledger.clone();
                    async move {
                        Ok(run_passbook(
                            ctx,
                            cfg,
                            ledger,
                            || EthereumStack::default(),
                        ))
                    }
                })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        },
    )
}
