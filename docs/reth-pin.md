# reth / op-reth source pin

Single source of truth for the L1 and OP facade git sources, revisions,
toolchain, minimal-checkout mechanism, and verified facade module paths.

**Bump in lockstep** (see "Bump procedure"): the OP monorepo `rev` AND the
matching `paradigmxyz/reth` `rev` must move together, plus the committed
`Cargo.lock`.

## Locked sources

| Facade | Source | Rev | Version |
|--------|--------|-----|---------|
| `reth-op` (OP) | `ethereum-optimism/optimism` monorepo, via local mirror `file:///Users/aj/Documents/code/passbook/.vendor/optimism` (branch `develop`) | `27bf9194a08aef70f3fdbff6b3d04bdd70af62ff` | `reth-op 1.11.3` |
| `reth-ethereum` (L1) | `https://github.com/paradigmxyz/reth` | `88505c7fcbfdebfd3b56d88c86b62e950043c6c4` | `reth-ethereum 2.2.0` (reth v2.2.0) |

Date locked: 2026-05-16 (monorepo `develop` @ 2026-05-15).
Toolchain channel: `1.95.0` (see Toolchain).

> **op-reth is not yet on crates.io.** We get the `reth-op` facade via a git
> dependency into the `ethereum-optimism/optimism` monorepo (cargo locates
> the crate `reth-op` by name within the repo's `rust/` workspace, at
> `rust/op-reth/crates/reth/`). **Migrate `reth-op` to the published crate
> when it exists**, and at that point drop the `.vendor/` mirror and the
> `.cargo/config.toml` git-fetch-with-cli machinery.

## Why co-resolution works

Both facades pull their shared transitive `reth-*` crates from **one
identical upstream rev**:

- The monorepo's `rust/Cargo.toml` pins **every** upstream L1 reth crate
  (`reth-ethereum`, `reth-exex`, `reth-exex-test-utils`, `reth-node-api`,
  `reth-node-ethereum`, `reth-evm`, `reth-revm`, `reth-cli-util`, …) to
  `git = "https://github.com/paradigmxyz/reth", rev = "88505c7…"` — this is
  reth **v2.2.0**.
- In our root `Cargo.toml [workspace.dependencies]` we pin our L1 facade
  `reth-ethereum` to the **exact same** `paradigmxyz/reth` rev `88505c7…`.
- Cargo therefore unifies the shared transitive `reth-*` crates into a
  single dependency graph: the OP crates from the monorepo and our L1 facade
  resolve their common reth dependencies to one rev. Verified in
  `Cargo.lock`: every `paradigmxyz/reth` entry is at `88505c7…` (one rev),
  `reth-op 1.11.3` from the local monorepo mirror, `reth-ethereum 2.2.0`.
- The shared revm / alloy-evm stack also unifies because we pin
  `revm = "38.0.0"`, `revm-inspectors = "0.39.0"`, `alloy-evm = "0.34.0"`
  in our pin table — the exact crates.io versions the monorepo's
  `rust/Cargo.toml` uses.

If the monorepo rev and the `paradigmxyz/reth` rev ever drift apart,
co-resolution breaks (two reth revs ⇒ duplicate, incompatible types). They
**must stay in lockstep**.

## Minimal-checkout mechanism

The optimism monorepo full history is multi-GB (Go/Solidity/TS/etc). We
must not let cargo fat-clone it. Mechanism that works:

1. **Local mirror** — a *shallow depth-1* clone of the EXACT rev only (no
   history, no tags, no remote-tracking refs, no promisor). This is the key:
   a blobless+sparse promisor mirror does NOT work because cargo's
   `git fetch '+refs/heads/*' '+HEAD'` makes the mirror act as an
   upload-pack server that must serve a full pack, which forces server-side
   lazy blob fetches that are disabled ⇒ "could not fetch … from promisor
   remote / bad pack header". A shallow depth-1 clone is self-contained:

   ```sh
   M=/Users/aj/Documents/code/passbook/.vendor/optimism
   mkdir -p "$M" && git -C "$M" init -q
   git -C "$M" remote add origin https://github.com/ethereum-optimism/optimism.git
   git -C "$M" -c protocol.version=2 fetch --depth 1 --no-tags origin \
     27bf9194a08aef70f3fdbff6b3d04bdd70af62ff
   git -C "$M" branch -f develop FETCH_HEAD
   git -C "$M" symbolic-ref HEAD refs/heads/develop
   git -C "$M" remote remove origin   # fully detach; no promisor, no lazy fetch
   ```

   Result: ~45 MB `.git`, single commit `27bf9194`, single `develop`
   branch, `fsck --connectivity-only` clean, no promisor config. Cargo can
   clone it entirely offline.

2. **`.cargo/config.toml`** — `[net] git-fetch-with-cli = true` so cargo
   uses the system `git` binary (honors shallow/grafted clones and
   recurses the monorepo's git submodules referenced by the workspace).

3. **Root `Cargo.toml`** references the OP facade from this local git
   source:

   ```toml
   reth-op = { git = "file:///Users/aj/Documents/code/passbook/.vendor/optimism",
               rev = "27bf9194a08aef70f3fdbff6b3d04bdd70af62ff",
               default-features = false, features = ["node", "cli"] }
   ```

4. `.vendor/` is git-ignored (local mirror, not source). Always set
   `CARGO_NET_GIT_FETCH_WITH_CLI=true` for builds (also configured in
   `.cargo/config.toml`).

   > If a `file://` git dep ever proves problematic, the documented
   > fallback is a submodule / sparse worktree + a `path =` dep to
   > `rust/op-reth/crates/reth` — but a path dep pulls the crate out of its
   > workspace and may break `.workspace = true` resolution in the monorepo
   > crates, so the shallow-local-git-source approach above is preferred and
   > is what is in use.

## Required facade features

| Facade | Features enabled | Why |
|--------|------------------|-----|
| `reth-ethereum` | `["full", "cli"]` | `full` pulls `node` (→ `EthereumNode`), `exex`, provider/rpc/etc; `cli` for later binary tasks |
| `reth-op` | `["node", "cli"]` (`default-features = false`) | `node` provides `reth_op::node::OpNode` (pulls provider/consensus/evm/network/node-api/rpc/pool/trie-db); `cli` for later OP binary. (`full = [consensus,evm,node,provider,rpc,trie,pool,network]` would also work; `node` is the minimal set that yields `OpNode`.) |

## Verified facade module paths

Spike source (`crates/spike/src/main.rs`) compiles against these exact
paths at the locked revs:

| Crate | Path used by spike | Notes |
|-------|--------------------|-------|
| `reth-ethereum` | `reth_ethereum::exex::{ExExContext, ExExEvent}` | re-export of `reth_exex` |
| `reth-ethereum` | `reth_ethereum::node::EthereumNode` | `EthereumNode::default()` |
| `reth-ethereum` | `reth_ethereum::node::api::FullNodeComponents` | `api` = `reth_node_api` |
| `reth-op` | `reth_op::node::OpNode` | `OpNode::default()` (re-export of `reth_optimism_node`; needs `node` feature) |

These match the plan's v2.2.0 assumptions
(`reth_ethereum::{exex,node}`, `reth_op::node::OpNode`) — the L1 facade is
reth v2.2.0, exactly what the plan's code was written against, so no facade
deltas are expected for later tasks.

Confirmed by a clean `cargo build -p spike --locked` (Finished dev profile;
the only diagnostic is the expected `dead_code` warning for the unused
compile-only `exex` fn) and `./target/debug/spike` exiting 0.

## Key dependency versions (from `Cargo.lock` / `cargo tree`)

| Crate | Version |
|-------|---------|
| `reth-ethereum` | 2.2.0 |
| `reth-op` | 1.11.3 |
| `revm` | 38.0.0 |
| `revm-inspectors` | 0.39.0 |
| `alloy-evm` | 0.34.0 |

These ARE the plan's predicted baseline (`revm 38.0.0`,
`revm-inspectors 0.39.0`, `alloy-evm 0.34.0`). Later tasks (esp. the custom
revm inspector, Task 3.1, and EVM frame attribution, Task 4.3) are written
against these exact APIs — no API-version delta vs the plan.

## Resolution notes (committed `Cargo.lock`)

`Cargo.lock` is committed and load-bearing — always build `--locked`.

A fresh resolution of this graph can in principle hit semver skews that the
prior (invalidated) op-rs spike hit at a different reth lineage
(vergen/vergen-git2 incompatible `vergen-lib` majors; alloy-network vs
op-alloy-network `E0119` duplicate `NetworkWallet` impl). Mitigation if they
recur: seed the shared-crate versions from the monorepo's own tested
lockfile at `.vendor/optimism/rust/Cargo.lock` (it pins revm/alloy/vergen to
the set the monorepo CI validated), or pin the offending crates in our
`[workspace.dependencies]`. The committed `Cargo.lock` here was produced by
`cargo generate-lockfile` against the pinned sources (976 packages); if a
build skew is found it is recorded here with the applied pin.

## Toolchain

`rust-toolchain.toml` pins `channel = "1.95.0"`. The monorepo `rust/`
workspace declares `rust-version = "1.94"`; system default `stable` is
`rustc 1.95.0`. We pin `1.95.0` explicitly for reproducibility. Real
minimum observed: ≥ 1.94 (monorepo) / works on 1.95.0. Bump only if a build
error demands a newer compiler; record the real minimum here if so.

## Bump procedure

The two revs MUST move together (they share the upstream reth graph):

1. Pick a new `ethereum-optimism/optimism` `develop` SHA. Read its
   `rust/Cargo.toml` and note the `paradigmxyz/reth` `rev` it pins ALL
   upstream reth crates to, plus its `revm` / `revm-inspectors` /
   `alloy-evm` versions.
2. Re-seed the local mirror at the new SHA (rerun the shallow-clone
   commands in "Minimal-checkout mechanism" with the new rev; delete the
   old `.vendor/optimism` first).
3. In root `Cargo.toml`: update `reth-op` `rev` to the new monorepo SHA AND
   `reth-ethereum` `rev` to the new matching `paradigmxyz/reth` SHA (they
   must stay identical to what the monorepo uses), and update
   `revm`/`revm-inspectors`/`alloy-evm` to the monorepo's versions.
4. Clear cargo's git cache for the source
   (`rm -rf ~/.cargo/git/db/optimism-* ~/.cargo/git/checkouts/optimism-*`),
   then `CARGO_NET_GIT_FETCH_WITH_CLI=true cargo generate-lockfile` to
   re-seed `Cargo.lock`. If a transitive skew appears, seed the affected
   crates from `.vendor/optimism/rust/Cargo.lock` and document here.
5. Re-run the spike gate:
   `CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build -p spike --locked`
   then `./target/debug/spike`.
6. Re-run the integration test (added in later tasks).
7. Update the version table, toolchain row, and facade-path table in this
   file; re-check facade module paths against the new facade `lib.rs` if
   anything moved.
8. When `reth-op` is published to crates.io: replace the git dep with the
   published version, delete `.vendor/`, drop the `.cargo/config.toml`
   git-fetch-with-cli net section, and update steps 1–7 accordingly.
