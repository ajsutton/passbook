# Passbook

Passbook is a [Reth](https://github.com/paradigmxyz/reth) **Execution
Extension (ExEx)** that performs forward-only capture, for a small static set
of watched addresses, of:

- **ERC20 transfers** in/out (any contract emitting the standard `Transfer`
  event — no token allowlist),
- **Native ETH transfers** in/out, **including value moved by internal
  calls** (`CALL`/`CALLCODE`/`CREATE`/`CREATE2`/`SELFDESTRUCT`),
- **Gas fees** paid (L2 execution fee, plus the OP L1 data fee where
  applicable),

into a durable, queryable **SQLite ledger** with a strict reconciliation
**completeness property**: for every touched watched address, the observed
post-execution balance delta must equal the sum of everything attributed
(top-level + internal + gas + recognised system credits). Any unexplained
residual is treated as a processing failure, not silently flagged.

It runs **one chain per node instance** and captures **forward only**, from
the block at which the ExEx first runs. There is no historical backfill and
no archive-node dependency.

This is a standalone Cargo workspace that produces two Docker images. How
those images are deployed and configured is the operator's concern and out of
scope here. See [`passbook-spec.md`](passbook-spec.md) for the full design and
[`docs/reth-pin.md`](docs/reth-pin.md) for the upstream pin rationale.

## Two images, one workspace

One multi-stage `Dockerfile` builds **both** final images from the same
workspace:

| Image | Stack | Build |
|-------|-------|-------|
| `reth-passbook` | L1 (Ethereum), stock `reth` node | `docker build --target reth-passbook .` |
| `op-reth-passbook` | OP-Stack (Optimism), stock `op-reth` node | `docker build --target op-reth-passbook .` |

`make docker` builds both (`reth-passbook:dev` / `op-reth-passbook:dev`).

The capture core is shared. Everything that differs L1-vs-OP — node
primitives / chain-spec types, the EVM config used for the gated
re-execution, receipt/bundle extraction, and the per-block OP L1 data fee —
is confined to a single `ChainExec` trait implementation per stack. The
generic ExEx driver (`run_passbook`) and the pure per-block pipeline
(`process_block`, reconciliation, ledger writes) are written once and invoked
by both binaries; only the `ChainExec` arm differs. `passbook-core` is
node-generic and **OP-free** (it never depends on `reth-op`); the OP-specific
adapter lives in `passbook-stack-optimism`.

Image tagging (e.g. `<upstream-version>-passbook<N>`) is a deploy concern and
is not enforced by this repo.

## Drop-in safety

The image is a **safe drop-in** for a stock reth / op-reth node. Behaviour is
driven entirely by whether watched addresses are supplied:

| `--passbook.addresses` | Result |
|------------------------|--------|
| absent / empty | **Stock node.** No Passbook ExEx, no `passbook` RPC namespace — byte-for-byte upstream `reth` / `op-reth`. |
| any address malformed | **Node startup aborts** loudly (non-zero exit, before any node starts). A watched-set typo must never silently degrade to "watch nothing". |
| one or more valid addresses | Passbook ExEx writer **and** the read-only `passbook` RPC namespace are both active, sharing one ledger handle. |

Every native reth / op-reth subcommand and flag is preserved verbatim —
Passbook only *adds* behaviour. On the OP binary, op-reth's own `--rollup.*`
flags are preserved too.

### Configuration

No config file. Flags live on the **`node` subcommand**:

| Flag | Env | Default | Notes |
|------|-----|---------|-------|
| `--passbook.addresses` | `PASSBOOK_ADDRESSES` | (empty) | Comma-separated list, ≤10 expected (a larger set warns but is not rejected). |
| `--passbook.db-path` | `PASSBOOK_DB_PATH` | `/data/passbook.db` | SQLite ledger path. |

Changing the watched set = change the flag and **restart the node**. There is
no hot reload (rare-change assumption).

## Guarantees

**Absolute rule: never lose or skip an entry.** Block processing is atomic
and strictly ordered — a block is either fully captured and durably
committed, or the ExEx does not advance past it. There is no skip-and-flag
path anywhere.

- A block is "done" only when **all** of: the ERC20 scan, the gated
  native/gas attribution, reconciliation, and the durable SQLite transaction
  have succeeded.
- **Any** failure in any step (log/ABI decode, tracer/re-execution, missing
  parent state, an unexplained reconciliation residual, or a DB write error)
  ⇒ the ExEx **STALLS on that block**: it retries forever with bounded
  exponential backoff (200 ms → 30 s cap). It never advances, never skips,
  and never emits `ExExEvent::FinishedHeight` for an incomplete block.
- An unexplained residual additionally writes an `unattributed_deltas`
  diagnostic row recording *why* processing halted, and is surfaced loudly
  via error logs and the `passbook_health` RPC (which stops advancing its
  reported last block). A deterministic failure (e.g. a decode bug) therefore
  halts indexing progress until fixed — "can't process" means "don't
  proceed". The fix is a **code change + redeploy**, after which processing
  resumes **exactly where it stalled** — no gap, no duplicate.
- **Recognized system events** (no captured call frame) are attributed
  `kind = system` and produce **no reconciliation residual** (so they do
  not stall): **L1 beacon-chain withdrawals**, the **post-merge block
  reward** (the beneficiary/coinbase priority-fee credit, = Σ
  `(effective_gas_price − base_fee) × gas_used`), **OP deposit
  mints**, and the **OP fee-vault** per-block credits (SequencerFeeVault
  = coinbase priority fee, BaseFeeVault = Σ `base_fee × gas_used`,
  L1FeeVault = Σ per-tx L1 data cost — reconstructed from the same per-tx
  fee data Passbook already gathers, mirroring the L1 block-reward
  derivation). They are recorded as queryable `kind=system`
  `eth_transfers` rows, so a watched OP fee-vault predeploy now nets to
  zero residual instead of stalling. **Narrow remaining gap:** the
  Isthmus *operator-fee* vault (`0x..1b`, defaults to zero on
  effectively all chains) is not reconstructable without re-running the
  EVM at the pinned reth-op API (see `docs/reth-pin.md`); a watched
  operator-fee vault on a chain with a non-zero operator-fee scalar would
  still fail-closed (stall, not silent loss). Pre-merge fixed block
  rewards are out of scope (forward-only on post-merge networks).
- The ExEx halts by *stalling* (retrying the current block), not by crashing
  the node. The node keeps running so RPC stays available, the ExEx applies
  natural backpressure, and reth will not prune below the stalled height.
- **Reorg-safe**: a reverted chain deletes its rows first, keyed by block
  hash, before the new chain is applied. Rows are idempotently keyed.
- **Restart-safe**: `FinishedHeight` is emitted only after a block's rows are
  durably committed, so a restart resumes with no gap and no duplicate.
- The ledger is a **separate SQLite store** (opened WAL,
  `synchronous=FULL`), **never reth's own MDBX environment** — Passbook is
  not coupled to reth storage internals.

## Query API

When the ExEx is active, Passbook serves a custom **`passbook` JSON-RPC
namespace** on reth's **existing RPC server** (no separate port; merged onto
the node's configured RPC transports). It is registered exactly when the ExEx
is active and absent on a stock node. The reader shares a read-only handle to
the same ledger the ExEx writer owns; it is fully independent of the writer
and cannot affect block processing.

| Method | Params | Returns |
|--------|--------|---------|
| `passbook_health` | — | `{ last_block, chain_id }` (last durably processed block; a stall makes `last_block` stop advancing). |
| `passbook_getTransfers` | `{ address, fromBlock?, toBlock?, kind?, cursor? }` | A **block-complete**, paginated stream of rows over the `eth`, `erc20`, `gas`, and `unattributed` categories, ordered by `(block_number, category)`. `kind` (one of `eth` / `erc20` / `gas` / `unattributed`) restricts to one category; an unknown value yields an empty page. |

`passbook_getTransfers` pagination is **block-complete**: a block is never
split across a page boundary; following `next_cursor` until it is `null`
yields every matching row exactly once (no skip, no dup). Callers derive
their own totals/exports from these rows. RPC errors (e.g. a poisoned lock or
a query failure) are returned as JSON-RPC errors (code `-32000`) — never
swallowed, never a silently-empty result.

## Build / development

### Prerequisites

- Rust toolchain pinned to **1.95.0** via `rust-toolchain.toml`
  (`rustfmt` + `clippy` components).
- `git` and network access (the bootstrap shallow-clones an upstream mirror).
- A working C toolchain (for the bundled SQLite build used by `rusqlite`).

### Bootstrap (required before any cargo build/test)

op-reth is not yet on crates.io, so the OP facade is a git dependency into
the `ethereum-optimism/optimism` monorepo. The minimal-checkout fast path
must be run **once** on any fresh machine / CI runner before any
`cargo build` / `cargo test`:

```sh
bash scripts/seed-vendor.sh      # or: make seed
```

This creates the **gitignored** shallow (`depth-1`) `.vendor/optimism` mirror
at the pinned optimism rev and writes the **gitignored**
`.cargo/config.toml` (machine-local, with an absolute path) that source-
replaces the optimism git source onto that mirror. Nothing it writes is
committed. A clean checkout that skips this step still builds correctly —
cargo just clones the full multi-GB monorepo from the remote (slow); the
mirror is purely an optimization.

All cargo invocations are `--locked` (`Cargo.lock` is committed and
load-bearing) and set `CARGO_NET_GIT_FETCH_WITH_CLI=true` (the system git
binary handles the shallow mirror and the `paradigmxyz/reth` git dependency;
cargo's libgit2 mishandles shallow clones).

### Make targets

```sh
make seed          # (re)create the gitignored .vendor mirror + .cargo config
make build         # cargo build --workspace --locked
make test          # cargo test  --workspace --locked
make verify-pin    # re-seed + the `spike` co-resolution gate (post-bump check)
make bump ARGS='--optimism-rev <SHA> --reth-rev <SHA>'   # lockstep pin bump
make docker        # build both binary images
make help          # list targets
```

The Docker build is **hermetic**: its build stage runs
`scripts/seed-vendor.sh` itself (re-creating the mirror and config *inside*
the image), so the image is reproducible from committed source and never
copies host-local state. A long first build is expected — the reth / op-reth
git compile is very large.

## Upgrading reth / op-reth

The upstream pin is a **lockstep pair** that must move together:

- the `ethereum-optimism/optimism` monorepo rev (drives `reth-op` /
  `reth-optimism-evm` and the `OPTIMISM_REV` in `scripts/seed-vendor.sh`),
  and
- the matching `paradigmxyz/reth` rev (drives `reth-ethereum` /
  `reth-exex-test-utils`) — which **must equal** the `paradigmxyz/reth` rev
  that the chosen optimism monorepo pins all its upstream reth crates to. If
  the two drift, co-resolution breaks.

Bump with:

```sh
scripts/bump-reth.sh --optimism-rev <SHA> --reth-rev <SHA>   # or: make bump ARGS='...'
make verify-pin
```

`bump-reth.sh` rewrites both revs in every committed file, re-seeds the
mirror, regenerates `Cargo.lock`, runs the `spike` co-resolution gate, and
**never commits** (the operator reviews the working tree). Run `--help` (or
pass only `--optimism-rev`) and it prints how to find the matching
`--reth-rev` from the monorepo's `rust/Cargo.toml`. The full rationale,
locked source table, and recovery steps are in
[`docs/reth-pin.md`](docs/reth-pin.md).

> When `reth-op` is published to crates.io, migrate to the published crate
> and drop `.vendor/`, the generated `.cargo/config.toml`, and
> `scripts/seed-vendor.sh`.

## Contributing

Code must stay clean under the exact gates CI enforces (see
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)). Run, after `make
seed`:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

CI also builds both Docker images and smoke-tests that `node --help` exposes
`passbook.addresses` and that the stock reth subcommands survive.

## Repository layout

| Path | Responsibility |
|------|----------------|
| `crates/passbook-core` | Node-generic capture logic: ERC20 decode, value-only revm inspector, gas/eth attribution, reconciliation, the SQLite ledger, the `passbook` RPC namespace, the `ChainExec` seam, and the generic `run_passbook` ExEx driver. No binary; OP-free. |
| `crates/passbook-stack-ethereum` | The L1 `ChainExec` adapter (`reth-ethereum` only; no L1 data fee). |
| `crates/passbook-stack-optimism` | The OP `ChainExec` adapter (`reth-op`; per-tx L1 data fee table). |
| `crates/bin/reth-passbook` | L1 binary: stock `reth` Ethereum CLI + Passbook. |
| `crates/bin/op-reth-passbook` | OP binary: stock `op-reth` CLI + `--rollup.*` + Passbook. |
| `crates/spike` | Compile-only co-resolution probe: proves the L1 and OP facades resolve in one workspace and that one generic ExEx signature type-checks for both node types. The post-bump gate. |
| `passbook-spec.md` | Full design specification. |
| `docs/reth-pin.md` | Upstream pin: locked sources, lockstep bump procedure, minimal-checkout mechanism, verified facade module paths. |

## Status / known limitations

- The capture core, ledger, RPC, both binaries, the multi-stage Dockerfile,
  and CI are implemented. Correctness is covered by unit tests plus
  integration tests using reth's `reth-exex-test-utils` harness over
  *genuinely executed* synthetic chains, asserting ledger rows and a zero
  reconciliation residual, plus fault-injection (stall), reorg
  (replace/no-dup), and restart-resume scenarios.
- **Live OP end-to-end testing is not performed in this repo.** The
  integration tests exercise the shared core and the L1 wiring; the OP
  adapter is unit-tested and the OP binary is build- and smoke-tested
  (image builds, `node --help`), but a full live op-reth sync has not been
  validated at the current pin. The L1-vs-OP seam is deliberately narrow
  (only `ChainExec`) to keep this risk contained, but treat live OP
  behaviour as not yet end-to-end verified.
- **System-event recognition (spec §(b)/(c)).** L1 beacon withdrawals +
  the post-merge beneficiary priority-fee block reward are fully
  implemented and live-tested (the exact case that previously
  residual-stalled now completes with zero residual). OP deposit mints
  are implemented and unit-tested. **OP fee-vault per-block credits are
  NOT recognized** — the pinned reth-op API exposes no per-block
  vault-credit accessor (`docs/reth-pin.md` "B1 — recognized
  system-event APIs"); a watched address that is itself an OP fee-vault
  predeploy would residual-stall on its vault credit. Disclosed honestly;
  it is fail-closed (a stall, never silent loss) and a narrow case.
- `passbook_health` reports `last_block` and `chain_id` only. It does not
  report node-tip lag; a stall is observable as `last_block` no longer
  advancing (together with the error logs and the `unattributed_deltas`
  row).
- `op-reth` is not on crates.io; the build depends on a git dependency into
  the optimism monorepo and the documented minimal-checkout bootstrap. This
  goes away once `reth-op` is published.
