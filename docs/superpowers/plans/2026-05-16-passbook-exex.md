# Passbook — Address Transfer ExEx Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Reth Execution Extension that forward-only captures ERC20 transfers, native ETH transfers (including internal calls), and gas fees for a small static set of watched addresses into a verifiable SQLite ledger, shipped as two drop-in-safe Docker images (`reth-passbook` L1, `op-reth-passbook` OP) with a read-only `passbook` JSON-RPC namespace.

**Architecture:** Approach A (all-in-one): decode + gated re-execution + SQLite ledger + RPC all run inside the node process. A node-generic `passbook-core` crate is written against `reth-exex`/`reth-node-api` traits so one ExEx function compiles for both `EthereumNode` and `OpNode`. L1-vs-OP differences (L1 data fee, system-balance events) are isolated behind a `StackAdapter` trait whose OP implementation lives in the OP binary crate so core never depends on OP crates. The ledger is a separate SQLite store (never reth's MDBX). Per-block processing is atomic with retry-until-success — the ExEx never skips, never advances past an unexplained reconciliation residual.

**Tech Stack:** Rust, reth `v2.2.0` (L1) + matching `op-reth` (OP) pinned via one workspace git rev, `revm`/`revm-inspectors` (custom value-only inspector), `rusqlite` (bundled SQLite, WAL), `jsonrpsee` (RPC namespace), `clap` (CLI ext), `reth-exex-test-utils` (integration fixtures), Docker multi-stage.

---

## Dependency & version strategy (answers "latest op-reth, easy to update")

**Decision:** all reth / op-reth / revm crates are declared **once** in the root `[workspace.dependencies]` table, every member uses `.workspace = true`, and all reth/op-reth crates are pinned to **one git `rev`** of a single op-reth source repo. Bumping to a new op-reth release is then: change one `rev` value, run one script, run the build-spike test. No member crate ever names a reth version.

**Why one repo:** since reth 2.0 the OP stack lives outside `paradigmxyz/reth`. The L1 facade (`reth-ethereum`) and OP facade (`reth-op`) only verifiably co-resolve in one Cargo workspace when both are pulled from the **same** op-reth source tree (op-reth is a fork that vendors/tracks upstream reth and re-exports both facades). Pulling L1 from `paradigmxyz/reth` and OP from a different repo is a two-source-repo workspace and is **not** verified to compile. Task 0.1 is a build spike that resolves the exact repo (`ethereum-optimism/op-reth` vs `op-rs/op-reth`), the latest rev, and the exact facade crate names by compiling — not by guessing.

**Pinned baseline (verified against `paradigmxyz/reth` tag `v2.2.0`, commit `88505c7`, latest stable 2026-05-16):** `revm 38.0.0`, `revm-inspectors 0.39.0`, `revm-database 13.0.0`, `alloy-evm 0.34.0`. op-reth tracks these; Task 0.1 records the actual rev and any version drift in `docs/reth-pin.md`.

---

## File structure

```
passbook/
  Cargo.toml                          # workspace + the single dependency pin table
  rust-toolchain.toml                 # pinned toolchain
  Makefile                            # bump-reth, build, test, docker targets
  Dockerfile                          # one multi-stage build -> two images
  README.md
  docs/reth-pin.md                    # records exact op-reth repo+rev, bump procedure, drift notes
  scripts/bump-reth.sh                # one-command rev bump
  crates/
    passbook-core/                    # node-generic; NO op-reth crate deps
      Cargo.toml
      src/
        lib.rs                        # re-exports; module wiring
        config.rs                     # PassbookConfig: parse/validate addresses + db path
        stack.rs                      # StackAdapter trait (gas/L1-fee + system events)
        erc20.rs                      # Transfer log topic match + decode + direction
        inspector.rs                  # custom value-only revm Inspector
        attribution.rs                # frame -> EthTransferRow; gas computation
        reconcile.rs                  # observed delta vs Σ attribution; residual
        model.rs                      # row structs shared by ledger + rpc + attribution
        ledger/
          mod.rs                      # open + pragmas + migrate
          schema.rs                   # schema v1 DDL
          writer.rs                   # atomic per-block write; reorg delete-by-hash
          queries.rs                  # read-only queries (health, getTransfers)
        exex.rs                       # generic ExEx loop: gate, re-exec, reconcile, retry, FinishedHeight
        rpc.rs                        # #[rpc(namespace="passbook")] trait + impl over queries
    passbook-stack-ethereum/          # L1 StackAdapter impl (no L1 data fee)
      Cargo.toml
      src/lib.rs
    passbook-stack-optimism/          # OP StackAdapter impl (L1 data fee via reth-optimism-evm)
      Cargo.toml
      src/lib.rs
    bin/
      reth-passbook/                  # EthereumNode + ExEx + ethereum stack adapter
        Cargo.toml
        src/main.rs
      op-reth-passbook/               # OpNode + ExEx + optimism stack adapter
        Cargo.toml
        src/main.rs
  tests/                              # workspace-level integration tests
```

Responsibilities are split by **what changes together**: pure decode/attribution/reconcile logic (no reth deps, fully unit-testable) is separate from the reth-coupled `exex.rs`/binaries, so an upstream reth bump only churns `exex.rs`, the binaries, and the stack crates — never the audited core logic.

---

## Phase 0 — Workspace, dependency pin, build spike

### Task 0.1: Build spike — lock op-reth repo, rev, and facade crate names

**Files:**
- Create: `Cargo.toml` (root, temporary minimal form)
- Create: `crates/spike/Cargo.toml`, `crates/spike/src/main.rs`
- Create: `docs/reth-pin.md`

- [ ] **Step 1: Write the spike crate that forces both facades to co-resolve**

`crates/spike/src/main.rs`:

```rust
//! Spike: proves L1 + OP facades co-resolve in one workspace and that one
//! generic ExEx fn compiles for both EthereumNode and OpNode.
use reth_ethereum::{
    exex::{ExExContext, ExExEvent},
    node::{api::FullNodeComponents, EthereumNode},
};
use futures::TryStreamExt;

async fn exex<Node: FullNodeComponents>(mut ctx: ExExContext<Node>) -> eyre::Result<()> {
    while let Some(n) = ctx.notifications.try_next().await? {
        if let Some(c) = n.committed_chain() {
            ctx.events.send(ExExEvent::FinishedHeight(c.tip().num_hash()))?;
        }
    }
    Ok(())
}

fn main() -> eyre::Result<()> {
    // Compile-only: reference both node types.
    let _ = EthereumNode::default();
    let _ = reth_op::node::OpNode::default();
    let _ = exex::<reth_ethereum::node::api::NodeAdapterStub> ; // see step 2 note
    Ok(())
}
```

- [ ] **Step 2: Resolve the repo + rev empirically**

Run, in order, until one resolves (record which in `docs/reth-pin.md`):

```bash
# Candidate A (Optimism official):
gh api repos/ethereum-optimism/op-reth/commits/optimism --jq '.sha'
# Candidate B:
gh api repos/op-rs/op-reth/commits/unstable --jq '.sha'
```

Set the root `Cargo.toml` `[workspace.dependencies]` (see Task 0.2 for full form) `git`/`rev` to the resolved repo + the returned SHA. Drop the unused stub line from `main.rs`; the spike's real test is that `cargo build -p spike` links both facades.

- [ ] **Step 3: Run the spike build**

Run: `cargo build -p spike 2>&1 | tail -20`
Expected: PASS (links). If `reth_op::node::OpNode` path differs, fix the path per the compiler error and record the correct path in `docs/reth-pin.md` under "facade paths".

- [ ] **Step 4: Record the lock**

Write `docs/reth-pin.md` with: chosen repo URL, exact `rev` SHA, date, the verified facade crate names/paths (`reth-ethereum` → `reth_ethereum::{exex,node,cli}`, `reth-op` → `reth_op::{node::OpNode,cli}`), revm/alloy-evm versions `cargo tree` reports, and the bump procedure (Task 0.4).

- [ ] **Step 5: Commit**

```bash
git init && git add -A
git commit -m "chore: build spike locks op-reth repo/rev and facade paths"
```

### Task 0.2: Root workspace manifest with the single pin table

**Files:**
- Modify: `Cargo.toml` (root)
- Create: `rust-toolchain.toml`

- [ ] **Step 1: Write the workspace manifest**

`Cargo.toml` (substitute `<REPO>`/`<REV>` from Task 0.1):

```toml
[workspace]
resolver = "2"
members = [
  "crates/passbook-core",
  "crates/passbook-stack-ethereum",
  "crates/passbook-stack-optimism",
  "crates/bin/reth-passbook",
  "crates/bin/op-reth-passbook",
  "crates/spike",
]

[workspace.package]
edition = "2021"
license = "MIT"

# === SINGLE PLACE TO BUMP RETH/OP-RETH ===
[workspace.dependencies]
reth-ethereum = { git = "<REPO>", rev = "<REV>", features = ["full", "cli"] }
reth-op       = { git = "<REPO>", rev = "<REV>", features = ["full", "cli"] }
reth-exex-test-utils = { git = "<REPO>", rev = "<REV>" }
reth-optimism-evm    = { git = "<REPO>", rev = "<REV>" }
revm = "38.0.0"
revm-inspectors = "0.39.0"
alloy-primitives = "1"
alloy-consensus = "1"
alloy-eips = "1"
rusqlite = { version = "0.32", features = ["bundled"] }
jsonrpsee = { version = "0.24", features = ["server", "macros"] }
clap = { version = "4", features = ["derive", "env"] }
eyre = "0.6"
futures = "0.3"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }

[profile.release]
lto = "thin"
codegen-units = 1
```

> Exact `jsonrpsee`/`alloy-*`/`rusqlite` versions: pin to whatever `cargo tree` shows reth re-exports after Task 0.1; record in `docs/reth-pin.md`. Mismatched `alloy`/`revm` versions cause trait-coherence errors — always match reth's.

- [ ] **Step 2: Pin the toolchain**

`rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.85"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Verify workspace resolves**

Run: `cargo metadata --format-version 1 >/dev/null && echo OK`
Expected: `OK`

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml rust-toolchain.toml
git commit -m "chore: workspace manifest with single reth pin table"
```

### Task 0.3: Member crate skeletons

**Files:**
- Create: `crates/passbook-core/Cargo.toml`, `crates/passbook-core/src/lib.rs`
- Create: `crates/passbook-stack-ethereum/Cargo.toml`, `.../src/lib.rs`
- Create: `crates/passbook-stack-optimism/Cargo.toml`, `.../src/lib.rs`
- Create: `crates/bin/reth-passbook/Cargo.toml`, `.../src/main.rs`
- Create: `crates/bin/op-reth-passbook/Cargo.toml`, `.../src/main.rs`

- [ ] **Step 1: Write `passbook-core/Cargo.toml`** (no op-reth crate deps)

```toml
[package]
name = "passbook-core"
edition.workspace = true
license.workspace = true
version = "0.1.0"

[dependencies]
alloy-primitives.workspace = true
alloy-consensus.workspace = true
alloy-eips.workspace = true
revm.workspace = true
rusqlite.workspace = true
jsonrpsee.workspace = true
eyre.workspace = true
futures.workspace = true
tokio.workspace = true
tracing.workspace = true
thiserror.workspace = true
serde.workspace = true

[dev-dependencies]
reth-exex-test-utils.workspace = true
tempfile = "3"
```

- [ ] **Step 2: Write the four other crate manifests**

`passbook-stack-ethereum/Cargo.toml`: deps = `passbook-core` (path), `alloy-primitives`, `alloy-consensus`.
`passbook-stack-optimism/Cargo.toml`: deps = `passbook-core` (path), `reth-optimism-evm.workspace = true`, `alloy-primitives`, `alloy-consensus`.
`crates/bin/reth-passbook/Cargo.toml`: `[[bin]] name="reth-passbook"`, deps = `passbook-core`, `passbook-stack-ethereum`, `reth-ethereum.workspace = true`, `clap`, `eyre`, `tokio`, `futures`.
`crates/bin/op-reth-passbook/Cargo.toml`: `[[bin]] name="op-reth-passbook"`, deps = `passbook-core`, `passbook-stack-optimism`, `reth-op.workspace = true`, `clap`, `eyre`, `tokio`, `futures`.

- [ ] **Step 3: Stub each `lib.rs`/`main.rs`**

`passbook-core/src/lib.rs`:

```rust
pub mod config;
pub mod stack;
pub mod model;
pub mod erc20;
pub mod inspector;
pub mod attribution;
pub mod reconcile;
pub mod ledger;
pub mod exex;
pub mod rpc;
```

Create empty (`// implemented in later tasks`) module files for each so the crate compiles. Stack crate `lib.rs`: `// impl in Phase 8`. Binary `main.rs`: `fn main() {}`.

- [ ] **Step 4: Verify**

Run: `cargo build --workspace 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/
git commit -m "chore: member crate skeletons"
```

### Task 0.4: One-command bump script + Makefile

**Files:**
- Create: `scripts/bump-reth.sh`
- Create: `Makefile`

- [ ] **Step 1: Write the bump script**

`scripts/bump-reth.sh`:

```bash
#!/usr/bin/env bash
# Usage: scripts/bump-reth.sh <new-rev-sha>
# Bumps every git-pinned reth/op-reth dependency to <new-rev-sha> in one place.
set -euo pipefail
NEW="${1:?usage: bump-reth.sh <rev-sha>}"
sed -i.bak -E "s/(rev = \")[0-9a-f]{7,40}(\")/\1${NEW}\2/g" Cargo.toml
rm -f Cargo.toml.bak
cargo update -p reth-ethereum -p reth-op 2>/dev/null || cargo update
echo "Bumped to ${NEW}. Now run: make verify-pin"
```

`chmod +x scripts/bump-reth.sh`.

- [ ] **Step 2: Write the Makefile**

```make
.PHONY: build test verify-pin docker bump
build:        ; cargo build --workspace --release
test:         ; cargo test --workspace
verify-pin:   ; cargo build -p spike && cargo test -p passbook-core --test exex_integration
bump:         ; ./scripts/bump-reth.sh $(REV)
docker:       ; docker build -t reth-passbook:dev --target reth-passbook . && \
                docker build -t op-reth-passbook:dev --target op-reth-passbook .
```

- [ ] **Step 3: Smoke-test the script is idempotent**

Run: `./scripts/bump-reth.sh $(grep -oE 'rev = "[0-9a-f]+"' Cargo.toml | head -1 | grep -oE '[0-9a-f]+') && git diff --quiet Cargo.toml && echo NOOP-OK`
Expected: `NOOP-OK` (re-bumping to the same rev changes nothing)

- [ ] **Step 4: Commit**

```bash
git add scripts/bump-reth.sh Makefile
git commit -m "chore: one-command reth bump (scripts/bump-reth.sh + make bump)"
```

---

## Phase 1 — Shared model + SQLite ledger (no reth dependency)

### Task 1.1: Row model structs

**Files:**
- Create: `crates/passbook-core/src/model.rs`
- Test: inline `#[cfg(test)]` in `model.rs`

- [ ] **Step 1: Write a failing test for `Direction` parsing**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direction_roundtrips_as_str() {
        assert_eq!(Direction::In.as_str(), "in");
        assert_eq!(Direction::Out.as_str(), "out");
        assert_eq!(Direction::from_str("in").unwrap(), Direction::In);
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core model::tests::direction_roundtrips_as_str 2>&1 | tail -3`
Expected: FAIL — `Direction` not found.

- [ ] **Step 3: Implement the model**

```rust
use alloy_primitives::{Address, B256, U256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction { In, Out }
impl Direction {
    pub fn as_str(&self) -> &'static str { match self { Self::In => "in", Self::Out => "out" } }
    pub fn from_str(s: &str) -> eyre::Result<Self> {
        match s { "in" => Ok(Self::In), "out" => Ok(Self::Out),
            _ => Err(eyre::eyre!("bad direction {s}")) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EthKind { TopLevel, Internal, System }
impl EthKind {
    pub fn as_str(&self) -> &'static str {
        match self { Self::TopLevel => "top_level", Self::Internal => "internal", Self::System => "system" }
    }
}

#[derive(Debug, Clone)]
pub struct EthTransferRow {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub tx_hash: Option<B256>, pub trace_path: String,
    pub address: Address, pub direction: Direction, pub counterparty: Address,
    pub amount_wei: U256, pub kind: EthKind, pub reverted: bool,
}

#[derive(Debug, Clone)]
pub struct Erc20TransferRow {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub tx_hash: B256, pub log_index: u64, pub token: Address,
    pub from: Address, pub to: Address, pub amount: U256,
    pub address: Address, pub direction: Direction,
}

#[derive(Debug, Clone)]
pub struct GasPaymentRow {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub tx_hash: B256, pub address: Address,
    pub gas_used: u64, pub effective_gas_price: u128,
    pub l2_fee_wei: U256, pub l1_fee_wei: Option<U256>, pub total_wei: U256,
}

#[derive(Debug, Clone)]
pub struct UnattributedDeltaRow {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub address: Address, pub observed_wei: U256,
    pub attributed_wei: U256, pub residual_wei: U256,
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core model:: 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/model.rs
git commit -m "feat(core): ledger row model"
```

### Task 1.2: Schema DDL (schema v1)

**Files:**
- Create: `crates/passbook-core/src/ledger/schema.rs`
- Modify: `crates/passbook-core/src/ledger/mod.rs`

- [ ] **Step 1: Failing test — schema applies cleanly to an in-memory DB**

In `ledger/schema.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn schema_applies() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(super::SCHEMA_V1).unwrap();
        let n: i64 = c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 5); // meta + 4 data tables
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core ledger::schema 2>&1 | tail -3`
Expected: FAIL — `SCHEMA_V1` not found.

- [ ] **Step 3: Implement the DDL**

```rust
pub const SCHEMA_V1: &str = r#"
CREATE TABLE meta (
  k TEXT PRIMARY KEY, v TEXT NOT NULL
);
CREATE TABLE eth_transfers (
  chain_id INTEGER NOT NULL, block_number INTEGER NOT NULL, block_hash TEXT NOT NULL,
  tx_hash TEXT, trace_path TEXT NOT NULL, address TEXT NOT NULL,
  direction TEXT NOT NULL, counterparty TEXT NOT NULL, amount_wei TEXT NOT NULL,
  kind TEXT NOT NULL, reverted INTEGER NOT NULL,
  PRIMARY KEY (chain_id, block_hash, tx_hash, trace_path)
);
CREATE TABLE erc20_transfers (
  chain_id INTEGER NOT NULL, block_number INTEGER NOT NULL, block_hash TEXT NOT NULL,
  tx_hash TEXT NOT NULL, log_index INTEGER NOT NULL, token TEXT NOT NULL,
  from_addr TEXT NOT NULL, to_addr TEXT NOT NULL, amount TEXT NOT NULL,
  address TEXT NOT NULL, direction TEXT NOT NULL,
  PRIMARY KEY (chain_id, block_hash, tx_hash, log_index)
);
CREATE TABLE gas_payments (
  chain_id INTEGER NOT NULL, block_number INTEGER NOT NULL, block_hash TEXT NOT NULL,
  tx_hash TEXT NOT NULL, address TEXT NOT NULL, gas_used INTEGER NOT NULL,
  effective_gas_price TEXT NOT NULL, l2_fee_wei TEXT NOT NULL,
  l1_fee_wei TEXT, total_wei TEXT NOT NULL,
  PRIMARY KEY (chain_id, block_hash, tx_hash, address)
);
CREATE TABLE unattributed_deltas (
  chain_id INTEGER NOT NULL, block_number INTEGER NOT NULL, block_hash TEXT NOT NULL,
  address TEXT NOT NULL, observed_wei TEXT NOT NULL,
  attributed_wei TEXT NOT NULL, residual_wei TEXT NOT NULL,
  PRIMARY KEY (chain_id, block_hash, address)
);
CREATE INDEX ix_eth_addr   ON eth_transfers   (chain_id, address, block_number);
CREATE INDEX ix_erc20_addr ON erc20_transfers (chain_id, address, block_number);
CREATE INDEX ix_gas_addr   ON gas_payments    (chain_id, address, block_number);
CREATE INDEX ix_eth_bh     ON eth_transfers   (block_hash);
CREATE INDEX ix_erc20_bh   ON erc20_transfers (block_hash);
CREATE INDEX ix_gas_bh     ON gas_payments    (block_hash);
CREATE INDEX ix_unattr_bh  ON unattributed_deltas (block_hash);
"#;
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core ledger::schema 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/ledger/
git commit -m "feat(ledger): schema v1 DDL"
```

### Task 1.3: Ledger open with durability pragmas + migration

**Files:**
- Modify: `crates/passbook-core/src/ledger/mod.rs`

- [ ] **Step 1: Failing test — open sets WAL + records schema version**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn open_sets_wal_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.db");
        let l = Ledger::open(&path, 1).unwrap();
        let jm: String = l.conn().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(jm.to_lowercase(), "wal");
        let v: String = l.conn().query_row(
            "SELECT v FROM meta WHERE k='schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "1");
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core ledger::tests::open_sets_wal 2>&1 | tail -3`
Expected: FAIL — `Ledger` not found.

- [ ] **Step 3: Implement `Ledger::open`**

```rust
pub mod schema;
pub mod writer;
pub mod queries;

use rusqlite::Connection;
use std::path::Path;

pub struct Ledger { conn: Connection }

impl Ledger {
    /// Open (creating if absent) and apply durability pragmas + schema.
    /// pragmas: WAL (concurrent reads), synchronous=FULL (never lose a
    /// committed row on power loss — write rate is trivial so fsync cost is
    /// irrelevant), busy_timeout 30s (retry-until-success writer never sees
    /// spurious SQLITE_BUSY), foreign_keys ON (per-connection default is off).
    pub fn open(path: &Path, chain_id: u64) -> eyre::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
        let exists: bool = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='meta'",
            [], |r| r.get::<_, i64>(0))? > 0;
        if !exists {
            conn.execute_batch(schema::SCHEMA_V1)?;
            conn.execute("INSERT INTO meta(k,v) VALUES('schema_version','1')", [])?;
            conn.execute(
                "INSERT INTO meta(k,v) VALUES('chain_id',?1)",
                [chain_id.to_string()])?;
        } else {
            let v: String = conn.query_row(
                "SELECT v FROM meta WHERE k='schema_version'", [], |r| r.get(0))?;
            if v != "1" { eyre::bail!("unsupported schema version {v}"); }
        }
        Ok(Self { conn })
    }
    pub fn conn(&self) -> &Connection { &self.conn }
    pub fn conn_mut(&mut self) -> &mut Connection { &mut self.conn }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core ledger::tests::open_sets_wal 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/ledger/mod.rs
git commit -m "feat(ledger): durable open (WAL, synchronous=FULL) + schema guard"
```

### Task 1.4: Atomic per-block write

**Files:**
- Create: `crates/passbook-core/src/ledger/writer.rs`

- [ ] **Step 1: Failing test — write a block's rows atomically, idempotently**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use alloy_primitives::{Address, B256, U256};

    fn ledger() -> crate::ledger::Ledger {
        let p = std::env::temp_dir().join(format!("pb-{}.db", rand_suffix()));
        crate::ledger::Ledger::open(&p, 1).unwrap()
    }
    fn rand_suffix() -> u64 { std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 }

    #[test]
    fn write_block_is_idempotent() {
        let mut l = ledger();
        let bh = B256::repeat_byte(7);
        let batch = BlockBatch {
            chain_id: 1, block_number: 100, block_hash: bh,
            eth: vec![EthTransferRow {
                chain_id:1, block_number:100, block_hash:bh,
                tx_hash: Some(B256::repeat_byte(1)), trace_path:"0".into(),
                address: Address::repeat_byte(0xaa), direction: Direction::In,
                counterparty: Address::repeat_byte(0xbb), amount_wei: U256::from(5),
                kind: EthKind::TopLevel, reverted:false }],
            erc20: vec![], gas: vec![], unattributed: vec![],
        };
        write_block(l.conn_mut(), &batch).unwrap();
        write_block(l.conn_mut(), &batch).unwrap(); // replay -> no dup
        let n: i64 = l.conn().query_row(
            "SELECT count(*) FROM eth_transfers", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let last: String = l.conn().query_row(
            "SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0)).unwrap();
        assert_eq!(last, "100");
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core ledger::writer 2>&1 | tail -3`
Expected: FAIL — `BlockBatch`/`write_block` not found.

- [ ] **Step 3: Implement the writer**

```rust
use crate::model::*;
use rusqlite::Connection;

pub struct BlockBatch {
    pub chain_id: u64, pub block_number: u64,
    pub block_hash: alloy_primitives::B256,
    pub eth: Vec<EthTransferRow>,
    pub erc20: Vec<Erc20TransferRow>,
    pub gas: Vec<GasPaymentRow>,
    pub unattributed: Vec<UnattributedDeltaRow>,
}

fn h(b: &alloy_primitives::B256) -> String { format!("{b:#x}") }
fn a(x: &alloy_primitives::Address) -> String { format!("{x:#x}") }
fn u(x: &alloy_primitives::U256) -> String { x.to_string() }

/// One DB transaction per block. INSERT OR REPLACE on the natural PKs makes
/// replay (after a crash between commit and FinishedHeight) a no-op.
pub fn write_block(conn: &mut Connection, b: &BlockBatch) -> eyre::Result<()> {
    let tx = conn.transaction()?;
    for r in &b.eth {
        tx.execute(
            "INSERT OR REPLACE INTO eth_transfers VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                r.chain_id, r.block_number, h(&r.block_hash),
                r.tx_hash.as_ref().map(h), r.trace_path, a(&r.address),
                r.direction.as_str(), a(&r.counterparty), u(&r.amount_wei),
                r.kind.as_str(), r.reverted as i64])?;
    }
    for r in &b.erc20 {
        tx.execute(
            "INSERT OR REPLACE INTO erc20_transfers VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![
                r.chain_id, r.block_number, h(&r.block_hash), h(&r.tx_hash),
                r.log_index, a(&r.token), a(&r.from), a(&r.to), u(&r.amount),
                a(&r.address), r.direction.as_str()])?;
    }
    for r in &b.gas {
        tx.execute(
            "INSERT OR REPLACE INTO gas_payments VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                r.chain_id, r.block_number, h(&r.block_hash), h(&r.tx_hash),
                a(&r.address), r.gas_used, r.effective_gas_price.to_string(),
                u(&r.l2_fee_wei), r.l1_fee_wei.as_ref().map(u), u(&r.total_wei)])?;
    }
    for r in &b.unattributed {
        tx.execute(
            "INSERT OR REPLACE INTO unattributed_deltas VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                r.chain_id, r.block_number, h(&r.block_hash), a(&r.address),
                u(&r.observed_wei), u(&r.attributed_wei), u(&r.residual_wei)])?;
    }
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('last_block',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [b.block_number.to_string()])?;
    tx.commit()?;
    Ok(())
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core ledger::writer 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/ledger/writer.rs
git commit -m "feat(ledger): atomic idempotent per-block write"
```

### Task 1.5: Reorg delete-by-block-hash

**Files:**
- Modify: `crates/passbook-core/src/ledger/writer.rs`

- [ ] **Step 1: Failing test**

Append to `writer.rs` tests:

```rust
#[test]
fn delete_by_block_hash_removes_all_categories() {
    let mut l = ledger();
    let bh = B256::repeat_byte(9);
    let batch = BlockBatch { chain_id:1, block_number:5, block_hash:bh,
        eth: vec![EthTransferRow { chain_id:1, block_number:5, block_hash:bh,
            tx_hash:Some(B256::repeat_byte(2)), trace_path:"0".into(),
            address:Address::repeat_byte(1), direction:Direction::Out,
            counterparty:Address::repeat_byte(2), amount_wei:U256::from(1),
            kind:EthKind::TopLevel, reverted:false }],
        erc20:vec![], gas:vec![], unattributed:vec![] };
    write_block(l.conn_mut(), &batch).unwrap();
    delete_blocks(l.conn_mut(), 1, &[bh]).unwrap();
    let n: i64 = l.conn().query_row(
        "SELECT count(*) FROM eth_transfers", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core ledger::writer::tests::delete_by_block_hash 2>&1 | tail -3`
Expected: FAIL — `delete_blocks` not found.

- [ ] **Step 3: Implement**

```rust
/// Reorg handling: drop every row for the reverted block hashes.
pub fn delete_blocks(
    conn: &mut Connection, chain_id: u64, hashes: &[alloy_primitives::B256],
) -> eyre::Result<()> {
    let tx = conn.transaction()?;
    for bh in hashes {
        let hs = h(bh);
        for table in ["eth_transfers","erc20_transfers","gas_payments","unattributed_deltas"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE chain_id=?1 AND block_hash=?2"),
                rusqlite::params![chain_id, hs])?;
        }
    }
    tx.commit()?;
    Ok(())
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core ledger::writer 2>&1 | tail -3`
Expected: PASS (both writer tests)

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/ledger/writer.rs
git commit -m "feat(ledger): reorg delete-by-block-hash"
```

### Task 1.6: Read-only queries (health + getTransfers)

**Files:**
- Create: `crates/passbook-core/src/ledger/queries.rs`

- [ ] **Step 1: Failing test — health + cursor pagination**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // reuse the writer test's ledger/batch helpers via crate::ledger::writer::tests is not
    // public; build a fresh ledger inline:
    #[test]
    fn health_reports_last_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = crate::ledger::Ledger::open(&dir.path().join("q.db"), 1).unwrap();
        l.conn().execute(
            "INSERT INTO meta(k,v) VALUES('last_block','42')
             ON CONFLICT(k) DO UPDATE SET v=excluded.v", []).unwrap();
        assert_eq!(health(l.conn()).unwrap().last_block, Some(42));
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core ledger::queries 2>&1 | tail -3`
Expected: FAIL — `health` not found.

- [ ] **Step 3: Implement queries**

```rust
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Health { pub last_block: Option<u64>, pub chain_id: Option<u64> }

pub fn health(conn: &Connection) -> eyre::Result<Health> {
    let last_block = conn.query_row(
        "SELECT v FROM meta WHERE k='last_block'", [], |r| r.get::<_,String>(0))
        .ok().and_then(|s| s.parse().ok());
    let chain_id = conn.query_row(
        "SELECT v FROM meta WHERE k='chain_id'", [], |r| r.get::<_,String>(0))
        .ok().and_then(|s| s.parse().ok());
    Ok(Health { last_block, chain_id })
}

#[derive(Debug, Serialize)]
pub struct TransferRowOut {
    pub category: &'static str, pub block_number: u64, pub block_hash: String,
    pub tx_hash: Option<String>, pub address: String, pub direction: Option<String>,
    pub counterparty: Option<String>, pub token: Option<String>,
    pub amount: String, pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TransfersPage { pub rows: Vec<TransferRowOut>, pub next_cursor: Option<u64> }

/// Unified, cursor-paginated read over eth + erc20 + gas + unattributed.
/// Cursor = block_number; pages by ascending block. `kind` filters the
/// `category`/`kind` column. Callers derive totals/exports themselves.
pub fn get_transfers(
    conn: &Connection, chain_id: u64, address: &str,
    from_block: Option<u64>, to_block: Option<u64>,
    kind: Option<&str>, cursor: Option<u64>, limit: u32,
) -> eyre::Result<TransfersPage> {
    let lo = cursor.or(from_block).unwrap_or(0);
    let hi = to_block.unwrap_or(u64::MAX);
    let lim = limit.min(1000) as i64;
    let mut rows: Vec<(u64, TransferRowOut)> = Vec::new();

    // eth_transfers
    let mut s = conn.prepare(
        "SELECT block_number,block_hash,tx_hash,address,direction,counterparty,amount_wei,kind
         FROM eth_transfers WHERE chain_id=?1 AND address=?2
           AND block_number>=?3 AND block_number<=?4
           AND (?5 IS NULL OR kind=?5)
         ORDER BY block_number LIMIT ?6")?;
    let it = s.query_map(
        rusqlite::params![chain_id, address, lo, hi, kind, lim],
        |r| Ok((r.get::<_,i64>(0)? as u64, TransferRowOut{
            category:"eth", block_number:r.get::<_,i64>(0)? as u64,
            block_hash:r.get(1)?, tx_hash:r.get(2)?, address:r.get(3)?,
            direction:r.get(4)?, counterparty:r.get(5)?, token:None,
            amount:r.get(6)?, kind:r.get(7)? })))?;
    for x in it { rows.push(x?); }

    // erc20_transfers (category fixed "erc20"; honour kind filter only if kind in (None,"erc20"))
    if kind.is_none() || kind == Some("erc20") {
        let mut s = conn.prepare(
            "SELECT block_number,block_hash,tx_hash,address,direction,token,amount,
                    from_addr,to_addr
             FROM erc20_transfers WHERE chain_id=?1 AND address=?2
               AND block_number>=?3 AND block_number<=?4
             ORDER BY block_number LIMIT ?5")?;
        let it = s.query_map(
            rusqlite::params![chain_id, address, lo, hi, lim],
            |r| Ok((r.get::<_,i64>(0)? as u64, TransferRowOut{
                category:"erc20", block_number:r.get::<_,i64>(0)? as u64,
                block_hash:r.get(1)?, tx_hash:r.get(2)?, address:r.get(3)?,
                direction:r.get(4)?, counterparty:None, token:r.get(5)?,
                amount:r.get(6)?, kind:Some("erc20".into()) })))?;
        for x in it { rows.push(x?); }
    }

    // gas_payments (kind "gas") and unattributed (kind "unattributed") follow the
    // same shape; include when kind is None or matches.
    if kind.is_none() || kind == Some("gas") {
        let mut s = conn.prepare(
            "SELECT block_number,block_hash,tx_hash,address,total_wei
             FROM gas_payments WHERE chain_id=?1 AND address=?2
               AND block_number>=?3 AND block_number<=?4
             ORDER BY block_number LIMIT ?5")?;
        let it = s.query_map(
            rusqlite::params![chain_id, address, lo, hi, lim],
            |r| Ok((r.get::<_,i64>(0)? as u64, TransferRowOut{
                category:"gas", block_number:r.get::<_,i64>(0)? as u64,
                block_hash:r.get(1)?, tx_hash:r.get(2)?, address:r.get(3)?,
                direction:Some("out".into()), counterparty:None, token:None,
                amount:r.get(4)?, kind:Some("gas".into()) })))?;
        for x in it { rows.push(x?); }
    }
    if kind.is_none() || kind == Some("unattributed") {
        let mut s = conn.prepare(
            "SELECT block_number,block_hash,address,residual_wei
             FROM unattributed_deltas WHERE chain_id=?1 AND address=?2
               AND block_number>=?3 AND block_number<=?4
             ORDER BY block_number LIMIT ?5")?;
        let it = s.query_map(
            rusqlite::params![chain_id, address, lo, hi, lim],
            |r| Ok((r.get::<_,i64>(0)? as u64, TransferRowOut{
                category:"unattributed", block_number:r.get::<_,i64>(0)? as u64,
                block_hash:r.get(1)?, tx_hash:None, address:r.get(2)?,
                direction:None, counterparty:None, token:None,
                amount:r.get(3)?, kind:Some("unattributed".into()) })))?;
        for x in it { rows.push(x?); }
    }

    rows.sort_by_key(|(b, _)| *b);
    let next_cursor = if rows.len() as i64 >= lim {
        rows.last().map(|(b, _)| b + 1)
    } else { None };
    Ok(TransfersPage { rows: rows.into_iter().map(|(_, r)| r).collect(), next_cursor })
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core ledger::queries 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/ledger/queries.rs
git commit -m "feat(ledger): read-only health + paginated getTransfers"
```

---

## Phase 2 — ERC20 decode + filter (pure)

### Task 2.1: Transfer topic0 constant + log matcher

**Files:**
- Create: `crates/passbook-core/src/erc20.rs`

- [ ] **Step 1: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bytes, U256};

    fn topic_addr(a: Address) -> B256 {
        let mut b = [0u8; 32]; b[12..].copy_from_slice(a.as_slice()); B256::from(b)
    }

    #[test]
    fn decodes_inbound_transfer_for_watched_to() {
        let watched = Address::repeat_byte(0xcc);
        let from = Address::repeat_byte(0x11);
        let token = Address::repeat_byte(0x99);
        let log = RawLog {
            address: token,
            topics: vec![TRANSFER_TOPIC0, topic_addr(from), topic_addr(watched)],
            data: Bytes::from(U256::from(1234).to_be_bytes::<32>().to_vec()),
        };
        let watch = [watched].into_iter().collect();
        let out = decode_transfer(&log, &watch).unwrap();
        assert_eq!(out.from, from);
        assert_eq!(out.to, watched);
        assert_eq!(out.amount, U256::from(1234));
        assert_eq!(out.matched, vec![(watched, crate::model::Direction::In)]);
    }

    #[test]
    fn ignores_non_transfer_and_unwatched() {
        let watch = [Address::repeat_byte(0xcc)].into_iter().collect();
        let other = RawLog { address: Address::ZERO,
            topics: vec![B256::repeat_byte(1)], data: Default::default() };
        assert!(decode_transfer(&other, &watch).is_none());
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core erc20 2>&1 | tail -3`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement**

```rust
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use std::collections::HashSet;
use crate::model::Direction;

/// keccak256("Transfer(address,address,uint256)")
pub static TRANSFER_TOPIC0: B256 = B256::new([
    0xdd,0xf2,0x52,0xad,0x1b,0xe2,0xc8,0x9b,0x69,0xc2,0xb0,0x68,0xfc,0x37,0x8d,0xaa,
    0x95,0x2b,0xa7,0xf1,0x63,0xc4,0xa1,0x16,0x28,0xf5,0x5a,0x4d,0xf5,0x23,0xb3,0xef]);

/// Minimal node-generic log shape (decouples core from reth log types;
/// `exex.rs` maps reth logs into this).
#[derive(Debug, Clone)]
pub struct RawLog { pub address: Address, pub topics: Vec<B256>, pub data: Bytes }

#[derive(Debug, Clone)]
pub struct DecodedTransfer {
    pub token: Address, pub from: Address, pub to: Address, pub amount: U256,
    /// Which watched addresses matched and the direction for each.
    pub matched: Vec<(Address, Direction)>,
}

fn topic_to_address(t: &B256) -> Address { Address::from_slice(&t.as_slice()[12..]) }

/// Returns Some when topic0 is Transfer and from|to ∈ watched.
pub fn decode_transfer(log: &RawLog, watched: &HashSet<Address>) -> Option<DecodedTransfer> {
    if log.topics.len() != 3 || log.topics[0] != TRANSFER_TOPIC0 { return None; }
    let from = topic_to_address(&log.topics[1]);
    let to = topic_to_address(&log.topics[2]);
    if log.data.len() < 32 { return None; }
    let amount = U256::from_be_slice(&log.data[..32]);
    let mut matched = Vec::new();
    if watched.contains(&to)   { matched.push((to,   Direction::In));  }
    if watched.contains(&from) { matched.push((from, Direction::Out)); }
    if matched.is_empty() { return None; }
    Some(DecodedTransfer { token: log.address, from, to, amount, matched })
}

#[allow(dead_code)]
fn _compile_time_topic_check() {
    debug_assert_eq!(TRANSFER_TOPIC0,
        keccak256("Transfer(address,address,uint256)".as_bytes()));
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core erc20 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Add a test asserting the hardcoded topic equals keccak**

```rust
#[test]
fn topic0_matches_keccak() {
    assert_eq!(TRANSFER_TOPIC0,
        alloy_primitives::keccak256("Transfer(address,address,uint256)"));
}
```

Run: `cargo test -p passbook-core erc20::tests::topic0_matches_keccak 2>&1 | tail -3`
Expected: PASS (fix the constant bytes from the failure message if it fails)

- [ ] **Step 6: Commit**

```bash
git add crates/passbook-core/src/erc20.rs
git commit -m "feat(core): ERC20 Transfer decode + watched filter"
```

---

## Phase 3 — Custom value-only inspector

> Rationale (from research): `revm-inspectors::TracingInspector`'s output schema (`CallTrace`/`CallKind`) is the highest-churn API across revm-inspectors 0.x. A ~40-line custom `revm::Inspector` capturing only value-bearing `CALL`/`CALLCODE`/`CREATE`/`CREATE2`/`SELFDESTRUCT` depends only on the stable `revm::interpreter` input types. Use the custom inspector.

### Task 3.1: ValueInspector capturing native-value frames

**Files:**
- Create: `crates/passbook-core/src/inspector.rs`

- [ ] **Step 1: Failing test — records a transferring CALL, skips delegate/static**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};

    #[test]
    fn records_value_call_and_assigns_trace_path() {
        let mut insp = ValueInspector::default();
        insp.push_frame(FrameMove {
            from: Address::repeat_byte(1), to: Address::repeat_byte(2),
            value: U256::from(10), kind: FrameKind::Call });
        insp.push_frame(FrameMove {
            from: Address::repeat_byte(1), to: Address::repeat_byte(3),
            value: U256::ZERO, kind: FrameKind::Call }); // zero -> dropped
        let frames = insp.into_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].trace_path, "0");
        assert_eq!(frames[0].value, U256::from(10));
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core inspector 2>&1 | tail -3`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement the inspector core (revm-trait wiring isolated)**

```rust
use alloy_primitives::{Address, U256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind { Call, CallCode, Create, Create2, SelfDestruct }

#[derive(Debug, Clone)]
pub struct FrameMove {
    pub from: Address, pub to: Address, pub value: U256, pub kind: FrameKind,
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub from: Address, pub to: Address, pub value: U256,
    pub kind: FrameKind, pub trace_path: String,
}

/// Pure capture buffer. `push_frame` is called by the revm Inspector glue
/// (Step 5) for every value-bearing sub-call; DELEGATECALL/STATICCALL never
/// reach here because they carry no transferable value.
#[derive(Default)]
pub struct ValueInspector { seq: u64, frames: Vec<CapturedFrame> }

impl ValueInspector {
    pub fn push_frame(&mut self, m: FrameMove) {
        if m.value.is_zero() { return; }
        let trace_path = self.seq.to_string();
        self.seq += 1;
        self.frames.push(CapturedFrame {
            from: m.from, to: m.to, value: m.value, kind: m.kind, trace_path });
    }
    pub fn into_frames(self) -> Vec<CapturedFrame> { self.frames }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core inspector 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Add the revm `Inspector` impl (verify trait shape against pinned revm 38)**

Append to `inspector.rs`. The exact `revm::Inspector` associated types/method
signatures must be confirmed against the rev locked in Task 0.1 — the research
flagged this as version-sensitive. Implement only `call`, `create_end`,
`selfdestruct`, forwarding into `push_frame`:

```rust
use revm::interpreter::{CallInputs, CallOutcome, CallValue, CreateInputs, CreateOutcome};
use revm::Inspector;

impl<CTX> Inspector<CTX> for ValueInspector {
    fn call(&mut self, _ctx: &mut CTX, i: &mut CallInputs) -> Option<CallOutcome> {
        if let CallValue::Transfer(v) = i.value {
            if !v.is_zero() {
                let kind = match i.scheme {
                    revm::interpreter::CallScheme::CallCode => FrameKind::CallCode,
                    _ => FrameKind::Call, // Call; Delegate/Static never carry Transfer
                };
                self.push_frame(FrameMove {
                    from: i.caller, to: i.target_address, value: v, kind });
            }
        }
        None
    }
    fn create_end(&mut self, _ctx: &mut CTX, i: &CreateInputs, o: &mut CreateOutcome) {
        if !i.value.is_zero() {
            if let Some(addr) = o.address {
                let kind = match i.scheme {
                    revm::interpreter::CreateScheme::Create2 { .. } => FrameKind::Create2,
                    _ => FrameKind::Create,
                };
                self.push_frame(FrameMove {
                    from: i.caller, to: addr, value: i.value, kind });
            }
        }
    }
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        if !value.is_zero() {
            self.push_frame(FrameMove {
                from: contract, to: target, value, kind: FrameKind::SelfDestruct });
        }
    }
}
```

- [ ] **Step 6: Verify it compiles against the pinned revm**

Run: `cargo build -p passbook-core 2>&1 | tail -10`
Expected: PASS. If method signatures differ, adjust to the compiler's reported
`revm::Inspector` trait shape and record the delta in `docs/reth-pin.md`. The
pure `push_frame`/test from Steps 1–4 is unaffected by any such adjustment.

- [ ] **Step 7: Commit**

```bash
git add crates/passbook-core/src/inspector.rs
git commit -m "feat(core): custom value-only revm inspector"
```

---

## Phase 4 — Attribution + StackAdapter

### Task 4.1: StackAdapter trait

**Files:**
- Create: `crates/passbook-core/src/stack.rs`

- [ ] **Step 1: Failing test — a no-L1-fee stub adapter**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    struct NoL1;
    impl StackAdapter for NoL1 {
        fn l1_data_fee_wei(&self, _tx_index: usize) -> Option<U256> { None }
    }
    #[test]
    fn default_adapter_has_no_l1_fee() {
        assert_eq!(NoL1.l1_data_fee_wei(0), None);
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core stack 2>&1 | tail -3`
Expected: FAIL — `StackAdapter` not found.

- [ ] **Step 3: Implement the trait**

```rust
use alloy_primitives::{Address, U256};

/// Isolates L1-vs-OP differences. Implemented per binary:
/// - ethereum: always returns None for the L1 data fee.
/// - optimism: computes per-tx L1 data fee via reth-optimism-evm.
/// `system_credits` surfaces recognised non-call balance changes
/// (L1 withdrawals/beacon deposits/block rewards, OP deposit mints / fee
/// vaults) so reconciliation attributes them as kind=system.
pub trait StackAdapter: Send + Sync + 'static {
    /// Per-transaction OP L1 data fee, or None on L1.
    fn l1_data_fee_wei(&self, tx_index: usize) -> Option<U256>;

    /// Recognised system balance credits/debits for this block,
    /// as (address, signed_wei) where positive = credit to address.
    fn system_credits(&self) -> Vec<(Address, i128)> { Vec::new() }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core stack 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/stack.rs
git commit -m "feat(core): StackAdapter trait"
```

### Task 4.2: Gas computation

**Files:**
- Create: `crates/passbook-core/src/attribution.rs`

- [ ] **Step 1: Failing test — L2 fee + optional L1 fee**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};

    #[test]
    fn gas_payment_includes_l1_when_present() {
        let g = compute_gas_payment(GasInput {
            chain_id:1, block_number:7, block_hash:B256::ZERO,
            tx_hash:B256::repeat_byte(1), tx_from:Address::repeat_byte(0xaa),
            gas_used:21000, effective_gas_price:1_000_000_000u128,
            l1_fee_wei: Some(U256::from(500)) });
        assert_eq!(g.l2_fee_wei, U256::from(21000u64) * U256::from(1_000_000_000u64));
        assert_eq!(g.total_wei, g.l2_fee_wei + U256::from(500));
        assert_eq!(g.l1_fee_wei, Some(U256::from(500)));
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core attribution::tests::gas_payment 2>&1 | tail -3`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement gas computation**

```rust
use alloy_primitives::{Address, B256, U256};
use crate::model::GasPaymentRow;

pub struct GasInput {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub tx_hash: B256, pub tx_from: Address,
    pub gas_used: u64, pub effective_gas_price: u128,
    pub l1_fee_wei: Option<U256>,
}

/// Charged whenever tx.from ∈ watched, even on reverted txs.
pub fn compute_gas_payment(i: GasInput) -> GasPaymentRow {
    let l2 = U256::from(i.gas_used) * U256::from(i.effective_gas_price);
    let total = l2 + i.l1_fee_wei.unwrap_or(U256::ZERO);
    GasPaymentRow {
        chain_id: i.chain_id, block_number: i.block_number, block_hash: i.block_hash,
        tx_hash: i.tx_hash, address: i.tx_from, gas_used: i.gas_used,
        effective_gas_price: i.effective_gas_price, l2_fee_wei: l2,
        l1_fee_wei: i.l1_fee_wei, total_wei: total,
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core attribution::tests::gas_payment 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/attribution.rs
git commit -m "feat(core): gas payment computation (L2 + optional OP L1 fee)"
```

### Task 4.3: Frame → EthTransferRow attribution

**Files:**
- Modify: `crates/passbook-core/src/attribution.rs`

- [ ] **Step 1: Failing test — frames map to in/out rows for watched addresses**

```rust
#[test]
fn frames_attribute_in_and_out_for_watched() {
    use crate::inspector::{CapturedFrame, FrameKind};
    let w = Address::repeat_byte(0xcc);
    let frames = vec![
        CapturedFrame { from: Address::repeat_byte(1), to: w,
            value: U256::from(9), kind: FrameKind::Call, trace_path:"0".into() },
        CapturedFrame { from: w, to: Address::repeat_byte(2),
            value: U256::from(3), kind: FrameKind::SelfDestruct, trace_path:"1".into() },
    ];
    let watch = [w].into_iter().collect();
    let rows = attribute_eth_frames(
        1, 7, B256::ZERO, Some(B256::repeat_byte(1)), false, &frames, &watch);
    assert_eq!(rows.len(), 2);
    let (inb, outb): (Vec<_>,Vec<_>) =
        rows.iter().partition(|r| r.direction == crate::model::Direction::In);
    assert_eq!(inb[0].amount_wei, U256::from(9));
    assert_eq!(inb[0].kind, crate::model::EthKind::Internal);
    assert_eq!(outb[0].amount_wei, U256::from(3));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core attribution::tests::frames_attribute 2>&1 | tail -3`
Expected: FAIL — `attribute_eth_frames` not found.

- [ ] **Step 3: Implement**

```rust
use std::collections::HashSet;
use crate::inspector::CapturedFrame;
use crate::model::{Direction, EthKind, EthTransferRow};

/// Top-level frames use kind=TopLevel (caller passes a `is_top_level`
/// flag via trace_path "tx:<i>"); internal frames use Internal. Here we
/// treat trace_path starting with "tx:" as top-level.
pub fn attribute_eth_frames(
    chain_id: u64, block_number: u64, block_hash: B256,
    tx_hash: Option<B256>, reverted: bool,
    frames: &[CapturedFrame], watched: &HashSet<Address>,
) -> Vec<EthTransferRow> {
    let mut out = Vec::new();
    for f in frames {
        let kind = if f.trace_path.starts_with("tx:") {
            EthKind::TopLevel } else { EthKind::Internal };
        if watched.contains(&f.to) {
            out.push(EthTransferRow {
                chain_id, block_number, block_hash, tx_hash, trace_path: f.trace_path.clone(),
                address: f.to, direction: Direction::In, counterparty: f.from,
                amount_wei: f.value, kind, reverted });
        }
        if watched.contains(&f.from) {
            out.push(EthTransferRow {
                chain_id, block_number, block_hash, tx_hash,
                trace_path: format!("{}:out", f.trace_path),
                address: f.from, direction: Direction::Out, counterparty: f.to,
                amount_wei: f.value, kind, reverted });
        }
    }
    out
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core attribution::tests::frames_attribute 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/attribution.rs
git commit -m "feat(core): frame -> eth transfer attribution"
```

---

## Phase 5 — Reconciliation

### Task 5.1: Observed-delta vs Σ-attribution reconciliation

**Files:**
- Create: `crates/passbook-core/src/reconcile.rs`

- [ ] **Step 1: Failing test — zero residual passes, nonzero produces a row**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};

    #[test]
    fn balanced_account_has_no_residual() {
        let addr = Address::repeat_byte(0xaa);
        let r = reconcile_account(ReconcileInput {
            chain_id:1, block_number:5, block_hash:B256::ZERO, address:addr,
            observed_delta: 100i128,             // balance went +100
            eth_in: U256::from(150), eth_out: U256::from(20),
            gas_paid: U256::from(30), system_signed: 0i128,
        });
        assert!(r.is_none()); // 150 - 20 - 30 == 100
    }

    #[test]
    fn imbalance_yields_unattributed_row() {
        let addr = Address::repeat_byte(0xaa);
        let r = reconcile_account(ReconcileInput {
            chain_id:1, block_number:5, block_hash:B256::ZERO, address:addr,
            observed_delta: 100i128, eth_in: U256::from(10), eth_out: U256::ZERO,
            gas_paid: U256::ZERO, system_signed: 0i128,
        }).unwrap();
        assert_eq!(r.residual_wei, U256::from(90)); // |100 - 10|
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core reconcile 2>&1 | tail -3`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement reconciliation**

```rust
use alloy_primitives::{Address, B256, U256};
use crate::model::UnattributedDeltaRow;

pub struct ReconcileInput {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub address: Address,
    /// observed post-state balance delta (new - old), signed wei.
    pub observed_delta: i128,
    pub eth_in: U256, pub eth_out: U256, pub gas_paid: U256,
    /// recognised system credit (+) / debit (-) in wei.
    pub system_signed: i128,
}

/// attributed = eth_in - eth_out - gas_paid + system_signed.
/// Returns Some(row) iff |observed - attributed| != 0. A returned row means
/// the caller MUST treat the block as a processing failure (do not advance,
/// do not emit FinishedHeight) and persist this row as the diagnostic.
pub fn reconcile_account(i: ReconcileInput) -> Option<UnattributedDeltaRow> {
    // Work in i128 wei deltas; amounts here are within i128 range for ETH.
    let to_i = |u: U256| -> i128 { u.try_into().unwrap_or(i128::MAX) };
    let attributed: i128 = to_i(i.eth_in)
        .saturating_sub(to_i(i.eth_out))
        .saturating_sub(to_i(i.gas_paid))
        .saturating_add(i.system_signed);
    let residual = i.observed_delta - attributed;
    if residual == 0 { return None; }
    Some(UnattributedDeltaRow {
        chain_id: i.chain_id, block_number: i.block_number, block_hash: i.block_hash,
        address: i.address,
        observed_wei: U256::from(i.observed_delta.unsigned_abs()),
        attributed_wei: U256::from(attributed.unsigned_abs()),
        residual_wei: U256::from(residual.unsigned_abs()),
    })
}
```

> Note: i128 wei holds up to ~1.7e20 ETH — safe for any real balance delta. If a fixture exceeds it the `try_into` saturates and reconciliation will (correctly) flag a residual rather than silently wrap.

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core reconcile 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/reconcile.rs
git commit -m "feat(core): per-account reconciliation + residual detection"
```

---

## Phase 6 — Config + the ExEx loop

### Task 6.1: Config parse/validate

**Files:**
- Create: `crates/passbook-core/src/config.rs`

- [ ] **Step 1: Failing test — valid list parses, malformed errors**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_valid_addresses() {
        let c = PassbookConfig::from_parts(
            vec!["0x0000000000000000000000000000000000000001".into()],
            "/tmp/x.db".into()).unwrap();
        assert_eq!(c.watched.len(), 1);
    }
    #[test]
    fn rejects_malformed_address() {
        assert!(PassbookConfig::from_parts(vec!["nope".into()], "/tmp/x.db".into()).is_err());
    }
    #[test]
    fn empty_list_is_disabled() {
        assert!(PassbookConfig::from_parts(vec![], "/tmp/x.db".into()).unwrap().watched.is_empty());
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core config 2>&1 | tail -3`
Expected: FAIL — `PassbookConfig` not found.

- [ ] **Step 3: Implement**

```rust
use alloy_primitives::Address;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct PassbookConfig {
    pub watched: HashSet<Address>,
    pub db_path: PathBuf,
}

impl PassbookConfig {
    /// Malformed address ⇒ Err (binary maps this to "abort node startup").
    /// Empty list ⇒ Ok with empty set (binary treats as ExEx-disabled).
    pub fn from_parts(addrs: Vec<String>, db_path: PathBuf) -> eyre::Result<Self> {
        let mut watched = HashSet::new();
        for a in addrs {
            let a = a.trim();
            if a.is_empty() { continue; }
            let addr = Address::from_str(a)
                .map_err(|e| eyre::eyre!("invalid watched address {a:?}: {e}"))?;
            watched.insert(addr);
        }
        if watched.len() > 10 {
            tracing::warn!(n = watched.len(), "watched set larger than design target (<10)");
        }
        Ok(Self { watched, db_path })
    }
    pub fn enabled(&self) -> bool { !self.watched.is_empty() }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core config 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/config.rs
git commit -m "feat(core): config parse/validate (abort on malformed addr)"
```

### Task 6.2: Per-block processing pipeline (pure orchestrator)

**Files:**
- Modify: `crates/passbook-core/src/exex.rs`

This is the heart: a **pure** function that, given decoded inputs for one block, produces either a `BlockBatch` or a `ProcessingError` (residual / decode failure). Keeping it pure makes the retry-until-success behaviour testable without a node.

- [ ] **Step 1: Failing test — a clean block yields a batch with FinishedHeight allowed**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};

    #[test]
    fn clean_block_produces_batch() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id:1, block_number:10, block_hash:B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![], frames: vec![], gas: vec![],
            // observed delta 0, nothing attributed -> balanced
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
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core exex::tests 2>&1 | tail -3`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement the orchestrator**

```rust
use alloy_primitives::{Address, B256, U256};
use std::collections::HashSet;
use crate::erc20::{decode_transfer, RawLog};
use crate::inspector::CapturedFrame;
use crate::model::{Direction, Erc20TransferRow, GasPaymentRow};
use crate::reconcile::{reconcile_account, ReconcileInput};
use crate::ledger::writer::BlockBatch;

pub struct BlockInputs {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub watched: HashSet<Address>,
    pub erc20_logs: Vec<(Option<B256>, u64, RawLog)>, // (tx_hash, log_index, log)
    pub frames: Vec<(Option<B256>, bool, CapturedFrame)>, // (tx_hash, reverted, frame)
    pub gas: Vec<GasPaymentRow>,
    pub account_deltas: Vec<(Address, i128)>,        // watched accounts touched
    pub system_signed: Vec<(Address, i128)>,         // recognised system credits
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
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
    let mut unattributed = Vec::new();
    for (addr, observed) in &i.account_deltas {
        if !i.watched.contains(addr) { continue; }
        if let Some(row) = reconcile_account(ReconcileInput {
            chain_id: i.chain_id, block_number: i.block_number,
            block_hash: i.block_hash, address: *addr, observed_delta: *observed,
            eth_in: eth_in.get(addr).copied().unwrap_or(U256::ZERO),
            eth_out: eth_out.get(addr).copied().unwrap_or(U256::ZERO),
            gas_paid: gas_paid.get(addr).copied().unwrap_or(U256::ZERO),
            system_signed: sys.get(addr).copied().unwrap_or(0),
        }) {
            return Err(ProcessingError::UnexplainedResidual {
                block: i.block_number, address: *addr,
                residual: *observed }); // diagnostic row built by caller before halt
            #[allow(unreachable_code)] { unattributed.push(row); }
        }
    }
    Ok(BlockBatch {
        chain_id: i.chain_id, block_number: i.block_number, block_hash: i.block_hash,
        eth, erc20, gas: i.gas, unattributed })
}
```

> The `unattributed_deltas` diagnostic row is persisted by the loop (Task 6.3) on the halt path before retrying — `process_block` returning `Err` is the signal not to advance.

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core exex::tests 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/exex.rs
git commit -m "feat(core): pure per-block processing orchestrator"
```

### Task 6.3: The ExEx notification loop (retry-until-success, reorg, FinishedHeight)

**Files:**
- Modify: `crates/passbook-core/src/exex.rs`

> reth-coupled glue. Provider/EVM accessor names (`ctx.provider()`, `evm_config()`, parent-state method) must be confirmed against the Task 0.1 rev; the research flagged these as node-generic and version-sensitive. The structure below is the contract; integration tests (Task 6.4) prove it.

- [ ] **Step 1: Implement the loop**

```rust
use futures::TryStreamExt;
use reth_ethereum::exex::{ExExContext, ExExEvent, ExExNotification};
use reth_ethereum::node::api::FullNodeComponents;
use crate::config::PassbookConfig;
use crate::stack::StackAdapter;
use crate::ledger::Ledger;
use crate::model::UnattributedDeltaRow;
use std::sync::{Arc, Mutex};
use alloy_primitives::U256;

pub async fn run_passbook<Node, S>(
    mut ctx: ExExContext<Node>,
    cfg: PassbookConfig,
    ledger: Arc<Mutex<Ledger>>,
    make_adapter: impl Fn(&[u8]) -> S + Send + Sync + 'static,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    S: StackAdapter,
{
    while let Some(notification) = ctx.notifications.try_next().await? {
        // Reorg/revert first: delete reverted block hashes.
        if let Some(reverted) = notification.reverted_chain() {
            let hashes: Vec<_> = reverted.blocks_iter()
                .map(|b| b.hash()).collect();
            let mut l = ledger.lock().unwrap();
            crate::ledger::writer::delete_blocks(l.conn_mut(), cfg_chain_id(&cfg, &ctx), &hashes)?;
        }
        if let Some(chain) = notification.committed_chain() {
            for block in chain.blocks_iter() {
                // Retry-until-success: a deterministic failure halts here.
                let mut backoff = std::time::Duration::from_millis(200);
                loop {
                    match process_one_committed_block(
                        &ctx, &chain, block, &cfg, &make_adapter).await
                    {
                        Ok(batch) => {
                            let mut l = ledger.lock().unwrap();
                            crate::ledger::writer::write_block(l.conn_mut(), &batch)?;
                            break;
                        }
                        Err(ProcessingError::UnexplainedResidual {
                            block: bn, address, residual }) => {
                            // Persist the diagnostic, then halt-by-retry.
                            {
                                let mut l = ledger.lock().unwrap();
                                let row = UnattributedDeltaRow {
                                    chain_id: cfg_chain_id(&cfg, &ctx), block_number: bn,
                                    block_hash: block.hash(), address,
                                    observed_wei: U256::from(residual.unsigned_abs()),
                                    attributed_wei: U256::ZERO,
                                    residual_wei: U256::from(residual.unsigned_abs()) };
                                crate::ledger::writer::write_unattributed(l.conn_mut(), &row)?;
                            }
                            tracing::error!(block = bn, %address,
                                "unexplained residual — ExEx stalled, not advancing");
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                            continue;
                        }
                        Err(e) => {
                            tracing::error!(?e, "block processing failed — retrying");
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                            continue;
                        }
                    }
                }
            }
            // Only now is every block in the chain durably committed.
            ctx.events.send(ExExEvent::FinishedHeight(chain.tip().num_hash()))?;
        }
    }
    Ok(())
}

fn cfg_chain_id<Node: FullNodeComponents>(
    _cfg: &PassbookConfig, ctx: &ExExContext<Node>) -> u64 {
    ctx.config.chain.chain().id()
}

/// Build BlockInputs for one committed block: scan receipts for ERC20 logs,
/// compute per-account BundleState deltas, and ONLY if a watched account has a
/// balance/nonce delta, re-execute the block with ValueInspector against the
/// committed parent state (pruning-independent). Then call process_block.
async fn process_one_committed_block<Node, S>(
    ctx: &ExExContext<Node>,
    chain: &reth_ethereum::exex::Chain,
    block: &impl BlockAccess,
    cfg: &PassbookConfig,
    make_adapter: &(impl Fn(&[u8]) -> S),
) -> Result<crate::ledger::writer::BlockBatch, ProcessingError>
where Node: FullNodeComponents, S: StackAdapter {
    // 1. ERC20 path: always, from receipts (no tracing).
    // 2. Gate: read chain.execution_outcome() per-account old/new balance+nonce;
    //    intersect with cfg.watched. Empty -> skip native path.
    // 3. If non-empty: re-exec the block with ValueInspector via
    //    evm_config.evm_with_env_and_inspector + create_executor on
    //    StateProviderDatabase at block.parent_hash().
    // 4. Assemble BlockInputs and call process_block.
    // Concrete provider/evm accessor calls are confirmed in Task 6.4.
    todo!("wired + asserted by integration test 6.4")
}

trait BlockAccess { fn hash(&self) -> alloy_primitives::B256; }
```

> `write_unattributed` is a one-row helper: add it to `ledger/writer.rs` (mirror `write_block`'s `unattributed` INSERT OR REPLACE, single-row, own transaction).

- [ ] **Step 2: Add `write_unattributed` to writer.rs and a unit test for it**

In `ledger/writer.rs`:

```rust
pub fn write_unattributed(
    conn: &mut Connection, r: &UnattributedDeltaRow,
) -> eyre::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO unattributed_deltas VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            r.chain_id, r.block_number, h(&r.block_hash), a(&r.address),
            u(&r.observed_wei), u(&r.attributed_wei), u(&r.residual_wei)])?;
    Ok(())
}
```

Test: write one, assert it is queryable.

- [ ] **Step 3: Compile (loop body; `process_one_committed_block` still `todo!`)**

Run: `cargo build -p passbook-core 2>&1 | tail -10`
Expected: PASS (warnings for `todo!`/unused OK). Fix any reth path mismatches per the compiler against the Task 0.1 rev, recording deltas in `docs/reth-pin.md`.

- [ ] **Step 4: Run unit tests still green**

Run: `cargo test -p passbook-core 2>&1 | tail -5`
Expected: PASS (all prior unit tests; `process_one_committed_block` not yet exercised)

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/exex.rs crates/passbook-core/src/ledger/writer.rs
git commit -m "feat(core): ExEx loop scaffold (retry-until-success, reorg, FinishedHeight)"
```

### Task 6.4: Integration test — wire `process_one_committed_block`, prove capture + reconcile

**Files:**
- Create: `crates/passbook-core/tests/exex_integration.rs`
- Modify: `crates/passbook-core/src/exex.rs` (replace the `todo!`)

- [ ] **Step 1: Write the failing integration test using reth-exex-test-utils**

```rust
//! Uses reth_exex_test_utils to build a synthetic chain with:
//! - an ERC20 Transfer log to a watched addr
//! - a tx that forwards ETH through a contract to a watched addr (internal)
//! - a tx sent BY a watched addr (gas)
//! Asserts ledger rows and zero unattributed residual.
use reth_exex_test_utils::test_exex_context;

#[tokio::test]
async fn captures_erc20_internal_and_gas_with_zero_residual() {
    let (ctx, mut handle) = test_exex_context().await.unwrap();
    // Build a committed-chain notification with the three scenarios using the
    // test-utils block builder; send via handle. Confirm exact builder API
    // against the Task 0.1 rev (reth_exex_test_utils docs).
    // ... construct cfg with the watched addr, in-memory/temp Ledger ...
    // ... spawn run_passbook, send ChainCommitted, await FinishedHeight ...
    // ASSERT: erc20_transfers has 1 row; eth_transfers has the internal row;
    //         gas_payments has the gas row; unattributed_deltas is EMPTY.
    let _ = (ctx, &mut handle);
    panic!("implement against pinned reth-exex-test-utils API");
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core --test exex_integration 2>&1 | tail -5`
Expected: FAIL (`panic!`)

- [ ] **Step 3: Implement `process_one_committed_block` against the real API**

Replace the `todo!` using the verified shapes from research:
- ERC20: `chain.blocks_and_receipts()` → per receipt `logs()`; map each to `erc20::RawLog`.
- Gate: `chain.execution_outcome().bundle_accounts_iter()` → `(addr, BundleAccount)`; `original_info` vs `info` for balance/nonce delta; **split per block** via `chain.execution_outcome_at_block(n)` (research flagged the bundle is multi-block aggregate).
- Re-exec: `let sp = ctx.provider().history_by_block_hash(block.parent_hash())?; let db = StateProviderDatabase::new(&sp); let mut state = State::builder().with_database(db).with_bundle_update().build(); let evm_env = evm_config.evm_env(block.header())?; let evm = evm_config.evm_with_env_and_inspector(&mut state, evm_env, &mut insp); let ex = evm_config.create_executor(evm, evm_config.context_for_block(block.sealed_block())?); ex.execute_block(block.transactions_recovered())?;`
- Gas: per tx `gas_used = cumulative_gas_used` delta from receipts; `effective_gas_price = tx.effective_gas_price(base_fee)`; `l1 = adapter.l1_data_fee_wei(tx_index)`.
- Assemble `BlockInputs`, call `process_block`.

Confirm each accessor name compiles; record any deviation in `docs/reth-pin.md`.

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core --test exex_integration 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/tests/exex_integration.rs crates/passbook-core/src/exex.rs
git commit -m "test(core): integration — erc20+internal+gas capture, zero residual"
```

### Task 6.5: Fault-injection, reorg, resume integration tests

**Files:**
- Modify: `crates/passbook-core/tests/exex_integration.rs`

- [ ] **Step 1: Write three failing tests**

```rust
#[tokio::test]
async fn fault_injected_residual_stalls_without_advancing() {
    // Build a block whose observed delta intentionally != Σ attribution.
    // ASSERT: no FinishedHeight emitted, unattributed_deltas row written,
    //         loop still retrying (last_block unchanged).
    panic!("implement");
}
#[tokio::test]
async fn reorg_replaces_rows_no_dup() {
    // commit block@hashA -> revert -> commit alternate block@hashB.
    // ASSERT: rows for hashA gone, hashB present, no duplicates.
    panic!("implement");
}
#[tokio::test]
async fn restart_resumes_no_gap_no_dup() {
    // process N blocks, drop+reopen Ledger, replay last notification.
    // ASSERT: idempotent — counts unchanged, last_block correct.
    panic!("implement");
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core --test exex_integration fault_ reorg_ restart_ 2>&1 | tail -5`
Expected: FAIL (panics)

- [ ] **Step 3: Implement all three** against the test-utils API (mirror Task 6.4 setup; for fault injection, craft inputs with a deliberate residual; for reorg, send `ChainReverted` then `ChainCommitted`; for restart, reconstruct `Ledger` from the same temp path and re-send the last notification).

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core --test exex_integration 2>&1 | tail -5`
Expected: PASS (all integration tests)

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/tests/exex_integration.rs
git commit -m "test(core): fault-injection halt, reorg replace, restart resume"
```

---

## Phase 7 — RPC namespace

### Task 7.1: `passbook` jsonrpsee namespace

**Files:**
- Modify: `crates/passbook-core/src/rpc.rs`

- [ ] **Step 1: Failing test — impl returns health/transfers from a populated ledger**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[tokio::test]
    async fn health_method_returns_last_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = crate::ledger::Ledger::open(&dir.path().join("r.db"), 1).unwrap();
        l.conn().execute("INSERT INTO meta(k,v) VALUES('last_block','77')
            ON CONFLICT(k) DO UPDATE SET v=excluded.v", []).unwrap();
        let rpc = PassbookRpc { ledger: Arc::new(Mutex::new(l)), chain_id: 1 };
        let h = PassbookApiServer::health(&rpc).await.unwrap();
        assert_eq!(h.last_block, Some(77));
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core rpc 2>&1 | tail -3`
Expected: FAIL — symbols not found.

- [ ] **Step 3: Implement the trait + impl**

```rust
use jsonrpsee::{proc_macros::rpc, core::RpcResult};
use std::sync::{Arc, Mutex};
use crate::ledger::Ledger;
use crate::ledger::queries::{Health, TransfersPage, get_transfers, health};

#[rpc(server, namespace = "passbook")]
pub trait PassbookApi {
    #[method(name = "health")]
    async fn health(&self) -> RpcResult<Health>;
    #[method(name = "getTransfers")]
    async fn get_transfers(
        &self, address: String, from_block: Option<u64>, to_block: Option<u64>,
        kind: Option<String>, cursor: Option<u64>,
    ) -> RpcResult<TransfersPage>;
}

#[derive(Clone)]
pub struct PassbookRpc { pub ledger: Arc<Mutex<Ledger>>, pub chain_id: u64 }

fn err<E: std::fmt::Display>(e: E) -> jsonrpsee::types::ErrorObjectOwned {
    jsonrpsee::types::ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
}

#[async_trait::async_trait]
impl PassbookApiServer for PassbookRpc {
    async fn health(&self) -> RpcResult<Health> {
        let l = self.ledger.lock().map_err(err)?;
        health(l.conn()).map_err(err)
    }
    async fn get_transfers(
        &self, address: String, from_block: Option<u64>, to_block: Option<u64>,
        kind: Option<String>, cursor: Option<u64>,
    ) -> RpcResult<TransfersPage> {
        let l = self.ledger.lock().map_err(err)?;
        get_transfers(l.conn(), self.chain_id, &address, from_block, to_block,
            kind.as_deref(), cursor, 500).map_err(err)
    }
}
```

> RPC handlers never swallow errors — every failure becomes a JSON-RPC error (spec §error-handling). Add `async-trait` to `passbook-core` deps if `#[rpc]` requires it at the pinned jsonrpsee version (confirm in Task 0.1 / build).

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core rpc 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/rpc.rs crates/passbook-core/Cargo.toml
git commit -m "feat(core): passbook JSON-RPC namespace (health, getTransfers)"
```

---

## Phase 8 — Stack adapters + dual binaries

### Task 8.1: Ethereum stack adapter (no L1 fee)

**Files:**
- Modify: `crates/passbook-stack-ethereum/src/lib.rs`

- [ ] **Step 1: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use passbook_core::stack::StackAdapter;
    #[test]
    fn ethereum_adapter_never_has_l1_fee() {
        assert_eq!(EthereumStack.l1_data_fee_wei(0), None);
        assert!(EthereumStack.system_credits().is_empty());
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-stack-ethereum 2>&1 | tail -3`
Expected: FAIL — `EthereumStack` not found.

- [ ] **Step 3: Implement**

```rust
use passbook_core::stack::StackAdapter;
use alloy_primitives::U256;

pub struct EthereumStack;

impl StackAdapter for EthereumStack {
    fn l1_data_fee_wei(&self, _tx_index: usize) -> Option<U256> { None }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-stack-ethereum 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-stack-ethereum/src/lib.rs
git commit -m "feat(stack-eth): ethereum adapter (no L1 data fee)"
```

### Task 8.2: Optimism stack adapter (L1 data fee via reth-optimism-evm)

**Files:**
- Modify: `crates/passbook-stack-optimism/src/lib.rs`

- [ ] **Step 1: Failing test — adapter returns Some for a non-deposit tx**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use passbook_core::stack::StackAdapter;
    #[test]
    fn optimism_adapter_exposes_precomputed_l1_fees() {
        // Construct with a precomputed per-tx fee table (the binary fills this
        // from extract_l1_info + l1_tx_data_fee per block).
        let a = OptimismStack::from_fees(vec![Some(alloy_primitives::U256::from(500))]);
        assert_eq!(a.l1_data_fee_wei(0), Some(alloy_primitives::U256::from(500)));
        assert_eq!(a.l1_data_fee_wei(9), None); // out of range -> None
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-stack-optimism 2>&1 | tail -3`
Expected: FAIL — `OptimismStack` not found.

- [ ] **Step 3: Implement (fee table + a builder using reth-optimism-evm)**

```rust
use passbook_core::stack::StackAdapter;
use alloy_primitives::U256;

/// Per-tx L1 data fees for ONE block, precomputed by the binary via
/// reth_optimism_evm::{extract_l1_info, RethL1BlockInfo::l1_tx_data_fee}
/// (deposit txs -> None/zero). Kept as a plain table so core stays OP-free.
pub struct OptimismStack { fees: Vec<Option<U256>> }

impl OptimismStack {
    pub fn from_fees(fees: Vec<Option<U256>>) -> Self { Self { fees } }

    /// Built per block in op-reth-passbook:
    ///   let mut info = reth_optimism_evm::extract_l1_info(block.body())?;
    ///   for (i, tx) in txs.enumerate() {
    ///     let raw = tx.encoded_2718();
    ///     let fee = info.l1_tx_data_fee(&spec, ts, bn, &raw, tx.is_deposit())?;
    ///     fees.push((!tx.is_deposit()).then_some(fee));
    ///   }
    /// Signature names confirmed against the Task 0.1 op-reth rev.
    pub fn placeholder_doc() {}
}

impl StackAdapter for OptimismStack {
    fn l1_data_fee_wei(&self, tx_index: usize) -> Option<U256> {
        self.fees.get(tx_index).copied().flatten()
    }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-stack-optimism 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-stack-optimism/src/lib.rs
git commit -m "feat(stack-op): optimism adapter (per-tx L1 data fee table)"
```

### Task 8.3: CLI args struct

**Files:**
- Create: `crates/passbook-core/src/cli.rs`
- Modify: `crates/passbook-core/src/lib.rs` (add `pub mod cli;`)

- [ ] **Step 1: Failing test — clap parses dotted flags + env**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)] struct W { #[command(flatten)] p: PassbookArgs }
    #[test]
    fn parses_flags() {
        let w = W::parse_from([
            "x", "--passbook.addresses",
            "0x0000000000000000000000000000000000000001",
            "--passbook.db-path", "/tmp/p.db"]);
        assert_eq!(w.p.addresses.len(), 1);
        assert_eq!(w.p.db_path.to_str().unwrap(), "/tmp/p.db");
    }
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p passbook-core cli 2>&1 | tail -3`
Expected: FAIL — `PassbookArgs` not found.

- [ ] **Step 3: Implement**

```rust
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Clone, Args)]
pub struct PassbookArgs {
    /// Comma-separated watched addresses (≤10). Absent ⇒ ExEx disabled.
    #[arg(long = "passbook.addresses", env = "PASSBOOK_ADDRESSES",
          value_delimiter = ',', default_value = "")]
    pub addresses: Vec<String>,

    /// Ledger SQLite path.
    #[arg(long = "passbook.db-path", env = "PASSBOOK_DB_PATH",
          default_value = "/data/passbook.db")]
    pub db_path: PathBuf,
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p passbook-core cli 2>&1 | tail -3`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/passbook-core/src/cli.rs crates/passbook-core/src/lib.rs
git commit -m "feat(core): PassbookArgs clap extension"
```

### Task 8.4: `reth-passbook` L1 binary

**Files:**
- Modify: `crates/bin/reth-passbook/src/main.rs`

- [ ] **Step 1: Implement main (drop-in safe: empty addrs ⇒ stock node)**

```rust
use clap::Parser;
use reth_ethereum::{cli::Cli, node::EthereumNode};
use reth_ethereum::cli::chainspec::EthereumChainSpecParser;
use passbook_core::{cli::PassbookArgs, config::PassbookConfig,
    ledger::Ledger, rpc::{PassbookRpc, PassbookApiServer}};
use passbook_stack_ethereum::EthereumStack;
use std::sync::{Arc, Mutex};

fn main() -> eyre::Result<()> {
    Cli::<EthereumChainSpecParser, PassbookArgs>::parse().run(
        async move |builder, args: PassbookArgs| {
            let cfg = PassbookConfig::from_parts(args.addresses.clone(), args.db_path.clone())?;
            if !cfg.enabled() {
                // Drop-in: stock node, no ExEx, no RPC namespace.
                let h = builder.node(EthereumNode::default()).launch().await?;
                return h.wait_for_node_exit().await;
            }
            let ledger = Arc::new(Mutex::new(
                Ledger::open(&cfg.db_path, /*chain_id filled at launch*/ 1)?));
            let rpc = PassbookRpc { ledger: ledger.clone(), chain_id: 1 };
            let h = builder
                .node(EthereumNode::default())
                .extend_rpc_modules(move |ctx| {
                    ctx.modules.merge_configured(PassbookApiServer::into_rpc(rpc.clone()))?;
                    Ok(())
                })
                .install_exex("passbook", {
                    let cfg = cfg.clone(); let ledger = ledger.clone();
                    async move |ctx| Ok(passbook_core::exex::run_passbook(
                        ctx, cfg, ledger, |_| EthereumStack))
                })
                .launch().await?;
            h.wait_for_node_exit().await
        })
}
```

> `chain_id` placeholder: set it from `ctx.config.chain.chain().id()` inside `run_passbook` (already wired via `cfg_chain_id`) and pass the real id into `Ledger::open` by deferring open into the ExEx if the node API requires the chain at launch only — confirm ordering in Task 0.1 build; if `Cli` exposes the chain spec pre-launch, read it there instead. Record the chosen approach in `docs/reth-pin.md`.

- [ ] **Step 2: Build**

Run: `cargo build -p reth-passbook 2>&1 | tail -10`
Expected: PASS (fix reth API path deltas per compiler; record in `docs/reth-pin.md`)

- [ ] **Step 3: Drop-in smoke test (no addresses ⇒ stock `--help` works)**

Run: `cargo run -p reth-passbook -- --help 2>&1 | grep -q passbook.addresses && echo FLAG-PRESENT`
Expected: `FLAG-PRESENT`

- [ ] **Step 4: Commit**

```bash
git add crates/bin/reth-passbook/src/main.rs
git commit -m "feat(bin): reth-passbook L1 binary (drop-in safe)"
```

### Task 8.5: `op-reth-passbook` OP binary

**Files:**
- Modify: `crates/bin/op-reth-passbook/src/main.rs`

- [ ] **Step 1: Implement main (same shape, OpNode + OptimismStack)**

```rust
use clap::Parser;
use reth_op::cli::Cli;
use reth_op::node::OpNode;
use passbook_core::{cli::PassbookArgs, config::PassbookConfig,
    ledger::Ledger, rpc::{PassbookRpc, PassbookApiServer}};
use passbook_stack_optimism::OptimismStack;
use std::sync::{Arc, Mutex};

fn main() -> eyre::Result<()> {
    // OP Cli default Ext is RollupArgs; flatten it so --rollup.* survive.
    #[derive(clap::Args, Debug, Clone)]
    struct OpExt {
        #[command(flatten)] passbook: PassbookArgs,
        // #[command(flatten)] rollup: reth_op::node::RollupArgs, // confirm path in 0.1
    }
    Cli::<_, OpExt>::parse().run(async move |builder, ext: OpExt| {
        let args = ext.passbook;
        let cfg = PassbookConfig::from_parts(args.addresses.clone(), args.db_path.clone())?;
        if !cfg.enabled() {
            let h = builder.node(OpNode::default()).launch().await?;
            return h.wait_for_node_exit().await;
        }
        let ledger = Arc::new(Mutex::new(Ledger::open(&cfg.db_path, 10)?));
        let rpc = PassbookRpc { ledger: ledger.clone(), chain_id: 10 };
        let h = builder
            .node(OpNode::default())
            .extend_rpc_modules(move |ctx| {
                ctx.modules.merge_configured(PassbookApiServer::into_rpc(rpc.clone()))?;
                Ok(())
            })
            .install_exex("passbook", {
                let cfg = cfg.clone(); let ledger = ledger.clone();
                async move |ctx| Ok(passbook_core::exex::run_passbook(
                    ctx, cfg, ledger,
                    // adapter built per-block in process_one_committed_block;
                    // here we hand a constructor closure over precomputed fees.
                    |_| OptimismStack::from_fees(vec![])))
            })
            .launch().await?;
        h.wait_for_node_exit().await
    })
}
```

> The `make_adapter` closure currently takes raw bytes; refine its signature in Task 6.4 so the OP binary can build a per-block `OptimismStack::from_fees(...)` from `extract_l1_info` + `l1_tx_data_fee`. Keep the signature change confined to `exex.rs` + the two binaries.

- [ ] **Step 2: Build**

Run: `cargo build -p op-reth-passbook 2>&1 | tail -10`
Expected: PASS (resolve `reth_op::node::OpNode` / `RollupArgs` paths per compiler against Task 0.1 rev; record in `docs/reth-pin.md`)

- [ ] **Step 3: Flag-present smoke test**

Run: `cargo run -p op-reth-passbook -- --help 2>&1 | grep -q passbook.addresses && echo FLAG-PRESENT`
Expected: `FLAG-PRESENT`

- [ ] **Step 4: Commit**

```bash
git add crates/bin/op-reth-passbook/src/main.rs
git commit -m "feat(bin): op-reth-passbook OP binary (drop-in safe)"
```

---

## Phase 9 — Docker + CI

### Task 9.1: One multi-stage Dockerfile → two images

**Files:**
- Create: `Dockerfile`, `.dockerignore`

- [ ] **Step 1: Write the Dockerfile**

```dockerfile
# syntax=docker/dockerfile:1
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release -p reth-passbook -p op-reth-passbook

FROM debian:bookworm-slim AS reth-passbook
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/reth-passbook /usr/local/bin/reth-passbook
VOLUME /data
ENTRYPOINT ["/usr/local/bin/reth-passbook"]

FROM debian:bookworm-slim AS op-reth-passbook
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/op-reth-passbook /usr/local/bin/op-reth-passbook
VOLUME /data
ENTRYPOINT ["/usr/local/bin/op-reth-passbook"]
```

`.dockerignore`: `target/`, `.git/`.

- [ ] **Step 2: Build both image targets**

Run: `docker build -t reth-passbook:dev --target reth-passbook . && docker build -t op-reth-passbook:dev --target op-reth-passbook . && echo IMAGES-OK`
Expected: `IMAGES-OK`

- [ ] **Step 3: Drop-in smoke test in-image**

Run: `docker run --rm reth-passbook:dev --help 2>&1 | grep -q passbook.addresses && echo OK`
Expected: `OK`

- [ ] **Step 4: Commit**

```bash
git add Dockerfile .dockerignore
git commit -m "build: one multi-stage Dockerfile -> reth-passbook + op-reth-passbook"
```

### Task 9.2: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Write CI (fmt, clippy, test, both image builds)**

```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85
        with: { components: rustfmt, clippy }
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
  images:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: docker build --target reth-passbook -t reth-passbook:ci .
      - run: docker build --target op-reth-passbook -t op-reth-passbook:ci .
      - run: docker run --rm reth-passbook:ci --help | grep -q passbook.addresses
      - run: docker run --rm op-reth-passbook:ci --help | grep -q passbook.addresses
```

- [ ] **Step 2: Validate YAML locally**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('YAML-OK')"`
Expected: `YAML-OK`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: fmt+clippy+test and both image builds"
```

---

## Phase 10 — Docs + validation matrix sign-off

### Task 10.1: README + image tagging convention

**Files:**
- Create: `README.md`

- [ ] **Step 1: Write README** covering: what Passbook is; drop-in safety (no `--passbook.addresses` ⇒ stock node); the two images and `<upstream-version>-passbook<N>` tag scheme; how to bump upstream (`make bump REV=<sha>` → `make verify-pin` → re-tag); the `passbook_health` / `passbook_getTransfers` RPC methods; the never-lose guarantee and the "deterministic failure halts indexing" consequence.

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: README (drop-in, image tags, bump procedure, RPC, guarantees)"
```

### Task 10.2: Validation matrix sign-off

**Files:**
- Create: `docs/validation.md`

- [ ] **Step 1: Map every spec validation-matrix row to its test and run it**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: PASS

Write `docs/validation.md` as a table: each spec property (ERC20 in/out, internal ETH, gas L1&OP, completeness/zero residual, halt-on-failure, resume-after-fix, reorg safety, restart safety, drop-in safety, dual-stack) → the exact test name (`exex_integration::*`, the binary smoke tests, the image builds) that verifies it. Every row must point at a test that exists and passes; if any row has no test, add the test before signing off.

- [ ] **Step 2: Commit**

```bash
git add docs/validation.md
git commit -m "docs: validation matrix mapped to passing tests"
```

---

## Self-review notes

- **Spec coverage:** ERC20 (Phase 2/6.4), internal+top-level native (Phase 3/4.3/6.4), gas incl. OP L1 (4.2/8.2), reconciliation+residual halt (Phase 5/6.2/6.5), reorg+resume (1.5/6.5), atomic retry-until-success never-skip (6.3/6.5), SQLite separate store + WAL+FULL (1.3), RPC namespace auto-enabled when active (Phase 7/8.4–8.5), drop-in safety (8.4–8.5/9.1), dual images one workspace (Phase 0/9.1), config abort-on-malformed (6.1/8.4). The three spec "open items" are resolved: revision pin (0.1/0.2, `docs/reth-pin.md`), inspector choice (Phase 3 — custom value-only inspector, with rationale), SQLite pragmas (1.3 — WAL + synchronous=FULL + busy_timeout + foreign_keys).
- **Highest-risk integration point (flagged for the executor):** L1+OP facade co-resolution in one workspace is only verified inside an op-reth example tree — Task 0.1 is a hard gate; if it cannot link both facades from one rev, stop and renegotiate the dual-binary topology before Phase 1. revm-inspectors API churn is sidestepped by the custom inspector. reth provider/EVM/test-utils accessor names are version-sensitive; every reth-coupled task carries an explicit "confirm against Task 0.1 rev, record deltas in `docs/reth-pin.md`" step so an upstream bump has one documented blast radius.
- **Type consistency:** `BlockBatch`, `EthTransferRow`/`Erc20TransferRow`/`GasPaymentRow`/`UnattributedDeltaRow`, `Direction`/`EthKind`, `StackAdapter::{l1_data_fee_wei,system_credits}`, `process_block`/`ProcessingError`, `run_passbook`, `PassbookConfig`, `PassbookArgs`, `PassbookRpc`/`PassbookApiServer` are defined once and referenced consistently across tasks.
