# reth / op-reth source pin

Single source of truth for the L1 and OP facade git sources, revisions,
toolchain, minimal-checkout mechanism, and verified facade module paths.

**Bump in lockstep** (see "Bump procedure"): the OP monorepo `rev` AND the
matching `paradigmxyz/reth` `rev` must move together, plus the committed
`Cargo.lock`. This is fully automated by **`scripts/bump-reth.sh`** (or
`make bump`) — that script is the primary, one-command path; the manual steps
below remain for understanding/recovery. Post-bump check: **`make verify-pin`**.

> **Bootstrap (every fresh machine / CI / Docker stage):** run
> `scripts/seed-vendor.sh` once, then build with `--locked`. The script
> creates the local mirror and the gitignored `.cargo/config.toml`. A clean
> checkout that skips the script still builds correctly — cargo just clones
> the full multi-GB monorepo from the remote (slow). The mirror is a pure
> optimization, never a correctness requirement.

## Locked sources

| Facade | Source | Rev | Version |
|--------|--------|-----|---------|
| `reth-op` (OP) | `https://github.com/ethereum-optimism/optimism` monorepo (declared against this stable remote in `Cargo.toml`; source-replaced onto a gitignored local mirror at `<repo>/.vendor/optimism` by `scripts/seed-vendor.sh` for speed) | `4ddba1610a5d13c1a8c297a91228559d731cc6d5` | `reth-op 1.11.3` (op-reth release tag `op-reth/v2.2.3-rc.1-pr20770.0`; crate-version metadata is still `1.11.3`) |
| `reth-ethereum` (L1) | `https://github.com/paradigmxyz/reth` | `e8c29c987dd5bb23e95114279c89ce5326fd206d` | `reth-ethereum 2.2.0` (post-v2.2.0 `paradigmxyz/reth` develop SHA — the rev the monorepo at the above optimism rev pins all upstream reth crates to) |

Date locked: 2026-05-17 (optimism monorepo @ op-reth release tag `op-reth/v2.2.3-rc.1-pr20770.0`).
Toolchain channel: `1.95.0` (see Toolchain).

> **op-reth is not yet on crates.io.** We get the `reth-op` facade via a git
> dependency into the `ethereum-optimism/optimism` monorepo (cargo locates
> the crate `reth-op` by name within the repo's `rust/` workspace, at
> `rust/op-reth/crates/reth/`). **Migrate `reth-op` to the published crate
> when it exists**, and at that point drop the `.vendor/` mirror, the
> generated `.cargo/config.toml`, and `scripts/seed-vendor.sh`.

## Why co-resolution works

Both facades pull their shared transitive `reth-*` crates from **one
identical upstream rev**:

- The monorepo's `rust/Cargo.toml` pins **every** upstream L1 reth crate
  (`reth-ethereum`, `reth-exex`, `reth-exex-test-utils`, `reth-node-api`,
  `reth-node-ethereum`, `reth-evm`, `reth-revm`, `reth-cli-util`, …) to
  `git = "https://github.com/paradigmxyz/reth", rev = "e8c29c98…"` — a
  post-**v2.2.0** develop SHA (crate version still `reth-ethereum 2.2.0`).
- In our root `Cargo.toml [workspace.dependencies]` we pin our L1 facade
  `reth-ethereum` to the **exact same** `paradigmxyz/reth` rev `e8c29c98…`.
- Cargo therefore unifies the shared transitive `reth-*` crates into a
  single dependency graph: the OP crates from the monorepo and our L1 facade
  resolve their common reth dependencies to one rev. Verified in
  `Cargo.lock`: every `paradigmxyz/reth` entry is at `e8c29c98…` (one rev),
  `reth-op 1.11.3` from the canonical
  `git+https://github.com/ethereum-optimism/optimism?rev=4ddba161…` source
  (source-replaced onto the local mirror at build time),
  `reth-ethereum 2.2.0`.
- The shared revm / alloy-evm stack also unifies because we pin
  `revm = "38.0.0"`, `revm-inspectors = "0.39.0"`, `alloy-evm = "0.34.0"`
  in our pin table — the exact crates.io versions the monorepo's
  `rust/Cargo.toml` uses.

If the monorepo rev and the `paradigmxyz/reth` rev ever drift apart,
co-resolution breaks (two reth revs ⇒ duplicate, incompatible types). They
**must stay in lockstep**.

## Minimal-checkout mechanism (portable)

The optimism monorepo full history is multi-GB (Go/Solidity/TS/etc). We do
not want cargo to fat-clone it. The mechanism is **portable** — nothing
machine-specific or absolute is committed — and the minimal checkout is a
**pure optimization** delivered entirely via gitignored, runtime-generated
state.

### Committed, portable

- **Root `Cargo.toml`** declares the OP facade against the **stable remote
  URL**:

  ```toml
  reth-op = { git = "https://github.com/ethereum-optimism/optimism",
              rev = "4ddba1610a5d13c1a8c297a91228559d731cc6d5",
              default-features = false, features = ["node", "cli"] }
  ```

  A clean checkout with **no** local mirror and **no** `.cargo/config.toml`
  is therefore still correct: cargo clones the full monorepo from the remote
  (slow, but produces an identical, `--locked`-consistent graph).

- **`Cargo.lock`** is committed and records the canonical remote source
  `git+https://github.com/ethereum-optimism/optimism?rev=4ddba161…` for
  `reth-op` and every sibling monorepo crate. This is the correct, portable
  form: cargo's source replacement keys off this canonical URL, so the same
  lockfile validates with `--locked` whether or not the mirror is present.
  (`.vendor/` and `.cargo/` are gitignored and hold **no** committed state.)

### Gitignored, generated by `scripts/seed-vendor.sh`

`scripts/seed-vendor.sh` is the documented bootstrap for every fresh
machine / CI runner / Docker stage. It is **idempotent** (running it twice
is a no-op), derives the repo root from its own location (no hardcoded
path), and does two things:

1. **Local mirror** — a *shallow depth-1* clone of the EXACT rev only into
   `<repo>/.vendor/optimism`. This is the key technique: a blobless+sparse
   promisor mirror does NOT work because cargo's `git fetch` makes the
   mirror act as an upload-pack server that must serve a full pack, forcing
   disabled server-side lazy blob fetches ⇒ "could not fetch … from
   promisor remote / bad pack header". A shallow depth-1 clone is
   self-contained. The clone keeps an inert `origin` remote — that is
   harmless: the "no promisor / no lazy fetch" property comes from the
   shallow depth-1 fetch (the pack is self-contained), **not** from removing
   the remote. `origin` is retained so a future re-seed/bump can fetch a new
   rev without re-adding it. Result: ~45 MB `.git`, single commit, single
   `develop` branch, cargo can clone it entirely offline.

2. **Gitignored `<repo>/.cargo/config.toml`** — generated with the
   absolute mirror path computed at runtime. It sets
   `[net] git-fetch-with-cli = true` (system `git` binary clones the
   shallow mirror correctly; cargo's libgit2 mishandles grafted/shallow
   clones) and a **`[source]` replacement** that redirects the *entire*
   `https://github.com/ethereum-optimism/optimism` git source onto the
   local mirror:

   ```toml
   [source."git+https://github.com/ethereum-optimism/optimism?rev=4ddba161…"]
   git = "https://github.com/ethereum-optimism/optimism"
   rev = "4ddba161…"
   replace-with = "optimism-local-mirror"

   [source.optimism-local-mirror]
   git = "file://<repo>/.vendor/optimism"
   rev = "4ddba161…"
   ```

   The replacement is keyed on the whole git source, so it covers `reth-op`
   **and every sibling monorepo crate** (`reth-optimism-evm`,
   `reth-optimism-node`, …) that `reth-op`'s workspace pulls from the same
   source — none of them touch the remote.

> **Why source replacement, not a `file://` dep in `Cargo.toml`:** the old
> approach put an absolute `file:///Users/...` URL straight into the
> committed `Cargo.toml`, which broke on every other machine / CI / Docker.
> Source replacement keeps `Cargo.toml`/`Cargo.lock` on the portable remote
> URL while a gitignored, regenerated config does the local redirection.
> Verified: `CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build -p spike
> --locked` finishes clean using the 45 MB local mirror (cargo logs
> `Updating git repository file://…/.vendor/optimism`; no multi-GB clone).
>
> One known cargo quirk: `cargo generate-lockfile` refuses to run while the
> source replacement is active ("requires a lock file to be present
> first … remove the source replacement, generate a lock file, then
> restore it"). The committed `Cargo.lock` already exists, so normal
> `--locked` builds are unaffected. Only a **rev bump** needs the lockfile
> regenerated — see the bump procedure (move the `.cargo/config.toml`
> aside, regenerate against the remote, restore).

> `edition = "2021"` for the spike crate (`crates/spike/Cargo.toml`) is
> **intentional** — it matches the reth ecosystem's edition. Do not
> "upgrade" it to 2024.

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

Confirmed by a clean `cargo build -p spike --locked` (Finished dev profile,
**zero warnings**) and `./target/debug/spike` exiting 0. The compile-only
`exex` fn carries an explicit `#[allow(dead_code)]` with a doc comment
stating it is a never-called compile gate, so there is no warning to
explain away.

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

## Workspace third-party pins

Task 0.2 turned the root `Cargo.toml [workspace.dependencies]` into the
single pin table for the whole project. The alloy / jsonrpsee versions are
NOT floated — they are pinned to the **exact versions the reth v2.2.0 graph
already resolved** in the committed `Cargo.lock`. A mismatched alloy/revm
version causes `E0119` trait-coherence breakage, so these must track the
locked graph.

| Crate | Pinned in `[workspace.dependencies]` | Cargo.lock evidence |
|-------|--------------------------------------|---------------------|
| `alloy-primitives` | `1.6.0` | sole `alloy-primitives` entry in `Cargo.lock` is `1.6.0` |
| `alloy-consensus` | `2.0.4` | sole `alloy-consensus` entry is `2.0.4` |
| `alloy-eips` | `2.0.4` | `Cargo.lock` has `1.8.3` and `2.0.4`; the reth v2.2.0 / `alloy-consensus 2.0.4` graph uses the `2.x` line (`alloy-consensus 2.0.4` → `alloy-eips 2.0.4`), so we pin the `2.0.4` major |
| `jsonrpsee` | `0.26.0` (features `server`, `macros`) | `jsonrpsee`, `jsonrpsee-server`, `jsonrpsee-core` all resolve to `0.26.0` |
| `async-trait` | `0.1` | resolved `0.1.89` (kept loose `0.1`, semver-compatible) |
| `clap` | `4` | resolved `4.6.1` |
| `tokio` | `1` | resolved `1.52.3` |
| `tracing` | `0.1` | resolved `0.1.44` |
| `thiserror` | `2` | `Cargo.lock` has `1.0.69` and `2.0.18`; we pin the `2` major (`2.0.18` present) |
| `serde` | `1` | resolved `1.0.228` |
| `tempfile` | `3` (dev) | resolved `3.27.0` |
| `rusqlite` | `0.37` (feature `bundled`) | NOT in `Cargo.lock` yet — no member depends on it until Task 1.x. Adding it to the pin table does not change resolution; it enters the lock when the first member uses it. `0.37` chosen as a conservative stable line. |
| `reth-optimism-evm` | monorepo `4ddba161…` | already in `Cargo.lock` at `1.11.3` from the optimism git source (needed later by the OP stack adapter, Task 8.2) |
| `reth-exex-test-utils` | paradigmxyz/reth `e8c29c98…` | NOT in `Cargo.lock` yet — dev-dep used by passbook-core integration tests (Task 6.5); enters the lock when that member uses it. Pinned to the same rev as `reth-ethereum` so it stays in the unified reth v2.2.0 graph. |

Adding entries to `[workspace.dependencies]` that no current member uses
does not alter resolution or the lockfile — `cargo build -p spike --locked`
stays green and `Cargo.lock` is unchanged. Later tasks add the members that
actually consume these pins.

## Resolution notes (committed `Cargo.lock`)

`Cargo.lock` is committed and load-bearing — always build `--locked`.

A fresh resolution of this graph can in principle hit semver skews that the
prior (invalidated) op-rs spike hit at a different reth lineage
(vergen/vergen-git2 incompatible `vergen-lib` majors; alloy-network vs
op-alloy-network `E0119` duplicate `NetworkWallet` impl). Mitigation if they
recur: seed the shared-crate versions from the monorepo's own tested
lockfile at `.vendor/optimism/rust/Cargo.lock` (it pins revm/alloy/vergen to
the set the monorepo CI validated), or pin the offending crates in our
`[workspace.dependencies]`. The committed `Cargo.lock` here was originally
produced by `cargo generate-lockfile` against the pinned sources
(976 packages) and its `reth-op`/sibling source strings were retargeted to
the canonical remote URL when the portable source-replacement mechanism
landed (see "Minimal-checkout mechanism"); it is `--locked`-consistent. If a
build skew is found it is recorded here with the applied pin.

## Toolchain

`rust-toolchain.toml` pins `channel = "1.95.0"`. The monorepo `rust/`
workspace declares `rust-version = "1.94"`; system default `stable` is
`rustc 1.95.0`. We pin `1.95.0` explicitly for reproducibility. Real
minimum observed: ≥ 1.94 (monorepo) / works on 1.95.0. Bump only if a build
error demands a newer compiler; record the real minimum here if so.

## Bump procedure

The two revs MUST move together (they share the upstream reth graph).

### Automated path (primary) — `scripts/bump-reth.sh`

This SUPERSEDES the old single-`rev` sed draft. One command does the whole
lockstep bump (rev rewrites in **both** `Cargo.toml` *and* the duplicated
`OPTIMISM_REV` in `scripts/seed-vendor.sh`, mirror re-seed, `Cargo.lock`
regen, and the spike co-resolution gate). It NEVER commits — the operator
reviews then commits.

```sh
# 1. Pick a new ethereum-optimism/optimism develop SHA, then find the
#    matching paradigmxyz/reth rev it pins (running the script with only
#    --optimism-rev prints these exact instructions):
git clone --depth 1 https://github.com/ethereum-optimism/optimism /tmp/op \
  && git -C /tmp/op fetch --depth 1 origin <OPT_SHA> \
  && git -C /tmp/op checkout FETCH_HEAD \
  && grep -m1 'paradigmxyz/reth' /tmp/op/rust/Cargo.toml   # -> <RETH_SHA>

# 2. Run the lockstep bump (both revs REQUIRED):
scripts/bump-reth.sh --optimism-rev <OPT_SHA> --reth-rev <RETH_SHA>
#   or:  make bump ARGS='--optimism-rev <OPT_SHA> --reth-rev <RETH_SHA>'

# 3. Review the working-tree diff, then run the post-bump check:
make verify-pin

# 4. Update the version/toolchain/facade-path tables below if anything
#    moved, then commit yourself.
```

The script is idempotent: re-running with the currently-pinned revs is a
clean no-op (it reports "already at these revs" and touches no files). On a
gate failure it exits non-zero and leaves the (uncommitted) working tree for
review — it never produces a half-broken commit. It also detects pre-existing
drift between the `Cargo.toml` optimism rev and `seed-vendor.sh`'s
`OPTIMISM_REV` and refuses to proceed.

### Manual steps (for understanding / recovery)

The script automates exactly the following; do these by hand only if the
script cannot be used:

1. Pick a new `ethereum-optimism/optimism` `develop` SHA. Read its
   `rust/Cargo.toml` and note the `paradigmxyz/reth` `rev` it pins ALL
   upstream reth crates to, plus its `revm` / `revm-inspectors` /
   `alloy-evm` versions.
2. Update the rev in **both** places that hold it: `reth-op` `rev` in root
   `Cargo.toml` (set to the new monorepo SHA) and the `OPTIMISM_REV` default
   in `scripts/seed-vendor.sh`. Also set `reth-ethereum` `rev` to the new
   matching `paradigmxyz/reth` SHA (must stay identical to what the
   monorepo's `rust/Cargo.toml` uses), and update
   `revm`/`revm-inspectors`/`alloy-evm` to the monorepo's versions.
3. Re-seed the local mirror: `rm -rf .vendor/optimism` then
   `scripts/seed-vendor.sh` (it shallow-clones the new rev and regenerates
   the gitignored `.cargo/config.toml`). The script is idempotent.
4. Regenerate `Cargo.lock`. Because cargo's source replacement blocks
   `generate-lockfile`, do the documented dance:
   ```sh
   mv .cargo/config.toml /tmp/cc.bak                 # disable replacement
   rm -rf ~/.cargo/git/db/optimism-* ~/.cargo/git/checkouts/optimism-*
   CARGO_NET_GIT_FETCH_WITH_CLI=true cargo generate-lockfile  # full remote clone, slow
   mv /tmp/cc.bak .cargo/config.toml                 # restore replacement
   ```
   The resulting `Cargo.lock` correctly records the canonical remote source
   URL. (If you only changed the rev and want to avoid the multi-GB clone,
   you may instead rewrite the old rev's source strings to the new rev in
   `Cargo.lock` by hand and validate with `--locked` — that is how the
   I2 fix produced the current lock.) If a transitive skew appears, seed the
   affected crates from `.vendor/optimism/rust/Cargo.lock` and document here.
5. Re-run the spike gate:
   `CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build -p spike --locked`
   then `./target/debug/spike`. Confirm it logs
   `Updating git repository file://…/.vendor/optimism` (mirror used, no
   multi-GB clone).
6. Re-run the integration test *(once Task 6.5 lands; skip until then —
   no integration test exists yet)*.
7. Update the version table, toolchain row, and facade-path table in this
   file; re-check facade module paths against the new facade `lib.rs` if
   anything moved.
8. When `reth-op` is published to crates.io: replace the git dep with the
   published version, delete `.vendor/`, delete `scripts/seed-vendor.sh`,
   stop generating `.cargo/config.toml`, and update steps 1–7 accordingly.

## Known constraints

- **op-reth is not yet on crates.io.** We depend on it via a git dep into
  the `ethereum-optimism/optimism` monorepo. Migrate to the published crate
  when it exists (see bump-procedure step 8), then drop all `.vendor/` +
  `seed-vendor.sh` machinery.
- **The `.vendor/` mirror and the generated `.cargo/config.toml` are
  gitignored and hold no committed state.** They are a per-environment
  optimization and **must be regenerated on every fresh machine, CI runner,
  and Docker build stage** by running `scripts/seed-vendor.sh` before
  `cargo build --locked`. Task 9.1 (Dockerfile) and Task 9.2 (CI) must
  invoke `scripts/seed-vendor.sh` as an explicit build step.
- A clean checkout that skips `seed-vendor.sh` is still *correct* — cargo
  clones the full multi-GB monorepo from the stable remote URL in
  `Cargo.toml` (slow). Correctness never depends on the mirror.

## revm 38 API deltas

How the `revm::Inspector` impl for `ValueInspector`
(`crates/passbook-core/src/inspector.rs`, Task 3.1) differed from the plan's
candidate, verified against the pinned stack: `revm 38.0.0` →
`revm-inspector 19.0.0` (the `Inspector` trait), `revm-interpreter 35.0.1`
(`CallInputs`/`CallValue`/`CallScheme`/`CreateInputs`/`CreateOutcome`),
`revm-context-interface 17.0.1` (`CreateScheme`). **Load-bearing for Task
6.4** (wiring the inspector into the ExEx execution path).

Exact trait shape in revm-inspector 19.0.0:

```rust
#[auto_impl(&mut, Box)]
pub trait Inspector<CTX, INTR: InterpreterTypes = EthInterpreter,
                    FI = FrameInput, FR = FrameResult> {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs)
        -> Option<CallOutcome> { ... }
    fn create_end(&mut self, context: &mut CTX, inputs: &CreateInputs,
                  outcome: &mut CreateOutcome) { ... }
    fn selfdestruct(&mut self, contract: Address, target: Address,
                    value: U256) { ... }
    // plus initialize_interp/step/step_end/log/log_full/
    // frame_start/frame_end/call_end/create defaulted hooks
}
```

Deltas vs. the candidate:

- **Trait generics.** The trait is
  `Inspector<CTX, INTR: InterpreterTypes = EthInterpreter, FI = FrameInput,
  FR = FrameResult>`, not `Inspector<CTX>`. The three extra params all have
  defaults, so the candidate's `impl<CTX> Inspector<CTX> for ValueInspector`
  still compiles unchanged via the `EthInterpreter`/`FrameInput`/
  `FrameResult` defaults. Kept as-is. (If a future caller needs a non-Eth
  interpreter type, the impl must become generic over `INTR`/`FI`/`FR`.)
- **`call` / `create_end` / `selfdestruct` signatures matched the candidate
  exactly** — same parameter order, mutability, and `&mut CTX`/`&CreateInputs`/
  `&mut CreateOutcome` shapes. `selfdestruct(contract, target, value: U256)`
  is verbatim.
- **`CreateInputs` fields are PRIVATE** (`caller`, `scheme`, `value`,
  `init_code`, … are non-`pub`). The candidate's direct field access
  (`i.value`, `i.caller`, `i.scheme`) does **not** compile. Switched to the
  accessor methods: `inputs.value() -> U256`, `inputs.caller() -> Address`,
  `inputs.scheme() -> CreateScheme`. (`CreateInputs` also exposes
  `created_address(nonce)`, `init_code()`, `gas_limit()`, `reservoir()`.)
- **`CreateScheme` import path moved.** It is **not** in `revm::interpreter`;
  it lives in `revm::context_interface::CreateScheme` (defined in
  `revm-context-interface 17.0.1` `src/cfg.rs`). Candidate imported it from
  `revm::interpreter::CreateScheme` — corrected. Variants:
  `Create`, `Create2 { salt: U256 }`, `Custom { address: Address }`. The
  candidate's `CreateScheme::Create2 { .. }` match arm is correct (the new
  `Custom` variant falls through the `_ => Create` arm, which is acceptable —
  it is not CREATE2).
- **`CallInputs` shape matched the candidate.** `value: CallValue` (public
  field), `caller: Address`, `target_address: Address`, `scheme: CallScheme`
  are all public. `CallValue::Transfer(U256)` / `CallValue::Apparent(U256)` —
  only `Transfer` carries real transferable value; `Apparent` is what
  DELEGATECALL/STATICCALL surface, so matching only `Transfer` correctly
  excludes them (the candidate's `if let CallValue::Transfer(v)` is right).
  `CallScheme::{Call, CallCode, DelegateCall, StaticCall}` — candidate's
  `CallScheme::CallCode` arm is correct.
- **`CreateOutcome.address` is `pub: Option<Address>`** — `outcome.address`
  works as the candidate assumed.
- **Import paths.** `CallInputs`, `CallOutcome`, `CallScheme`, `CallValue`,
  `CreateInputs`, `CreateOutcome` are all re-exported from
  `revm::interpreter::*` (revm 38 `pub use interpreter;` → revm-interpreter
  35.0.1 `pub use interpreter_action::{...}`). `CreateScheme` is the only one
  that comes from `revm::context_interface` instead.

Net: the **pure capture core (`ValueInspector::push_frame` + `FrameKind`/
`FrameMove`/`CapturedFrame`)** is verbatim per the plan and unchanged. The
trait glue required exactly two corrections — `CreateInputs` accessor methods
instead of private-field access, and the `CreateScheme` import path
(`context_interface`, not `interpreter`) — plus awareness that the trait
carries three defaulted generic params. Task 6.4 should expect to pass an
`EthInterpreter`-typed context for the default impl to apply.

## reth v2.2.0 ExEx API deltas (Task 6.3)

How the real reth **v2.2.0** ExEx surface (paradigmxyz/reth rev
`88505c7…`, facade `reth-ethereum 2.2.0`) differed from the plan
candidate, verified against the pinned source under
`~/.cargo/git/checkouts/reth-e231042ee7db3fb7/88505c7/` (and
`reth-primitives-traits 0.3.1` from crates.io for `RecoveredBlock`).
**Load-bearing for Task 6.4** (it writes the `process_one_committed_block`
body and the integration harness).

### Verified module paths (facade `reth-ethereum`)

| Symbol | Path the loop compiles against | Source |
|--------|--------------------------------|--------|
| `ExExContext`, `ExExEvent` | `reth_ethereum::exex::{ExExContext, ExExEvent}` | `pub use reth_exex as exex;` (facade lib.rs:65) |
| `ExExNotification` | `reth_ethereum::exex::ExExNotification` | same re-export |
| `FullNodeComponents` | `reth_ethereum::node::api::FullNodeComponents` | `node` mod → `pub use reth_node_api as api;` |
| `EthChainSpec` | `reth_ethereum::chainspec::EthChainSpec` | `chainspec` mod → `pub use reth_chainspec::*;` |

### Deltas vs the candidate

- **`ExExContext` field types (exact).** `crates/exex/exex/src/context.rs`:
  `pub struct ExExContext<Node: FullNodeComponents>` with
  `pub notifications: ExExNotifications<Node::Provider, Node::Evm>`,
  `pub events: UnboundedSender<ExExEvent>`,
  `pub config: NodeConfig<<Node::Types as NodeTypes>::ChainSpec>`,
  `pub components: Node`. Matches the candidate's assumed field names.

- **`notifications` Stream item type.** `ExExNotifications<P, E>` implements
  `Stream` with **`type Item = eyre::Result<ExExNotification<E::Primitives>>`**
  (`crates/exex/exex/src/notifications.rs:178`). So
  `while let Some(notification) = ctx.notifications.try_next().await? {`
  (via `futures::TryStreamExt`) is correct verbatim — the `?` unwraps the
  inner `eyre::Result`. Confirmed identical to the spike compile-gate.

- **`ExExNotification` chain accessors.** `crates/exex/exex/src/notification.rs`:
  `pub fn committed_chain(&self) -> Option<Arc<Chain<N>>>` and
  `pub fn reverted_chain(&self) -> Option<Arc<Chain<N>>>` — exactly the
  candidate's shapes. `N` defaults to `reth_chain_state::EthPrimitives`;
  the generic flows from `ExExNotification<E::Primitives>`.

- **`Chain` methods.** `crates/evm/execution-types/src/chain.rs`:
  `pub fn blocks_iter(&self) -> impl Iterator<Item = &RecoveredBlock<N::Block>>`,
  `pub fn tip(&self) -> &RecoveredBlock<N::Block>`,
  `pub fn range(&self) -> RangeInclusive<BlockNumber>`. The candidate's
  `chain.blocks_iter()` / `chain.tip()` are correct. Note `blocks_iter()`
  yields **`&RecoveredBlock`** (a *recovered* block, not a bare `SealedBlock`)
  — Task 6.4's body builds `BlockInputs` from `&RecoveredBlock`.

- **Block hash / num_hash.** `RecoveredBlock`
  (`reth-primitives-traits 0.3.1` `src/block/recovered.rs`) provides
  `pub fn hash(&self) -> BlockHash` (= `B256`) and
  `pub fn num_hash(&self) -> BlockNumHash`. The loop uses `block.hash()`
  for the per-block hash (reverted-chain deletes and the
  `UnattributedDeltaRow`) and `chain.tip().num_hash()` for
  `FinishedHeight` — both compile, matching the candidate's
  `block.hash()` / `chain.tip().num_hash()` hints.

- **`ExExEvent::FinishedHeight(BlockNumHash)`.**
  `crates/exex/exex/src/event.rs:5`: `pub enum ExExEvent { … FinishedHeight(BlockNumHash) }`
  (`BlockNumHash` = `alloy_eips::BlockNumHash`). `ctx.events` is an
  `UnboundedSender<ExExEvent>`, so `ctx.events.send(ExExEvent::FinishedHeight(
  chain.tip().num_hash()))?` is verbatim correct.

- **chain id accessor — the one real correction.** The candidate suggested
  `ctx.config.chain.chain().id()`. In v2.2.0 `ctx.config.chain` is
  `Arc<<Node::Types as NodeTypes>::ChainSpec>`. That associated type is
  bound `EthChainSpec` (`crates/node/types/src/lib.rs:31`), and
  `EthChainSpec` (`crates/chainspec/src/api.rs:14`) provides a **direct
  `fn chain_id(&self) -> u64`** (default impl over `fn chain(&self) -> Chain`).
  The loop uses `ctx.config.chain.chain_id()` — but this **requires the
  `EthChainSpec` trait to be in scope**: `use
  reth_ethereum::chainspec::EthChainSpec;`. Without that import the build
  fails `E0599: no method named chain_id … trait EthChainSpec … not in
  scope`. (`.chain().id()` would also work but pulls in `alloy_chains::Chain`;
  `chain_id()` is the minimal, generic, node-agnostic path and is what
  Task 6.4 / the OP binary should use.)

### Node-generic checkpoint (CRITICAL — PASSED)

`pub async fn run_passbook<Node, S>(mut ctx: ExExContext<Node>, …) where
Node: FullNodeComponents, S: StackAdapter` compiles cleanly against the
re-exported upstream ExEx / node-api surface of the **L1 facade
`reth-ethereum` only** — no extra upstream-reth crate was needed in
`passbook-core` (the existing `reth-ethereum` dep + `EthChainSpec` from its
own `chainspec` re-export sufficed). This is the same node-generic
signature shape the spike already proved type-checks for **both**
`EthereumNode` and `OpNode`, so the dependency boundary flagged in Task 0.3
holds: one ExEx fn body in `passbook-core` is usable by both stacks. No
restructuring of the dependency boundary was required.

`process_one_committed_block` is a `todo!()` stub. Its `chain`/`block`
params are kept as **inferred generics** (`C`/`B`) rather than naming
`Arc<Chain<N>>` / `RecoveredBlock<N::Block>` explicitly — this lets the
signature unify with the call site for any node primitive set without
importing the upstream `Chain` generic into `passbook-core` yet. Task 6.4
replaces the stub: it will need to name the concrete reth types (likely
`reth_ethereum::provider`/execution-types `Chain` and
`reth_ethereum::primitives` `RecoveredBlock`) and add the real trait
bounds, plus thread the revm `ValueInspector` (see "revm 38 API deltas",
`EthInterpreter`-typed context).

## reth v2.2.0 re-execution / test-utils API (Task 6.4)

How the real per-committed-block pipeline + integration harness were
wired against pinned reth **v2.2.0** (`paradigmxyz/reth` `88505c7…`),
and the deltas vs the research idioms. **Load-bearing for Tasks
6.5 / 8.4 / 8.5** (the L1/OP binaries reuse this body verbatim; only the
`make_adapter` closure differs).

### Final signatures (confined to `crates/passbook-core/src/exex.rs`)

```rust
pub async fn run_passbook<Node, S>(
    mut ctx: ExExContext<Node>,
    cfg: PassbookConfig,
    ledger: Arc<Mutex<Ledger>>,
    make_adapter: impl Fn() -> S + Send + Sync + 'static,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>,
    S: StackAdapter;

async fn process_one_committed_block<Node, S>(
    ctx: &ExExContext<Node>,
    chain: &reth_ethereum::provider::Chain,                              // = Chain<EthPrimitives>
    block: &reth_ethereum::primitives::RecoveredBlock<reth_ethereum::Block>,
    cfg: &PassbookConfig,
    make_adapter: &(impl Fn() -> S + Send + Sync + 'static),
) -> Result<BlockBatch, ProcessingError>
where Node: FullNodeComponents,
      Node::Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>,
      S: StackAdapter;
// ^ obtains the REAL parent-block state provider itself, from the node
//   provider, and threads it into the inner fn:
//     use reth_ethereum::storage::StateProviderFactory;          // trait
//     let parent_hash  = chain.first().header().parent_hash();   // BlockHeader
//     let parent_state = ctx.provider()
//         .history_by_block_hash(parent_hash)?;   // -> StateProviderBox
//   `Node::Provider: FullProvider: StateProviderFactory` already, so NO new
//   trait bound is needed (just `use ...StateProviderFactory;` in scope).

// Node-agnostic, ExExContext-free core (unit/integration-testable; reused
// verbatim by 8.4/8.5). NEW trailing arg `parent_state`:
#[doc(hidden)]
pub fn process_committed_block_inner<S: StackAdapter>(
    chain_id: u64,
    chain_spec: Arc<reth_ethereum::chainspec::ChainSpec>,
    chain: &reth_ethereum::provider::Chain,
    block: &reth_ethereum::primitives::RecoveredBlock<reth_ethereum::Block>,
    cfg: &PassbookConfig,
    adapter: &S,
    parent_state: reth_ethereum::storage::StateProviderBox, // NEW (Task 6.4 rework)
) -> Result<BlockBatch, ProcessingError>;
// `parent_state` MUST be the historical post-state of the committed
// chain's parent block (`chain.first().parent_hash`) = the pre-state of
// the chain's first block. Re-exec wraps it in `StateProviderDatabase` and
// layers in-chain blocks `< block.number` on top (see "parent-state
// re-execution" below). 8.4/8.5 obtain it exactly as `process_one_..`
// does (the call site there is the canonical example).
```

- **`make_adapter` refined** from the Task 6.3 placeholder to
  `impl Fn() -> S + Send + Sync + 'static` (a fresh adapter per block).
  The L1 binary (8.4) passes e.g. `|| EthereumStack`, the OP binary
  (8.5) a closure producing an `OpStack`. Call site in `run_passbook`
  unchanged except it now passes `chain.as_ref()` (committed_chain is
  `Arc<Chain>`; the fn takes `&Chain`).
- **Node bound narrowed**: `Node::Types: NodeTypes<Primitives =
  EthPrimitives, ChainSpec = ChainSpec>`. `notification.committed_chain()`
  is `Arc<Chain<E::Primitives>>`; naming the concrete `Chain`
  (`= Chain<EthPrimitives>`) + `RecoveredBlock<reth_ethereum::Block>`
  (research idiom #1 came true) requires fixing the primitives. The
  node-generic ExEx-loop body itself is unchanged; this only constrains
  the EVM/primitive set to the Ethereum one (the OP binary, 8.5, will
  supply its own equivalent — the loop scaffold remains shared).

### Verified provider / EVM / executor API + deltas vs research idioms

| Concern | Research idiom | What v2.2.0 actually needs |
|---|---|---|
| chain id | `ctx.config.chain.chain().id()` | `ctx.config.chain.chain_id()` + `use reth_ethereum::chainspec::EthChainSpec;` (already recorded Task 6.3) |
| per-block split | `chain.execution_outcome_at_block(n)` | **exact**: `reth_ethereum::provider::Chain::execution_outcome_at_block(n) -> Option<ExecutionOutcome>` (early-returns the full outcome when `n == tip`, else `clone().revert_to(n)`). Used verbatim. |
| receipts/logs | `chain.blocks_and_receipts()` | `ExecutionOutcome::receipts_by_block(n) -> &[Receipt]`; `Receipt = alloy_consensus::EthereumReceipt` with **public** `cumulative_gas_used: u64`, `logs: Vec<alloy_primitives::Log>`, `success: bool`. `log.topics()` / `log.data.data`. |
| bundle deltas / gate | `outcome.bundle_accounts_iter()` → `.original_info`/`.info` | **exact**: `bundle_accounts_iter() -> (Address, &BundleAccount)`; `BundleAccount.original_info` / `.info` are `Option<AccountInfo>` (revm-database 13.0.1); balance/nonce delta + gate from these. |
| header accessors | `block.header().number()` etc | need `use alloy_consensus::{BlockHeader, Transaction};` in scope — `.number()`, `.base_fee_per_gas()`, `tx.effective_gas_price(base_fee)` are trait methods (`E0599` without the import). |
| tx hash / sender | — | `block.body().transactions[i].tx_hash()`; `block.transactions_with_sender() -> (&Address, &TxSigned)` (recovered senders). |
| re-exec EVM config | use `ctx`'s `ConfigureEvm` | **CRITICAL DELTA**: under the ExEx **test harness** `ctx.evm_config()` / the node's `ConfigureEvm` is `MockEvmConfig = NoopEvmConfig<EthEvmConfig>` whose `inner()` is `unimplemented!()` — calling it panics. Re-execution therefore builds a **fresh real `EthEvmConfig::new(ctx.config.chain.clone())`** from the chain spec, never `ctx.evm_config()`. (Production `ctx.evm_config()` would work but the chain-spec path is correct for both and harness-safe.) |
| pre-state for re-exec | state provider at `block.parent_hash()` via `StateProviderDatabase` | **EXACT (Task 6.4 rework — the research idiom was right; the earlier `CacheDB<EmptyDB>`-from-own-bundle "improvement" was a CRITICAL DEFECT — see "parent-state re-execution" below). `reth`'s `BundleState` (revm-database 13.0.1) contains ONLY accounts/slots the block WROTE; an account/contract a tx merely READS produces no transition. Rebuilding pre-state from the block's own bundle therefore leaves every read-only account at `EmptyDB` default (balance 0 / empty code / zero storage) ⇒ re-exec diverges ⇒ wrong frames/gas ⇒ residual ⇒ ExEx stalls on ~every real block. Fix: pre-state = the REAL historical post-state of the committed chain's parent block — `ctx.provider().history_by_block_hash(chain.first().parent_hash())? -> StateProviderBox`, wrapped `revm::database::State::builder().with_database(StateProviderDatabase::new(parent_state)) .with_bundle_update().build()`. Pruning-independent / no archive node (the parent of the just-committed tip is the previous committed block, kept in plain/latest state on any full node). For a multi-block committed `Chain`, block N's `CacheDB` is overlaid with the cumulative `chain.execution_outcome_at_block(N-1)` BundleState **post**-values (`acct.info`, slot `present_value`, `bundle.contracts`) so it re-execs vs `(parent-of-chain state + in-chain blocks < N)`; the READ fallback always reaches the real provider, never `EmptyDB`. `StateProviderDatabase` ∈ `reth_ethereum::evm::revm::database`; `StateProviderBox`/`StateProviderFactory` ∈ `reth_ethereum::storage`.** |
| inspector-driven block exec | `evm_config.create_executor(evm_with_env_and_inspector(..), context_for_block(..)).execute_block(block.transactions_recovered())` | **CRITICAL DELTA**: that `BlockExecutor::execute_block` path uses `EthEvm::transact` whose nested call frames do **NOT** fire `Inspector::call`/`create` — only the top-level tx call is inspected, so **internal value transfers are never captured**. The correct primitive is reth's own block-trace path: `evm_config.evm_factory().create_tracer(&mut state, evm_env, inspector).try_trace_many(block.transactions_recovered(), |ctx| …).commit_last_tx()` (`EvmFactoryExt`/`TxTracer` from `reth_ethereum::evm::primitives::{evm::EvmFactoryExt, tracing::TracingCtx}`). `TxTracer`'s per-tx `evm.transact` routes through `EthEvm::transact_raw → inspect_tx` (the tracer's evm is built with `create_evm_with_inspector → activate_inspector`, i.e. `inspect=true`) → `MainnetHandler::inspect_run` → `inspect_run_exec_loop` → `inspect_frame_init` calls `frame_start` (→ `inspector.call`) for **every** frame, **before** `frame_init`'s empty-bytecode `Stop` short-circuit (`revm-handler 18.1.0 frame.rs:227-229`, `revm-inspector 19.0.0 traits.rs:99-107`). So nested CALL/CALLCODE/CREATE/CREATE2/SELFDESTRUCT frames — incl. a plain value `CALL` to a codeless EOA — DO fire the inspector. (The earlier "only top-level fires" was an artifact of the `CacheDB<EmptyDB>` pre-state above: with the contract's code missing, the top-level call hit empty bytecode and never produced the nested CALL at all — fixed by the real parent-state pre-state.) Pre-execution system changes (Cancun beacon root etc.) are applied first via `evm_config.create_executor(evm_with_env(&mut state, evm_env.clone()), context_for_block(block.sealed_block())?).apply_pre_execution_changes()`. `ConfigureEvm`/`EvmFactory`/`BlockExecutor`/`Executor` traits must be in scope. |
| inspector `Clone` | — | `ValueInspector` + the `TaggingInspector` wrapper must be `Clone` (the tracer clones/fuses the inspector between txs; per-tx index is known from `try_trace_many`, so the wrapper only tracks per-tx call depth to mark top-level vs internal). |
| frame tagging | mark `tx:<i>` top-level, seq path internal | top-level (tx depth-0) frame's `trace_path` rewritten to `"tx:<i>"` (→ attribution `EthKind::TopLevel`); nested frames keep the `ValueInspector` seq path (→ `EthKind::Internal`). |

### revm 38 internal-frame capture — internal value CALLs (incl. to codeless EOA) ARE captured (load-bearing)

**Correction (Task 6.4 rework).** An earlier note here claimed a plain
value `CALL` to a codeless EOA does not surface a nested `Inspector::call`
frame at this stack version (citing `revm-handler 14.1.0`). That is
**FALSE** — the project pins `revm-handler 18.1.0` / `revm-inspector
19.0.0` / `revm-interpreter 35.0.1`, and the pinned source shows the
opposite, **confirmed empirically** by the Task 6.4 integration test
(contract → watched-EOA plain value `CALL` produces an `eth_transfers`
`kind=internal, direction=in` row with zero residual):

- `revm-interpreter 35.0.1 src/instructions/contract.rs:135-185`: the
  `CALL` opcode emits `FrameInput::Call{ value: CallValue::Transfer(v) }`
  **unconditionally** — no codeless-target check.
- `revm-inspector 19.0.0 src/traits.rs:99-107` (`inspect_frame_init`)
  calls `frame_start` (→ `inspector.call`) for **every** frame init
  (top-level and nested), **before** `self.frame_init`.
- The empty-bytecode `Stop` short-circuit is in `revm-handler 18.1.0
  src/frame.rs:227-229`, i.e. **downstream** of that inspector hook.
- The tracer's per-tx `evm.transact` reaches this via `EthEvm::transact_raw
  → inspect_tx → MainnetHandler::inspect_run → inspect_run_exec_loop`
  (the tracer evm is built `create_evm_with_inspector → activate_inspector`
  ⇒ `inspect=true`).

So `Inspector::call` DOES fire for an internal value CALL to a codeless
EOA. The previous "only top-level fires" symptom was **not** a revm
limitation — it was an artifact of the (now-removed) `CacheDB<EmptyDB>`
pre-state: with the forwarding contract's code absent from the rebuilt
pre-state, the top-level call hit empty bytecode (21000-gas no-op) and the
nested CALL was never produced. Re-executing against the real
parent-block state provider (see the table row above) makes the code
present, the nested CALL executes, and the hook fires.

- **CALL / CALLCODE / CREATE / CREATE2 / SELFDESTRUCT value transfers —
  including contract→EOA plain `CALL` — ARE all captured** as internal
  frames. No "known limitation"; **no balance-diff fallback** is used or
  needed (reconciliation is not weakened).
- The Task 6.4 integration test exercises BOTH a `SELFDESTRUCT` forwarder
  AND a plain-`CALL` forwarder to the codeless watched EOA `W`, plus a
  2-block chain where block 2 calls a contract **deployed in block 1**
  (its runtime code lives only in block 1's `BundleState`, read but not
  modified by block 2) and still reconciles to zero — proving the
  read-only parent/in-chain-overlay state path.

### `reth_exex_test_utils` API used (pinned v2.2.0)

- `test_exex_context_with_chain_spec(Arc<ChainSpec>) -> (ExExContext<Adapter>, TestExExHandle)`
  (and `test_exex_context()` = mainnet). The harness EVM is
  `MockEvmConfig` (see CRITICAL DELTA above). `init_genesis` seeds genesis
  into the harness `BlockchainProvider`, so `ctx.provider()
  .history_by_block_hash(genesis_hash)` returns a **real** genesis-state
  provider — exactly the parent-state pre-state the Task 6.4 rework
  re-executes against. `TestExExHandle::provider_factory
  .history_by_block_hash(genesis_hash)? -> StateProviderBox` gives the
  same provider for the deterministic direct `process_committed_block_inner`
  check. The default test-utils notification stream is `WithoutHead` (no
  backfill), so the synthetic committed chain need not be in the provider
  DB — its blocks are parented at genesis (single-block: parent = genesis;
  2-block: block 1 parent = genesis, block 2 re-execs vs genesis provider +
  block 1's in-chain BundleState overlay).
- **`ctx.config` is built from `NodeConfig::test()` (default mainnet
  spec), NOT from the `chain_spec` argument.** For an end-to-end
  `run_passbook` drive against a custom chain spec the test sets
  `ctx.config.chain = chain_spec` (field is `pub Arc<ChainSpec>`) so
  `run_passbook`'s `ctx.config.chain.chain_id()` + re-exec hardforks
  match the committed block. In production `ctx.config.chain` is already
  the node's real spec; **8.4/8.5 need no such override**.
- `TestExExHandle::send_notification_chain_committed(Chain)` (consumes a
  `Chain`); `handle.events_rx: UnboundedReceiver<ExExEvent>` polled for
  `ExExEvent::FinishedHeight(BlockNumHash)`.
- The synthetic `Chain` is built by **genuinely executing** a hand-built
  Shanghai block: `EthEvmConfig::new(spec).executor(state).execute(&recovered_block)`
  → `BlockExecutionOutput`; `ExecutionOutcome::single(1, out)`;
  `Chain::new(vec![recovered], outcome, BTreeMap::new())` (empty
  `trie_data` is fine — `execution_outcome_at_block(tip)` early-returns
  before touching it). Because BundleState + receipts come from one real
  execution, reconciliation is consistent **by construction**.
- Block built with `alloy_consensus::{Block, BlockBody, Header}` +
  `RecoveredBlock::try_recover`; txs signed with
  `alloy_signer_local::PrivateKeySigner` +
  `alloy_network::TxSignerSync::sign_transaction_sync`; genesis via
  `alloy_genesis::{Genesis, GenesisAccount, ChainConfig}` →
  `ChainSpec::from_genesis`. The default test-utils notifications stream
  is **`WithoutHead`** (no `with_head`), so it passes the committed
  `Chain` straight through without backfill — the synthetic chain need
  not be in the provider DB.

### Dev-dependencies added (Cargo.lock impact: 3 lines, justified)

`alloy-signer-local`, `alloy-network`, `alloy-genesis` (all `= "2.0.4"`,
the exact versions the reth v2.2.0 graph already resolved — pulled
transitively by `reth-exex-test-utils`' alloy stack). Added to the pin
table + `passbook-core` `[dev-dependencies]`; `cargo update -p
passbook-core --offline` locked **0 packages** (no version/rev change) —
the only `Cargo.lock` change is the 3 new dependency-edge lines under the
`passbook-core` package entry. `--locked` stays green; pinned reth/op
revs untouched.

## jsonrpsee 0.26 RPC API (Task 7.1 — load-bearing for Tasks 8.4 / 8.5)

The `passbook` namespace lives in `crates/passbook-core/src/rpc.rs`. Tasks
8.4/8.5 register it into reth's RPC server via `extend_rpc_modules` using
`PassbookApiServer::into_rpc()`. The exact pinned-jsonrpsee-0.26 API used
(verified against the registry source at
`~/.cargo/registry/src/index.crates.io-*/jsonrpsee-{proc-macros,core,types}-0.26.0`):

- **Macro:** `#[jsonrpsee::proc_macros::rpc(server, namespace = "passbook")]`
  on a `pub trait PassbookApi` with `#[method(name = "...")]` on each fn.
  Default namespace separator `_`, so the wire method names are
  `passbook_health` and `passbook_getTransfers`.
  (`jsonrpsee-proc-macros-0.26.0/src/lib.rs:64` — server trait is named
  `<Trait>Server`.)
- **Server trait:** the macro generates **`PassbookApiServer`** (input trait
  name + `Server`). It supplies a provided `fn into_rpc(self) ->
  RpcModule<Self>` — that is the registration entry point for 8.4/8.5.
- **Result type:** `jsonrpsee::core::RpcResult<T>` =
  `Result<T, jsonrpsee::types::ErrorObjectOwned>`
  (`jsonrpsee-core-0.26.0/src/lib.rs:67`).
- **`#[async_trait::async_trait]` IS required** on the `impl
  PassbookApiServer for PassbookRpc` block. jsonrpsee 0.26 still desugars
  async trait methods via `async-trait`; `jsonrpsee::core::async_trait`
  (`jsonrpsee-core-0.26.0/src/lib.rs:63`) re-exports the same macro. We use
  the direct `async-trait` workspace dep — either is equivalent.
- **Error constructor:** `jsonrpsee::types::ErrorObjectOwned::owned(code:
  i32, message: impl Into<String>, data: Option<S: Serialize>)`
  (`jsonrpsee-types-0.26.0/src/error.rs:70`). We use code `-32000`
  (application-error range) and `None::<()>` for data.
- **DELTA vs the Task 7.1 candidate (the one material difference):**
  jsonrpsee 0.26's `IntoResponse` blanket impl
  (`jsonrpsee-core-0.26.0/src/server/mod.rs:63`) bounds the success type
  `T: serde::Serialize + Clone`. The candidate's return types (`Health`,
  `TransfersPage`, and transitively `TransferRowOut`) derived only
  `Serialize`, so the `#[rpc]` macro failed to compile with `the trait
  bound TransfersPage: Clone is not satisfied`. **Fix:** added `Clone` to
  the `#[derive(...)]` of `Health`, `TransferRowOut`, `TransfersPage` in
  `queries.rs` (now `#[derive(Debug, Clone, Serialize)]`). No other
  deviation from the candidate — server trait name, `async_trait` need,
  error type, and macro form are all exactly as the candidate predicted.
- **Errors never swallowed:** every fallible step (mutex lock poison; the
  `health` / `get_transfers` query) is mapped through the `err(...)` helper
  into an `ErrorObjectOwned` and returned as a JSON-RPC error. There is no
  `unwrap`, no `Default`, no empty-result-on-error path. A unit test
  (`poisoned_lock_is_a_jsonrpc_error_not_swallowed`) poisons the shared
  mutex and asserts the call returns an error with code `-32000` rather
  than panicking or returning an empty page.
- `PassbookRpc { ledger: Arc<Mutex<Ledger>>, chain_id: u64 }` holds the
  **same** `Arc<Mutex<Ledger>>` the ExEx writer uses; the RPC side is
  read-only (the ExEx is the sole writer). `getTransfers` calls
  `get_transfers(..., 500)` (server-side soft page target).

## `get_transfers` is now block-complete (Task 1.6 plan amendment — RESOLVED)

The Task 1.6 review flagged that the original `get_transfers` applied
`LIMIT lim` to each of the 4 category queries independently, merged, sorted
by block, and set `next_cursor = last_block + 1` — which **silently skipped
caller rows** in two ways: (a) per-category truncation when one category
alone exceeded `lim` in the window; (b) the `+1` skipped the remainder of a
block whose rows straddled the page boundary.

**Amendment resolved in Task 7.1.** `crates/passbook-core/src/ledger/queries.rs`
now issues a single `UNION ALL` over the 4 category subqueries (projected to
the common `TransferRowOut` shape), ordered by `(block_number, category,
rowid)` — a total deterministic order — and uses a **block-complete cursor**:
fetch `lim+1` rows; if `<= lim` the page is the whole remaining stream
(`next_cursor = None`); if `lim+1`, a block is never split — emit all rows of
fully-present blocks and resume the cursor at the first untouched block, and
when a single block alone exceeds `lim` re-query that block in full so it is
emitted whole (the page may legitimately exceed `lim` to complete a block).
The invariant — following `next_cursor` to `None` yields every matching row
exactly once, no skip, no dup, blocks never split — is proven by 5 unit
tests in `queries.rs` (single-category > lim across blocks; single block
> lim at a page boundary never split; multi-category merged/complete; kind
filter; empty/out-of-range). The `TransferRowOut` / `TransfersPage` /
`Health` shapes are unchanged except for the added `Clone` derive (required
by jsonrpsee 0.26, above). The `kind` filter still selects the *category*
(`None` = all; `Some("eth"|"erc20"|"gas"|"unattributed")`); the eth-internal
`kind` sub-values (`top_level`/`internal`/`system`) are surfaced verbatim in
the output `kind` field and were never used as a filter value (behaviour
preserved).

## reth-optimism-evm L1-fee API (Task 8.2, for 8.5)

Verified by reading the pinned source at optimism monorepo rev
`27bf9194a08aef70f3fdbff6b3d04bdd70af62ff` (crate `reth-optimism-evm`,
`reth-op 1.11.3`). File: `rust/op-reth/crates/evm/src/l1.rs`, accessed via
the local mirror `.vendor/optimism/rust/op-reth/crates/evm/src/l1.rs` (also
present at `~/.cargo/git/checkouts/optimism-*/.../crates/evm/src/l1.rs`).
`lib.rs:56-57` does `pub mod l1; pub use l1::*;` so everything below is
reachable as `reth_optimism_evm::<name>` (the `l1::` prefix is optional).

**Extract L1 block info (once per L2 block):**

```text
// l1.rs:25
pub fn reth_optimism_evm::extract_l1_info<B>(body: &B)
    -> Result<op_revm::L1BlockInfo, reth_optimism_evm::OpBlockExecutionError>
where B: reth_primitives_traits::BlockBody
```

Returns `Err(OpBlockExecutionError::L1BlockInfo(L1BlockInfoError::MissingTransaction))`
on an empty block. The L1-info transaction is always tx index 0 of the L2
block. Variant also available: `extract_l1_info_from_tx::<T: alloy_consensus::Transaction>(tx: &T)`
(`l1.rs:37`) and `parse_l1_info(input: &[u8])` (`l1.rs:57`).

**Per-tx L1 data fee (trait `RethL1BlockInfo`, `l1.rs:295`; impl for
`op_revm::L1BlockInfo` at `l1.rs:325`):**

```text
fn reth_optimism_evm::RethL1BlockInfo::l1_tx_data_fee(
    &mut self,
    chain_spec: impl reth_optimism_forks::OpHardforks,
    timestamp: u64,
    input: &[u8],          // EIP-2718 encoded raw tx bytes (tx.encoded_2718())
    is_deposit: bool,
) -> Result<alloy_primitives::U256, reth_execution_errors::BlockExecutionError>
```

- `is_deposit == true` short-circuits to `Ok(U256::ZERO)` (`l1.rs:333`) —
  so deposit txs (incl. the index-0 L1-info tx) have **zero** L1 data fee;
  Passbook records these as `None` (not present), not `Some(0)`.
- Receiver is `&mut self` (the call mutates cached state in `L1BlockInfo`),
  so the extracted `L1BlockInfo` must be held `mut` and reused across the
  block's txs.
- `input` is the **2718-encoded** raw tx (confirmed by the crate's own test
  `l1.rs:402-424`, which decodes a 2718 tx and feeds it through). Use
  `alloy_eips::eip2718::Encodable2718::encoded_2718(&tx)`.
- Sibling `l1_data_gas(&self, chain_spec, timestamp, input)` (`l1.rs:317`)
  returns the data-gas component only — not needed for Passbook.

**Inputs Task 8.5 must obtain:** the committed block (`block.body` for
`extract_l1_info`, `block.header.timestamp` for `timestamp`, per-tx
`tx.is_deposit()` + `tx.encoded_2718()`), and the OP chain spec (an
`Arc<OpChainSpec>` from the node context; `OpChainSpec: OpHardforks`).

**Why no compiling end-to-end helper in `passbook-stack-optimism`:** the
fully-typed signature names `op_revm::L1BlockInfo`, `reth_optimism_forks::OpHardforks`
and `reth_execution_errors::BlockExecutionError`. Those are NOT direct
dependencies of `passbook-stack-optimism` (deps are only `passbook-core`,
`reth-optimism-evm`, `alloy-primitives`, `alloy-consensus`), and adding them
would mutate the committed `Cargo.lock` (forbidden for Task 8.2). They ARE
reachable at the Task 8.5 OP-binary call site, where the concrete
`OpChainSpec` type is in scope. Task 8.2 therefore ships:
`passbook_stack_optimism::build_block_l1_fee_table(txs: impl IntoIterator<Item=(bool /*is_deposit*/, Vec<u8> /*raw_2718*/)>, fee_of: impl FnMut(&[u8]) -> Option<U256>) -> OptimismStack`
— it owns the deposit→`None` rule and the positional-table construction;
Task 8.5 only supplies `fee_of` as a closure wrapping
`l1_block_info.l1_tx_data_fee(&chain_spec, ts, raw, false).ok()` (deposits
are already filtered to `None` by `build_block_l1_fee_table`). Then
`OptimismStack::from_fees`/`build_block_l1_fee_table` feeds
`passbook_core::stack::StackAdapter`. No re-discovery needed for 8.5.

## Task 8.4 binary wiring (L1 `reth-passbook` — load-bearing for Task 8.5)

How `crates/bin/reth-passbook/src/main.rs` was wired against pinned reth
**v2.2.0** (`paradigmxyz/reth` `88505c7…`, facade `reth-ethereum 2.2.0`).
Task 8.5 (the OP binary) mirrors this structure with `reth-op`'s CLI / node /
chainspec-parser — the shape below is the template; only the facade-crate
paths and the `make_adapter` closure differ.

### Exact reth v2.2.0 CLI / parser / run / hook API used

| Concern | API the binary compiles against | Source |
|---|---|---|
| CLI type | `reth_ethereum::cli::Cli::<C, Ext>` (`pub use reth_ethereum_cli::interface::{Cli, ..}` via the facade's `cli` mod, which is `pub use reth_ethereum_cli::*`) | `crates/ethereum/cli/src/lib.rs:18`, `interface.rs:34` |
| Chain-spec parser | `reth_ethereum::cli::chainspec::EthereumChainSpecParser` (the facade `cli` mod re-exports `reth_ethereum_cli::*`, whose `pub mod chainspec` holds it — **NOT** in `reth_ethereum::chainspec`, which is `reth_chainspec`) | `crates/ethereum/cli/src/chainspec.rs:26` |
| Parse | `Cli::<EthereumChainSpecParser, PassbookArgs>::parse()` — `Cli` is `#[derive(Parser)]`, so `clap::Parser` must be in scope (`use clap::Parser;`) | `interface.rs:31` |
| Run | `.run(launcher)` where `launcher: FnOnce(WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>, Ext) -> Fut`, `Fut: Future<Output = eyre::Result<()>>`. Used as `.run(async move |builder, args: PassbookArgs| { … })` — an **`async` closure** (edition-2024 `async ||`, exactly the form in reth's own rustdoc example `interface.rs:103`). Requires `C: ChainSpecParser<ChainSpec = ChainSpec>` (the concrete reth `ChainSpec`) — `EthereumChainSpecParser` satisfies it. | `interface.rs:130-138` |
| Stock node | `builder.node(EthereumNode::default())` → `WithLaunchContext<NodeBuilderWithComponents<…>>`; `.launch().await?` → `NodeHandle`; `handle.wait_for_node_exit().await` | `crates/node/builder/src/builder/mod.rs:388,727`; `handle.rs:23` |
| chain id | `builder.config()` → `&NodeConfig<ChainSpec>`; field `pub chain: Arc<ChainSpec>` (`node_config.rs:100`); `ChainSpec: EthChainSpec` ⇒ `.chain_id() -> u64`, **requires `use reth_ethereum::chainspec::EthChainSpec;` in scope** (same trait-in-scope rule as Task 6.3/6.4) | `crates/node/core/src/node_config.rs:100`; `crates/chainspec/src/api.rs` |
| RPC hook | `.extend_rpc_modules(move |ctx| { ctx.modules.merge_configured(PassbookApiServer::into_rpc(PassbookRpc{ ledger, chain_id }))?; Ok(()) })` — `F: FnOnce(RpcContext<…>) -> eyre::Result<()> + Send + 'static`; `RpcRegistry::merge_configured` at `crates/rpc/rpc-builder/src/lib.rs:1794` | `crates/node/builder/src/builder/mod.rs:631` |
| ExEx hook | `.install_exex("passbook", move |ctx| async move { Ok(run_passbook(ctx, cfg, ledger, \|\| EthereumStack::default())) })` — `F: FnOnce(ExExContext<…>) -> R + Send + 'static`, `R: Future<Output = eyre::Result<E>> + Send`, `E: Future<Output = eyre::Result<()>> + Send`. So the closure body is an `async move` block that returns `Ok(<the run_passbook future>)`; `run_passbook` is **not** `.await`-ed here — its returned future is the inner `E` reth drives. | `crates/node/builder/src/builder/mod.rs:645` |

`make_adapter` is `|| EthereumStack::default()` (the established
`impl Fn() -> S + Send + Sync + 'static` from `exex.rs`; `EthereumStack` is
`Default`). `run_passbook`'s `Node::Types: NodeTypes<Primitives =
EthPrimitives, ChainSpec = ChainSpec>` bound is satisfied by `EthereumNode`.

### Deltas vs the Task 8.4 candidate

- **Only one path correction.** `EthereumChainSpecParser` is **not** under
  `reth_ethereum::chainspec` (that mod is `reth_chainspec::*`). It is under
  the facade's `cli` mod: `reth_ethereum::cli::chainspec::EthereumChainSpecParser`
  (the `cli` mod is `pub use reth_ethereum_cli::*`, and `reth_ethereum_cli`
  has `pub mod chainspec`). `Cli` itself is reachable as
  `reth_ethereum::cli::Cli` (re-exported from `interface`). Everything else
  (the `async move |builder, args|` closure shape, `builder.config().chain
  .chain_id()`, `builder.node(EthereumNode::default())`,
  `extend_rpc_modules`/`merge_configured`, `install_exex` returning
  `Ok(future)`, `wait_for_node_exit`) matched the candidate verbatim.
- The custom flag lands on the **`node` subcommand** (`reth-passbook node
  --help` shows `--passbook.addresses` / `--passbook.db-path`), NOT the
  top-level `--help`. This is correct/by-design: reth's `Cli`'s `Ext` is
  flattened into `node::NodeCommand<C, Ext>` (`interface.rs:264`), so the
  closure's second arg is the parsed `PassbookArgs`. The top-level `--help`
  is the full stock reth CLI (all subcommands: `node`, `init`, `db`, …) —
  i.e. it is a real reth binary with the flag added, not a reskinned one.
  Task 8.4's smoke check therefore targets `node --help`.

### chain_id / `Ledger::open` ordering — chosen approach

**Resolve before launch; one shared handle; no `OnceCell`/oneshot.** The
reth `builder` exposes the fully-configured chain spec at
`builder.config().chain` (`Arc<ChainSpec>`) **before** `launch()`, and
`EthChainSpec::chain_id()` yields the `u64`. So on the main task, when
enabled, the binary: (1) `chain_id = builder.config().chain.chain_id()`;
(2) `Ledger::open(&cfg.db_path, chain_id)?` once; (3) wraps it in one
`Arc<Mutex<Ledger>>`; (4) moves a clone into the `extend_rpc_modules` hook
(`PassbookRpc`) and another clone into the `install_exex` closure
(`run_passbook` writer). The RPC reader and the ExEx writer hold the **same**
`Arc<Mutex<Ledger>>`. The deferred-open / `OnceCell` alternative (open inside
the ExEx where `ctx.config.chain.chain_id()` is available) is unnecessary
because the chain id is already available pre-launch and there is exactly one
opener. **Task 8.5 should use the identical pattern** with `reth-op`'s
`builder.config().chain` (its OP chain spec also implements `EthChainSpec`).

### Drop-in safety — three cases (spec-critical) & how verified

1. **No / empty addresses** (`!cfg.enabled()`): the binary takes the
   `builder.node(EthereumNode::default()).launch().await?` →
   `wait_for_node_exit().await` path and **returns early before** any
   `extend_rpc_modules`/`install_exex` call. The resulting node is the
   stock reth `EthereumNode` with no `passbook` ExEx and no `passbook` RPC
   namespace — byte-identical to upstream reth. (`PassbookArgs`'
   `--passbook.addresses` `default_value = ""` ⇒ `from_parts` yields an
   empty watched set ⇒ `enabled()` is `false`.)
2. **Malformed address**: `PassbookConfig::from_parts` returns `Err`; the
   `?` in the `run` closure propagates it, `Cli::run` returns that `Err`,
   `main` returns it ⇒ the process exits non-zero **before any node
   starts**. Loud failure, no silent degradation.
3. **Valid addresses** (`cfg.enabled()`): ledger opened once, RPC namespace
   + ExEx installed sharing the one `Arc<Mutex<Ledger>>`, then `launch`.

Verification performed: `cargo build -p reth-passbook --locked` — clean,
**zero warnings**. `reth-passbook node --help` prints both
`--passbook.addresses` and `--passbook.db-path` (FLAG-PRESENT);
`reth-passbook --help` lists the full stock reth subcommand set
(`node`/`init`/`db`/`stage`/…) confirming it is a genuine reth CLI with the
flag added. Cases 1 & 2 are realized by the early-return / `?`-propagation
control flow above (the no-args branch never touches the ExEx/RPC hooks; the
malformed branch errors out of `from_parts` before `builder` is consumed) —
verified by source review of the single `run` closure, which has exactly
these three branches and no other exit. Full workspace still green:
`cargo test -p passbook-core --locked` = 31 unit + 5 integration passed;
`cargo build -p spike --locked` green. **Cargo.lock / Cargo.toml unchanged**
(all deps were pre-wired in Task 0.3 — zero dependency delta).

## Task 8.5 — `op-reth-passbook` + shared `ChainExec` seam (L1/OP)

How the OP binary + the L1/OP chain-abstraction seam were wired against
pinned op-reth (`reth-op 1.11.3`, optimism monorepo `27bf9194…`) and reth
v2.2.0 (`paradigmxyz/reth` `88505c7…`). The spec's hard requirement — ONE
workspace producing BOTH `reth-passbook` (L1) and `op-reth-passbook` (OP)
driving the SAME capture core — is met by a single trait seam; the pure
core is shared, never forked.

### The seam: `passbook_core::exex::ChainExec`

```rust
pub trait ChainExec: Send + Sync + 'static {
    type Primitives: NodePrimitives;                 // EthPrimitives | OpPrimitives
    type ChainSpec: EthChainSpec + Send + Sync + 'static; // ChainSpec | OpChainSpec
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

pub async fn run_passbook<Node, C>(
    ctx: ExExContext<Node>, cfg, ledger, chain_exec: C,
) -> eyre::Result<()>
where C: ChainExec,
      Node: FullNodeComponents,
      Node::Types: NodeTypes<Primitives = C::Primitives, ChainSpec = C::ChainSpec>;
```

- **Chain-AGNOSTIC core stays shared & unchanged**: `process_block`
  (pure orchestrator), `reconcile`, `ledger`, `erc20`,
  `inspector::ValueInspector`, `attribution`, and the `run_passbook`
  safety contract (reorg-first delete, retry-until-success,
  FinishedHeight-only-after-durable-write) are SINGLE-sourced in
  `passbook-core` and **invoked** by every arm — never duplicated. The
  parent-state acquisition (`ctx.provider().history_by_block_hash(
  chain.first().parent_hash)`) is also generic in the shared loop
  (`Node::Provider: FullProvider: StateProviderFactory` holds for ANY
  primitive set — no extra bound).
- **Shared inspector + pre-state overlay**: extracted to
  `passbook_core::reexec` (new `pub` module). `TaggingInspector`
  (EVM-`CTX`-generic `revm::Inspector`), `Captured`/`TaggedFrame`, and
  `build_prestate_cache::<N: NodePrimitives>` (parent-state +
  in-chain-`BundleState` overlay; the overlay touches only the
  primitive-agnostic `BundleState`, so L1 and OP get IDENTICAL pre-state
  semantics, never `EmptyDB`). Both arms reuse these verbatim — the
  value-attribution inspector is NOT forked.
- **L1 arm** = `passbook_core::chain::EthChainExec` (lives in
  `passbook-core`; `reth-ethereum` only ⇒ core stays OP-free). It
  delegates to the **unchanged** Task 6.4
  `process_committed_block_inner` (still `pub`, still called verbatim by
  the 5 integration tests). An ergonomic blanket
  `impl<S: StackAdapter, F: Fn()->S> ChainExec for F` (assoc types =
  `EthPrimitives`/`ChainSpec`) keeps the established `|| L1Adapter` /
  `|| EthereumStack::default()` call-shape working, so the 5 Task
  6.4/6.5 integration tests pass **unchanged** (proof the L1 path is
  behaviour-identical: 31 unit + 5 integration still green). The two L1
  impls (`EthChainExec` struct, `Fn` blanket) are distinct types — no
  coherence overlap; a downstream concrete `OpChainExec` (not an `Fn`)
  also cannot overlap.
- **OP arm** = `passbook_stack_optimism::OpChainExec` (lives in
  `passbook-stack-optimism`, which now depends on `reth-op` — keeping
  `passbook-core` free of `reth-op`/`reth-optimism-*` as default deps,
  matching the existing crate layout). It re-expresses the per-block
  extraction for OP concrete types and invokes the SHARED
  `process_block`.

### OP re-exec / `OpEvmConfig` / primitives API used (verified, pinned)

| Concern | API | Source / delta |
|---|---|---|
| OP node | `reth_op::node::OpNode::new(RollupArgs)` (`reth_op::node = reth_optimism_node::*`). **No `Default`** — constructed with the flattened `RollupArgs`. `OpNode: NodeTypes<Primitives = OpPrimitives, ChainSpec = OpChainSpec>` (`node/src/node.rs:360`) | delta vs L1 `EthereumNode::default()` |
| OP EVM config | `reth_optimism_evm::OpEvmConfig::optimism(Arc<OpChainSpec>)` (`evm/src/lib.rs:112`; analogue of `EthEvmConfig::new`). `impl ConfigureEvm for OpEvmConfig` (`evm/src/lib.rs:190`) ⇒ same `evm_env`/`context_for_block`/`create_executor`/`evm_with_env`/`evm_factory().create_tracer().try_trace_many().commit_last_tx()` surface as the L1 driver | only the constructor + concrete type differ; tracer machinery identical |
| OP primitives | `reth_op::{OpPrimitives, OpBlock}` (`OpBlock = Block<OpTransactionSigned>`); `reth_op::provider::Chain` / `reth_op::primitives::RecoveredBlock` / `reth_op::storage::StateProviderBox` — these are the SAME `reth_provider`/`reth_primitives_traits`/`reth_storage_api` types as the L1 facade re-exports (co-resolution: one reth rev), so `passbook_core::reexec`'s generic helpers accept the OP types unchanged | — |
| OP receipt | `OpReceipt` is an **enum** (`op_alloy_consensus::OpReceipt`, via `reth_op::OpReceipt`); use the `alloy_consensus::TxReceipt` trait (`.status()`, `.logs()`, `.cumulative_gas_used()`) — NOT the L1 `Receipt`'s public struct fields (`success`/`logs`/`cumulative_gas_used`) | delta vs L1 |
| OP tx | `OpTransactionSigned = OpTxEnvelope`; deposit detection via the **inherent** `OpTxEnvelope::is_deposit(&self) -> bool` (`op-alloy-consensus envelope.rs:356`; the `OpTransaction` trait method is `pub(super)` on the reth wrapper so the inherent is used). `tx.tx_hash()` yields `[u8;32]` by value here (NOT `&B256` like L1) ⇒ wrap `B256::from(*tx.tx_hash())`. `tx.encoded_2718()` via `alloy_eips::eip2718::Encodable2718` (`primitives/src/transaction/signed.rs:266`). `tx.effective_gas_price(base_fee)` via `alloy_consensus::Transaction` | deltas vs L1 |
| OP L1 data fee | `reth_optimism_evm::extract_l1_info(block.body()) -> Result<L1BlockInfo, OpBlockExecutionError>` ONCE per L2 block (`evm/src/l1.rs:25`; `Err` only on an empty block ⇒ all-`None` table), then per-tx `RethL1BlockInfo::l1_tx_data_fee(&mut self, chain_spec: impl OpHardforks, timestamp, input: &[u8], is_deposit) -> Result<U256, BlockExecutionError>` (`l1.rs:295/325`). Fed through Task 8.2's `build_block_l1_fee_table(txs:(is_deposit,raw_2718), fee_of)` → `OptimismStack`. `fee_of = |raw| l1_info.l1_tx_data_fee(chain_spec.as_ref(), ts, raw, false).ok().filter(|v| !v.is_zero())` (deposits already `None` by the table; a zero fee ⇒ `None`, not `Some(0)`) | as Task 8.2 predicted; `chain_spec.as_ref(): &OpChainSpec` satisfies `impl OpHardforks` |
| `make_adapter` shape | The OP L1-fee table is INHERENTLY per-block (L1 base/blob scalars change every L2 block), so the OP arm FOLDS L1-fee extraction into the seam (`process_committed_block` builds the `OptimismStack` itself) rather than the old `make_adapter: impl Fn()->S`. The L1 arm keeps the `Fn`-based shape via the blanket impl; the L1 binary unchanged in behaviour | resolves the spec's "evolve the `make_adapter` shape" note |

### OP `Cli` / parser / `RollupArgs` specifics (verified, pinned)

| Concern | API | Source |
|---|---|---|
| CLI type | `reth_op::cli::Cli::<C, Ext>` (`reth_op::cli = reth_cli_util::{..} + reth_optimism_cli::*`; `Cli` from `reth_optimism_cli`) | `optimism/cli/src/lib.rs:66` |
| Chain-spec parser | `reth_op::cli::chainspec::OpChainSpecParser` (the OP analogue of L1's `EthereumChainSpecParser`; `reth_optimism_cli` has `pub mod chainspec`) | `optimism/cli/src/chainspec.rs` |
| `Cli::run` | `L: FnOnce(WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>, Ext) -> Fut`, `C: ChainSpecParser<ChainSpec = OpChainSpec>` — **identical shape to L1**; used as `async move |builder, ext: OpExt| {…}` | `optimism/cli/src/lib.rs:122-138` |
| Default `Ext` is `RollupArgs` | `Cli<Spec = OpChainSpecParser, Ext = RollupArgs>` (`optimism/cli/src/lib.rs:67`). To preserve `--rollup.*` AND add `--passbook.*`, the binary defines `struct OpExt { #[command(flatten)] passbook: PassbookArgs, #[command(flatten)] rollup: RollupArgs }` and passes `ext.rollup` to `OpNode::new` | `reth_op::node::args::RollupArgs` (`node/src/args.rs:22`, `#[derive(clap::Args)]`) |
| chain id | `builder.config().chain` = `Arc<OpChainSpec>`; `OpChainSpec: EthChainSpec` (`optimism/chainspec/src/lib.rs:247`) ⇒ `.chain_id()` with `use reth_op::chainspec::EthChainSpec;` in scope (same trait-in-scope rule as L1) | — |
| RPC / ExEx hooks | `.extend_rpc_modules(...).merge_configured(...)` and `.install_exex("passbook", |ctx| async move { Ok(run_passbook(ctx, cfg, ledger, OpChainExec)) })` — IDENTICAL to L1 (the builder API is node-generic) | matches Task 8.4 verbatim |

Drop-in safety (3 cases) is realised by the SAME early-return /
`?`-propagation control flow as L1 (Task 8.4): no/empty addresses ⇒
stock `OpNode::new(rollup)` with no ExEx/RPC; malformed ⇒ `from_parts`
`Err` aborts before launch; valid ⇒ one shared `Arc<Mutex<Ledger>>` +
RPC namespace + ExEx with `OpChainExec`.

### What OP testing was / was not realized

- **Realized**: `op-reth-passbook` **compiles clean against pinned
  op-reth** (zero warnings); the 3 smoke checks pass —
  `node --help` shows `--passbook.addresses` (FLAG-ON-NODE),
  top-level `--help` lists the stock op-reth subcommand set
  (STOCK-SUBCOMMANDS), and `--rollup.*` survives on `node --help`
  (RollupArgs preserved). OP unit tests: a type-level proof that
  `OpChainExec: ChainExec` with `Primitives = OpPrimitives` /
  `ChainSpec = OpChainSpec` (so `run_passbook`'s `OpNode` bound is
  satisfiable), and a test of the OP backend's per-block L1-fee table
  (deposit→`None`, positive→`Some`, zero→`None`, out-of-range→`None`).
- **Not realized** (honest bar): a full OP `reth_exex_test_utils`
  end-to-end integration test (an executed OP `Chain<OpPrimitives>` with
  an L1-info deposit tx + an ERC20/value tx, asserting
  `gas_payments.l1_fee_wei` populated + zero residual). The pinned
  `reth-exex-test-utils` harness in this workspace's dev-deps is
  Eth-typed (`test_exex_context_with_chain_spec` over `ChainSpec`); an
  OP equivalent + an OP signed-block/genesis builder would require NEW
  dev-dependencies, mutating `Cargo.lock` with new packages (outside the
  additive-only constraint for this task). What IS proven for the OP
  path: the binary compiles+wires against real pinned op-reth; the OP
  L1-fee logic is unit-tested; and the deep re-exec/reconcile pipeline
  is the **same shared code** the 5 L1 integration tests exercise
  green — the OP driver differs only in the concrete `OpEvmConfig` +
  receipt/tx accessors, reusing `passbook_core::reexec`'s
  `TaggingInspector` / `build_prestate_cache` and the pure
  `process_block` verbatim.

### Cargo.lock delta

**Additive dep-edges only, zero package/version/source change.** Adding
`reth-op`, `alloy-eips`, `revm`, `eyre`, `tracing` (all already in the
graph at the pinned versions/revs) as deps of `passbook-stack-optimism`
adds exactly 5 lines to that crate's `dependencies` array in
`Cargo.lock` (`alloy-eips 2.0.4`, `eyre`, `reth-op`, `revm`, `tracing`).
Package count unchanged (998 → 998); no `version`/`source` line changed.
Structurally required so the OP `ChainExec` arm can live in the OP stack
crate (keeping `passbook-core` OP-free), consistent with the existing
per-stack crate boundary.

## B1 — recognized system-event APIs (withdrawals / beneficiary priority-fee / OP deposit-mint / fee-vault)

Spec `passbook-spec.md` §(b)/(c) requires L1 beacon withdrawals, the
post-merge beneficiary "block reward", and OP deposit mints to be
attributed `kind = system` with **zero reconciliation residual** (only a
*truly* unexplained delta is the stall case). The exact pinned APIs used
by the `ChainExec` seam to surface these (file:line in the pinned
sources):

### L1 (fully implemented — `EthChainExec` / `process_committed_block_inner`)

- **Beacon withdrawals** — `alloy_consensus::Block` body field
  `withdrawals: Option<Withdrawals>`
  (`alloy-consensus 2.0.4 src/block/mod.rs:240`). Each
  `alloy_eips::eip4895::Withdrawal { address: Address, amount: u64 }`
  (`alloy-eips 2.0.4 src/eip4895.rs:19,30,33`); GWEI→wei via the inherent
  `Withdrawal::amount_wei(&self) -> U256`
  (`src/eip4895.rs:38`, `= amount * GWEI_TO_WEI(1e9)`,
  `src/eip4895.rs:11`). Reached as `block.body().withdrawals` exactly as
  the existing integration fixtures already build blocks.
- **Post-merge block "reward" = Σ beneficiary priority fee** — there is
  **no captured CALL frame**. Computed as `Σ over included txs of
  (effective_gas_price − base_fee_per_gas) × gas_used`, where
  `effective_gas_price` is `alloy_consensus::Transaction::
  effective_gas_price(base_fee)` (already used verbatim by the gas path),
  `base_fee_per_gas` is `alloy_consensus::BlockHeader::
  base_fee_per_gas()`, `gas_used` is the per-tx delta of
  `receipt.cumulative_gas_used` (same derivation the gas path uses), and
  the credit target is `BlockHeader::beneficiary()`. Pre-1559 / missing
  base fee ⇒ `base_fee = 0` (full `effective_gas_price × gas_used`).
  **Pre-merge fixed block rewards are intentionally OUT OF SCOPE**
  (forward-only on post-merge networks) and are NOT synthesised — a
  watched miner on a *pre-merge* chain's fixed reward would be a
  genuinely-unexplained residual (correct stall).

The chain-agnostic arithmetic is `passbook_core::system::
l1_system_credits` (unit-tested); the seam feeds it the extracted
withdrawal list + per-tx `(effective_gas_price, gas_used)` pairs.

### OP (deposit-mint implemented; fee-vault is a bounded, disclosed limitation)

- **Deposit mints (IMPLEMENTED)** — an OP deposit tx is
  `op_alloy_consensus::OpTxEnvelope::Deposit(Sealed<TxDeposit>)`; the
  recipient + minted wei come from
  `TxDeposit { mint: u128, to: TxKind, from: Address, .. }`
  (op-alloy-consensus monorepo rev `27bf9194`
  `rust/op-alloy/crates/consensus/src/transaction/deposit.rs:19,27,30`).
  Accessed via the **inherent** `OpTxEnvelope::as_deposit(&self) ->
  Option<&Sealed<TxDeposit>>`
  (`.../transaction/envelope.rs:436`) — inherent, so **no new op-alloy
  dependency edge** is required (`OpTransactionSigned` is a type alias
  for `OpTxEnvelope`, `op-reth/crates/primitives/src/transaction/
  mod.rs:15`; `as_deposit`/`is_deposit` are inherent on it). A deposit
  whose `to = TxKind::Call(addr)` with `addr ∈ watched` and `mint > 0`
  ⇒ a recognized `+mint` `kind=system` credit. The recogniser
  (`deposit_mint_credits`, `passbook-stack-optimism/src/op_chain.rs`) is
  pure over `&[(mint, TxKind, from)]` and unit-tested.
- **Fee vaults (BOUNDED, DISCLOSED LIMITATION)** — OP routes
  sequencer/base/L1 fees to predeploy vault addresses, but the pinned
  reth-op API (`op-reth/crates/evm/src/l1.rs`, rev `27bf9194`) applies
  these as **in-EVM state writes during block execution** and exposes
  **only the L1-fee scalars** (`extract_l1_info` /
  `RethL1BlockInfo::l1_tx_data_fee`, `l1.rs:25,295,325`) — there is **no
  per-block "vault credit" accessor** to derive a recognized
  fee-vault `SystemCredit` from. Consequence (disclosed honestly in
  `README.md` + `docs/validation.md`): **if a watched address is itself
  an OP fee-vault predeploy, its per-block vault credit is not
  recognized and would residual-stall.** This is a narrow, explicitly
  bounded case (watching a protocol predeploy is not the common user
  scenario); L1 (withdrawals + beneficiary priority-fee) — the
  proven-broken common case — is FULLY implemented, and OP deposit-mints
  (the common user-visible OP system credit) are implemented.

### Cargo.lock delta for B1

**None.** Zero package/version/source/edge change: the new
`passbook_core::system` module uses only `alloy-primitives`
(already a dep); the L1 withdrawal/fee derivation uses already-present
`alloy-consensus`/`alloy-eips`/`reth-ethereum` facade APIs; the OP
deposit-mint path uses the **inherent** `OpTxEnvelope::as_deposit`
(no op-alloy edge added). `git diff --stat Cargo.lock` is empty.
