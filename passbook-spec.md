# Passbook — Address Transfer ExEx — Design

Status: design approved, pending spec review
Date: 2026-05-16

## Overview

**Passbook** is a Reth Execution Extension (ExEx) that performs forward-only
capture, for a small static set of watched addresses (<10), of:

- ERC20 transfers in/out
- Native ETH transfers in/out, **including value moved by internal calls**
- Gas fees paid (L2 execution fee + OP L1 data fee where applicable)

into a durable, queryable ledger, with a verifiable completeness property.

This document is self-contained and describes only the ExEx to build. It is a
standalone Cargo workspace producing Docker image(s); how those images are
deployed and configured is the operator's concern and out of scope here.

## Scope

In scope:

- A node-generic ExEx crate + per-stack adapter.
- Two Docker images from one workspace: `reth-passbook` (L1) and
  `op-reth-passbook` (OP).
- Embedded SQLite ledger and an in-binary read-only query/export API.
- Static watched-address list supplied via a CLI option (no config file).
  One chain per node instance.
- Forward-only capture from the ExEx deploy block. No backfill. No archive nodes.

Out of scope:

- Historical backfill of any category (deferred; would need archive state for
  internal transfers — see Rejected alternatives).
- Any node migration work required to run reth on a given network. The image is
  required to work for both plain `reth` (L1) and `op-reth` (OP); provisioning
  such nodes is the operator's concern.
- Deployment, orchestration, and configuration management of the produced
  images.

## Architecture

Approach **A (all-in-one ExEx)**, chosen over a decoupled variant: decode +
filter + SQLite ledger + query API all run **inside the node process**. This
maximises topology simplicity at the cost of in-binary footprint; the
node-safety section is mandatory because of that choice.

### Repo layout (standalone workspace)

| Crate | Responsibility |
|-------|----------------|
| `passbook-core` | Node-generic capture logic. Written against `reth-exex` / `reth-node-api` traits so it compiles for both Ethereum and Optimism node types. No binary. |
| `passbook-stack-adapter` | Trait abstracting L1-vs-OP differences: gas/fee extraction (OP adds L1 data fee from the receipt; L1 has none) and system-balance events (L1 withdrawals/beacon deposits/block rewards; OP deposit mints / fee vaults). |
| `bin/reth-passbook` | Wires `EthereumNode` + the Passbook ExEx. |
| `bin/op-reth-passbook` | Wires the Optimism node + the same ExEx. |

### Images

One multi-stage `Dockerfile` builds **both** images. The upstream
reth/op-reth source revision is pinned to a chosen release; images are tagged
`<upstream-version>-passbook<N>` so deployments can pin an exact build. An
upstream bump means: bump the pin, rebuild, re-tag.

### Activation model

The image is a **safe drop-in**: with no watched-address option supplied the
ExEx is disabled and the node runs as stock reth/op-reth. Supplying the option
activates the ExEx. The option present but containing an invalid address ⇒
**abort node startup** (loud failure; a dedicated node silently not capturing
is worse than one that won't boot).

## Capture algorithm

Per `ExExNotification::ChainCommitted`, for each block:

### (a) ERC20 path — always, no tracing

Scan receipt logs for `topic0 == keccak("Transfer(address,address,uint256)")`
where topic1 (from) or topic2 (to) ∈ watched set. Record `token = log.address`,
from, to, amount, log index. No token allowlist — "all ERC20" means any
contract emitting the event; spoof tokens are stored as-is, filtering is a
query-time concern.

### (b) Native + gas path — gated

From the block's post-execution `BundleState`, compute accounts with a **balance
delta or nonce delta**, intersect with the watched set. Empty ⇒ skip the block
(the common fast path). Non-empty ⇒ re-execute the block with a call-tracing
inspector (state is the just-committed tip, so this is pruning-independent) and
walk call frames:

- **Top-level**: tx `from`/`to` with `value > 0` where watched.
- **Internal**: `CALL` / `CALLCODE` / `CREATE` / `CREATE2` / `SELFDESTRUCT`
  with `value > 0` where from or to ∈ watched. (DELEGATECALL moves no value.)
- **Gas**: txs where `tx.from ∈ watched` ⇒ `gas_used × effective_gas_price`
  (+ OP L1 fee via the stack adapter). Charged even on reverted txs.
- **System**: L1 withdrawals/deposits/block rewards, OP deposit mints — no call
  frame; surfaced via reconciliation below.

### (c) Completeness reconciliation

For each touched watched address, assert
`observed BundleState balance delta == Σ(top-level + internal + gas + system)`.
Recognized system events (L1 withdrawals/beacon deposits/block rewards, OP
deposit mints / fee vaults) are attributed as `kind = system` and produce no
residual. Any residual that remains is an **unexplained discrepancy and is
treated as a processing failure** (see Error handling): the
`unattributed_deltas` row is written as the diagnostic record of *why*
processing halted, and the block is not advanced past until resolved. The
ledger is therefore provably complete, never lossy.

### Reorgs & resume

`ChainReverted`/reorg ⇒ delete rows for reverted blocks (keyed by block hash)
before applying the new chain. Rows idempotently keyed by
`(chain_id, block_hash, tx_hash, trace_path | log_index)`.
`ExExEvent::FinishedHeight` is emitted **only after** a block's rows are durably
committed ⇒ correct restart resume, no dup/gap, reth can prune safely.

## Storage engine — rationale

SQLite (`rusqlite`), as a **separate store, never reth's own MDBX
environment**. Reth core uses MDBX (closed/typed table set) plus a custom
append-only static-file format; no SQL engine. SQLite is a net-new but
self-contained dependency, and is the persistence approach reth's own ExEx
documentation/examples use for address/transfer indexers.

Decisive factors:

- Workload is relational + analytical at a *trivial* write rate (only the rare
  gated blocks): filter by address, block-range scans, per-token group-by
  summaries, CSV export. SQL provides all of this directly; a KV store (MDBX
  reused, or redb) would require hand-rolled secondary indexes and aggregation
  for no benefit at this write volume.
- Reorg delete-by-`block_hash` is a trivial SQL statement.
- Sharing reth's own DB env would couple Passbook to reth storage internals and
  migrations on every upstream bump — against the minimise-blast-radius
  constraint. A separate store is required regardless of engine.

## Data model (SQLite)

| Table | Key columns |
|-------|-------------|
| `meta` | schema version, chain id, last-processed block, ExEx deploy block |
| `eth_transfers` | chain, block, block_hash, tx, trace_path, address, direction, counterparty, amount_wei, kind (`top_level\|internal\|system`), reverted |
| `erc20_transfers` | chain, block, block_hash, tx, log_index, token, from, to, amount, address, direction |
| `gas_payments` | chain, block, block_hash, tx, address, gas_used, effective_gas_price, l2_fee_wei, l1_fee_wei (nullable), total_wei |
| `unattributed_deltas` | chain, block, block_hash, address, observed_wei, attributed_wei, residual_wei |

Indexes on `(chain_id, address, block_number)`; every table carries
`block_hash` for reorg deletes.

## Query API (custom JSON-RPC namespace)

Served as the custom `passbook` namespace on reth's **existing JSON-RPC
server** — no separate HTTP port — to match the other op-reth/reth APIs.
Implemented as a `jsonrpsee` `#[rpc(server, namespace = "passbook")]` trait and
registered via the node builder's `extend_rpc_modules`
(`ctx.modules.merge_configured(...)`), sharing a read-only handle to the SQLite
ledger with the ExEx writer. The namespace is **enabled automatically whenever
watched addresses are listed** (i.e. exactly when the ExEx is active) — no
`--http.api` opt-in required; it is merged onto the node's configured RPC
transports. With no addresses listed the ExEx is inactive and the namespace is
absent (stock drop-in node).

Read-only methods:

- `passbook_health` — last processed block, lag vs node tip.
- `passbook_getTransfers` `{address, fromBlock?, toBlock?, kind?, cursor?}` —
  paginated native + ERC20 transfer rows (includes gas-payment and
  unattributed-delta rows via `kind`). Callers derive their own
  totals/exports from these rows.

## Configuration

No config file. The watched set is a CLI option (also settable via env),
`--passbook.addresses` taking a comma-separated list of addresses (≤10
expected). One chain runs per node instance, so there is no per-chain
structure. The ledger file path is a separate CLI option with a sensible
default. Behaviour:

- Option absent ⇒ ExEx disabled, node runs as stock node.
- Option present, all addresses valid ⇒ ExEx active.
- Option present, any address malformed ⇒ node startup aborts.

Changing the set = change the option + restart node (rare-change assumption;
no hot reload).

## Error handling & data-integrity guarantees

**Absolute rule: never lose or skip an entry.** Block processing is atomic and
strictly ordered — a block is either fully captured and durably committed, or
the ExEx does not advance past it. There is no skip-and-flag path anywhere.

- A block is "done" only when **all** of: ERC20 scan, gated native/gas
  attribution, reconciliation, and the durable DB transaction have succeeded.
  `ExExEvent::FinishedHeight` is emitted only then.
- Any failure in any step (log/ABI decode, tracer/re-execution, an unexplained
  reconciliation residual, DB write) ⇒ the **current block is retried with
  bounded backoff, indefinitely**. The ExEx never skips, never
  flags-and-continues, never advances, and never emits `FinishedHeight` for an
  incomplete block.
- Consequence, and intended: a *deterministic* failure (e.g. a decode bug)
  halts indexing progress until fixed — "can't process" means "don't proceed."
  It is surfaced loudly (error logs, a stalled-height metric, and
  `passbook_health` reporting the stall) for operator intervention. The fix is
  a code change + redeploy, after which processing resumes exactly where it
  stalled — no gap, no duplicate.
- The ExEx halts by **stalling** (retrying the current block), not by crashing
  the node. The node keeps running so RPC stays available; the ExEx applies
  natural backpressure and reth will not prune below the stalled height.
- RPC handlers never swallow errors: any error servicing a `passbook_*`
  request is returned to the caller as a JSON-RPC error. RPC failures are
  fully independent of, and cannot affect, the writer or block processing.

## Testing (TDD)

- Unit: ERC20 decode (incl. malformed/non-standard), call-frame value walker
  (internal / selfdestruct / create), L1-vs-OP fee math, reconciliation
  residual.
- Integration: synthetic/recorded block fixtures via reth `test-utils`; assert
  ledger rows and zero reconciliation residual for known scenarios.
- Fault injection: forced decode/tracer/DB/residual failure ⇒ ExEx stalls at
  the block, never advances or emits `FinishedHeight`; clearing the fault
  resumes at the same block with no gap/dup.
- Reorg: commit → revert → commit alternate; rows replaced, no dupes.
- Resume: restart mid-stream; no gap/dup via `FinishedHeight`.
- Build: both images compile and start against their chains in CI.

## Validation matrix

| Property | How verified |
|----------|--------------|
| ERC20 in/out captured | Fixture with known Transfer logs ⇒ expected rows |
| Internal ETH captured | Fixture with contract-forwarded value ⇒ internal rows |
| Gas (L1 & OP) captured | Fixture txs sent by watched addr ⇒ gas_payments incl. l1_fee on OP |
| Completeness | Reconciliation residual == 0 on fixtures |
| Halt-on-failure | Fault-injected decode/tracer/DB/residual error ⇒ ExEx stalls at that height, no advance, no `FinishedHeight`, health reports stall |
| Resume-after-fix | Clear the injected fault ⇒ processing resumes at the stalled block, no gap/dup |
| Reorg safety | Revert/replace test ⇒ no dup/orphan rows |
| Restart safety | Resume test ⇒ no gap/dup |
| Drop-in safety | No addresses option ⇒ node behaves as stock; smoke test |
| Dual-stack | `reth-passbook` and `op-reth-passbook` both build & run |

## Rejected alternatives

- **External sidecar (no ExEx), RPC `trace_filter`**: keeps stock images but
  higher per-block cost and weaker for internal transfers; true ExEx chosen.
- **Decoupled ExEx (thin ExEx + separate read-only API container)**: smaller
  in-binary surface and blast radius, but more topology; Approach A chosen for
  simplicity. Node-safety section compensates.
- **Standalone `axum`/REST server on a separate port**: extra port and process
  surface; rejected in favour of a custom JSON-RPC namespace on reth's existing
  server to match the other node APIs.
- **Backfill / archive nodes**: large disk, the expensive path; explicitly
  deferred to forward-only.

## Constraints (recap, do not violate)

- Self-contained workspace; no dependency on any particular deployment system.
- One workspace builds images that work for both `reth` (L1) and `op-reth` (OP).
- Forward-only; no backfill; no archive-node dependency.
- Drop-in safe: no addresses option ⇒ stock node behaviour.
- Never lose or skip an entry. Any processing failure halts progress (atomic
  per-block, retry-until-success); no skip-and-flag anywhere.
- Ledger is a separate SQLite store; never reth's own MDBX environment.

## Open items, resolved at implementation-plan time

- Exact upstream reth/op-reth crate revision to pin.
- Inspector choice/implementation for the gated re-execution (reth tracing
  inspector vs custom minimal value-only inspector).
- SQLite pragmas / WAL settings for single-writer durability under
  retry-until-success.
