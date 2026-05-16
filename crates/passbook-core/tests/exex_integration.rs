//! Task 6.4 ExEx integration test (reworked).
//!
//! Builds *genuinely executed* synthetic committed chains (so their
//! `BundleState` + receipts are internally consistent by construction —
//! they come from a real `EthEvmConfig` block execution, not hand-authored
//! numbers), drives the per-block pipeline + `run_passbook` through the
//! pinned `reth_exex_test_utils` harness, and asserts the durable ledger
//! rows plus a ZERO unattributed residual for the watched address.
//!
//! Two scenarios:
//!
//! 1. `erc20_internal_gas_capture_zero_residual` — single block (parent =
//!    genesis). The watched `W` is exercised across every capture path in
//!    one block:
//!      * tx0: `S -> TOKEN` — ERC20 `Transfer(S, W, amt)` log ⇒ erc20 row.
//!      * tx1: `S -> SDFWD` w/ value — internal `SELFDESTRUCT` forwards the
//!        whole balance to `W` ⇒ internal inbound eth row.
//!      * tx2: `S -> CFWD` w/ value — internal plain value **`CALL`** to
//!        the *codeless EOA* `W` ⇒ a second internal inbound eth row
//!        (proves nested value-CALL frames to codeless EOAs are captured).
//!      * tx3: `W -> SINK` plain value transfer — `tx.from == W` ⇒ a
//!        `gas_payments` row + a top-level eth out of W.
//!
//!    Reconciliation nets to zero for `W`.
//!
//! 2. `parent_state_readonly_two_block_chain_zero_residual` — a 2-block
//!    committed chain. Block 1 **deploys** a CALL-forwarder contract via a
//!    CREATE tx (its runtime code lives only in block 1's `BundleState`,
//!    NOT in genesis). Block 2 calls that contract with value; the
//!    contract — whose code block 2 only READS and never modifies —
//!    forwards the ETH to the codeless watched EOA `W`. This MUST fail
//!    under the old `CacheDB<EmptyDB>` reconstruction (block 2's own bundle
//!    has no code for the deployed contract → the call is a 21000-gas
//!    no-op → residual) and pass against the real parent-state provider
//!    with in-chain block-1 writes layered on top.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_consensus::{Block, BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_signer_local::PrivateKeySigner;

use reth_ethereum::chainspec::ChainSpec;
use reth_ethereum::evm::primitives::execute::Executor;
use reth_ethereum::evm::primitives::ConfigureEvm;
use reth_ethereum::evm::EthEvmConfig;
use reth_ethereum::primitives::RecoveredBlock;
use reth_ethereum::provider::{Chain, ExecutionOutcome};
use reth_ethereum::storage::StateProviderBox;
use reth_ethereum::TransactionSigned;
use reth_exex_test_utils::test_exex_context_with_chain_spec;

use passbook_core::config::PassbookConfig;
use passbook_core::ledger::Ledger;
use passbook_core::stack::StackAdapter;

/// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC0: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];

/// L1-style adapter: no OP L1 data fee, no system credits.
struct L1Adapter;
impl StackAdapter for L1Adapter {
    fn l1_data_fee_wei(&self, _tx_index: usize) -> Option<U256> {
        None
    }
}

fn addr32(a: Address) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[12..].copy_from_slice(a.as_slice());
    b
}

/// SELFDESTRUCT-FORWARDER: forward its entire received balance to `dst`.
/// `73<dst:20> FF` = PUSH20 dst ; SELFDESTRUCT. Reliably fires
/// `Inspector::selfdestruct` regardless of `dst` having code.
fn selfdestruct_forwarder_code(dst: Address) -> Bytes {
    let mut c = vec![0x73];
    c.extend_from_slice(dst.as_slice());
    c.push(0xff);
    Bytes::from(c)
}

/// CALL-FORWARDER runtime: forward the entire received balance to `dst`
/// via a plain value `CALL` (`dst` is a codeless EOA). Stack for CALL (top
/// first): gas, addr, value, argsOff, argsLen, retOff, retLen.
///   PUSH1 0 (retLen);PUSH1 0 (retOff);PUSH1 0 (argLen);PUSH1 0 (argOff);
///   SELFBALANCE (value);PUSH20 dst (addr);GAS (gas);CALL;STOP
fn call_forwarder_code(dst: Address) -> Bytes {
    let mut c = Vec::new();
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 retLen
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 retOff
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 argsLen
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 argsOff
    c.push(0x47); // SELFBALANCE -> value
    c.push(0x73); // PUSH20 dst
    c.extend_from_slice(dst.as_slice());
    c.push(0x5a); // GAS
    c.push(0xf1); // CALL
    c.push(0x00); // STOP
    Bytes::from(c)
}

/// REVERT-AFTER-FORWARD runtime (issue #2 fixture): forward the entire
/// received balance to `dst` via a plain value `CALL`, then `REVERT`. revm
/// rolls back the whole frame's state on the REVERT, so neither the
/// inbound value to this contract NOR the `dst` credit ever commit — the
/// captured `*->dst` value frame is a reverted-subtree frame that MUST NOT
/// be summed into reconciliation.
///   PUSH1 0 (retLen);PUSH1 0 (retOff);PUSH1 0 (argLen);PUSH1 0 (argOff);
///   SELFBALANCE (value);PUSH20 dst (addr);GAS (gas);CALL;
///   PUSH1 0;PUSH1 0;REVERT
fn revert_after_forward_code(dst: Address) -> Bytes {
    let mut c = Vec::new();
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 retLen
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 retOff
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 argsLen
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 argsOff
    c.push(0x47); // SELFBALANCE -> value
    c.push(0x73); // PUSH20 dst
    c.extend_from_slice(dst.as_slice());
    c.push(0x5a); // GAS
    c.push(0xf1); // CALL
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 (revert len)
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 (revert off)
    c.push(0xfd); // REVERT
    Bytes::from(c)
}

/// Plain unconditional REVERT runtime: `PUSH1 0;PUSH1 0;REVERT`. Any value
/// sent to a tx targeting this contract is rolled back (only gas is
/// charged) — the top-level value frame is a reverted frame.
fn always_revert_code() -> Bytes {
    Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd])
}

/// CREATE init code that returns `runtime` as the deployed contract's
/// code: copy `runtime` into memory then `RETURN`. Prologue:
///   PUSH2 len(3) ; PUSH1 off(2) ; PUSH1 0(2) ; CODECOPY(1) ;
///   PUSH2 len(3) ; PUSH1 0(2) ; RETURN(1)  = 14 bytes ⇒ off = 0x0e
/// followed by `<runtime...>`.
fn deploy_initcode(runtime: &Bytes) -> Bytes {
    const PROLOGUE_LEN: u8 = 14;
    let len = runtime.len() as u16;
    let mut c = Vec::new();
    c.push(0x61); // PUSH2 len
    c.extend_from_slice(&len.to_be_bytes());
    c.extend_from_slice(&[0x60, PROLOGUE_LEN]); // PUSH1 off (runtime offset)
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 (dest)
    c.push(0x39); // CODECOPY
    c.push(0x61); // PUSH2 len
    c.extend_from_slice(&len.to_be_bytes());
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0
    c.push(0xf3); // RETURN
    debug_assert_eq!(
        c.len(),
        PROLOGUE_LEN as usize,
        "initcode prologue length must match the runtime offset"
    );
    c.extend_from_slice(runtime);
    Bytes::from(c)
}

#[allow(clippy::too_many_arguments)]
fn sign_legacy(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: TxKind,
    value: U256,
    gas_limit: u64,
    gas_price: u128,
    input: Bytes,
) -> TransactionSigned {
    let mut tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price,
        gas_limit,
        to,
        value,
        input,
    };
    let sig = signer.sign_transaction_sync(&mut tx).expect("sign");
    TransactionSigned::from(tx.into_signed(sig))
}

/// Paris+Shanghai genesis config (Shanghai @0 so PUSH0/SELFBALANCE in our
/// hand-written bytecode are valid).
fn base_chain_config(chain_id: u64) -> ChainConfig {
    ChainConfig {
        chain_id,
        homestead_block: Some(0),
        eip150_block: Some(0),
        eip155_block: Some(0),
        eip158_block: Some(0),
        byzantium_block: Some(0),
        constantinople_block: Some(0),
        petersburg_block: Some(0),
        istanbul_block: Some(0),
        berlin_block: Some(0),
        london_block: Some(0),
        terminal_total_difficulty: Some(U256::ZERO),
        terminal_total_difficulty_passed: true,
        shanghai_time: Some(0),
        ..Default::default()
    }
}

fn make_genesis(
    chain_id: u64,
    alloc: std::collections::BTreeMap<Address, GenesisAccount>,
) -> Genesis {
    Genesis {
        config: base_chain_config(chain_id),
        nonce: 0,
        timestamp: 0,
        extra_data: Bytes::new(),
        gas_limit: 30_000_000,
        difficulty: U256::ZERO,
        mix_hash: B256::ZERO,
        coinbase: Address::ZERO,
        alloc,
        base_fee_per_gas: Some(7),
        number: Some(0),
        ..Default::default()
    }
}

fn build_block(
    chain_spec: &Arc<ChainSpec>,
    number: u64,
    parent_hash: B256,
    timestamp: u64,
    txs: Vec<TransactionSigned>,
) -> RecoveredBlock<Block<TransactionSigned>> {
    build_block_with_beneficiary(
        chain_spec,
        number,
        parent_hash,
        timestamp,
        Address::ZERO,
        txs,
    )
}

fn build_block_with_beneficiary(
    chain_spec: &Arc<ChainSpec>,
    number: u64,
    parent_hash: B256,
    timestamp: u64,
    beneficiary: Address,
    txs: Vec<TransactionSigned>,
) -> RecoveredBlock<Block<TransactionSigned>> {
    let genesis_header = chain_spec.genesis_header().clone();
    let header = Header {
        parent_hash,
        number,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(7),
        timestamp,
        beneficiary,
        difficulty: U256::ZERO,
        mix_hash: B256::ZERO,
        gas_used: 0,
        withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
        ..genesis_header
    };
    let body = BlockBody {
        transactions: txs,
        ommers: vec![],
        withdrawals: Some(Default::default()),
    };
    RecoveredBlock::try_recover(Block::new(header, body)).expect("recover senders")
}

fn acct(balance: U256, code: Option<Bytes>) -> GenesisAccount {
    GenesisAccount {
        balance,
        code,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn erc20_internal_gas_capture_zero_residual() {
    // ── Actors ──────────────────────────────────────────────────────────
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address(); // S
    let watched = w_signer.address(); // W (codeless EOA)
    let sdfwd = Address::repeat_byte(0xF0); // SELFDESTRUCT forwarder
    let cfwd = Address::repeat_byte(0xCF); // plain-CALL forwarder
    let token = Address::repeat_byte(0x70);
    let sink = Address::repeat_byte(0x51);

    let chain_id = 0x1234u64;
    let gas_price = 1_000_000_000u128; // 1 gwei
    let erc20_amount = U256::from(123_456_789u64);
    let sd_value = U256::from(7_000_000_000_000_000u64); // 0.007 ETH → W via SELFDESTRUCT
    let call_value = U256::from(3_000_000_000_000_000u64); // 0.003 ETH → W via CALL
    let w_send_value = U256::from(1_000_000_000_000_000u64); // 0.001 ETH W → SINK

    let funded = U256::from(10u64).pow(U256::from(18u64)); // 1 ETH
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    alloc.insert(
        sdfwd,
        acct(U256::ZERO, Some(selfdestruct_forwarder_code(watched))),
    );
    alloc.insert(cfwd, acct(U256::ZERO, Some(call_forwarder_code(watched))));
    alloc.insert(
        token,
        acct(U256::ZERO, Some(token_code(sender, watched, erc20_amount))),
    );
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // ── Build the block's transactions ─────────────────────────────────
    let gas_limit = 200_000u64;
    let tx0 = sign_legacy(
        &s_signer,
        chain_id,
        0,
        TxKind::Call(token),
        U256::ZERO,
        gas_limit,
        gas_price,
        Bytes::new(),
    );
    let tx1 = sign_legacy(
        &s_signer,
        chain_id,
        1,
        TxKind::Call(sdfwd),
        sd_value,
        gas_limit,
        gas_price,
        Bytes::new(),
    );
    let tx2 = sign_legacy(
        &s_signer,
        chain_id,
        2,
        TxKind::Call(cfwd),
        call_value,
        gas_limit,
        gas_price,
        Bytes::new(),
    );
    let tx3 = sign_legacy(
        &w_signer,
        chain_id,
        0,
        TxKind::Call(sink),
        w_send_value,
        gas_limit,
        gas_price,
        Bytes::new(),
    );

    let recovered = build_block(
        &chain_spec,
        1,
        chain_spec.genesis_hash(),
        12,
        vec![tx0, tx1, tx2, tx3],
    );

    // ── Genuinely execute the block to obtain a consistent outcome ──────
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let state_db = genesis_state(&chain_spec);
    let exec_out = evm_config
        .executor(state_db)
        .execute(&recovered)
        .expect("block execution");
    let outcome = ExecutionOutcome::single(1, exec_out);
    let chain = Chain::new(
        vec![recovered.clone()],
        outcome,
        std::collections::BTreeMap::new(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // ── Harness first: we need a REAL parent (= genesis) state provider
    //    for the deterministic direct check below. ──────────────────────
    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let genesis_hash = handle.genesis.hash();

    // ── Deterministic core check: run the per-block pipeline directly
    //    against the real genesis-state provider so a residual / re-exec
    //    failure surfaces as a concrete error. ──────────────────────────
    let parent_state: StateProviderBox = handle
        .provider_factory
        .history_by_block_hash(genesis_hash)
        .expect("genesis state provider");
    let res = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        &chain,
        &recovered,
        &cfg,
        &L1Adapter,
        parent_state,
    );
    let batch = res.expect("per-block processing must reconcile to zero residual");
    assert!(
        batch.unattributed.is_empty(),
        "zero unattributed residual expected from the pure orchestrator"
    );
    assert_eq!(
        batch.erc20.iter().filter(|r| r.address == watched).count(),
        1,
        "one ERC20 row for W"
    );
    let internal_in: Vec<&_> = batch
        .eth
        .iter()
        .filter(|r| {
            r.address == watched
                && matches!(r.direction, passbook_core::model::Direction::In)
                && matches!(r.kind, passbook_core::model::EthKind::Internal)
        })
        .collect();
    assert_eq!(
        internal_in.len(),
        2,
        "two internal inbound ETH rows for W (SELFDESTRUCT + plain CALL)"
    );
    let mut got: Vec<U256> = internal_in.iter().map(|r| r.amount_wei).collect();
    got.sort();
    let mut want = [sd_value, call_value];
    want.sort();
    assert_eq!(
        got, want,
        "internal-in amounts = SELFDESTRUCT + CALL values"
    );
    assert_eq!(
        batch.gas.iter().filter(|r| r.address == watched).count(),
        1,
        "one gas row for W (tx3)"
    );

    // ── End-to-end: drive run_passbook via the test ExEx harness ───────
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg,
        ledger.clone(),
        || L1Adapter,
    ));

    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");

    wait_finished_height(&mut handle, 1).await;
    driver.abort();

    // ── Assert the durable ledger ──────────────────────────────────────
    let g = ledger.lock().unwrap();
    let conn = g.conn();
    let w_lc = format!("{watched:#x}");

    let erc20_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM erc20_transfers WHERE address=?1 AND direction='in'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(erc20_count, 1, "expected one inbound ERC20 row for W");

    let erc20_amt: String = conn
        .query_row(
            "SELECT amount FROM erc20_transfers WHERE address=?1 AND direction='in'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(erc20_amt, erc20_amount.to_string());

    let internal_in: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM eth_transfers \
             WHERE address=?1 AND direction='in' AND kind='internal'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        internal_in, 2,
        "expected two internal inbound ETH rows for W (SELFDESTRUCT + CALL)"
    );

    let internal_sum: String = conn
        .query_row(
            "SELECT CAST(SUM(CAST(amount_wei AS INTEGER)) AS TEXT) FROM eth_transfers \
             WHERE address=?1 AND direction='in' AND kind='internal'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(internal_sum, (sd_value + call_value).to_string());

    let gas_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gas_payments WHERE address=?1",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(gas_rows, 1, "expected one gas_payments row for W (tx3)");

    let unattributed: i64 = conn
        .query_row("SELECT COUNT(*) FROM unattributed_deltas", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unattributed, 0, "ZERO unattributed residual expected");
}

#[tokio::test(flavor = "multi_thread")]
async fn parent_state_readonly_two_block_chain_zero_residual() {
    // Block 1 DEPLOYS a CALL-forwarder via CREATE (its runtime code lives
    // only in block 1's BundleState — NOT genesis). Block 2 calls that
    // contract with value; block 2 only READS the deployed code (never
    // modifies it). The forwarder sends the ETH to the codeless watched
    // EOA `W`. Old EmptyDB reconstruction (block 2's own bundle has no
    // code for the deployed contract) ⇒ 21000-gas no-op ⇒ residual. The
    // real parent-state provider + in-chain block-1 overlay ⇒ zero.
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address(); // W (codeless EOA)

    let chain_id = 0x4321u64;
    let gas_price = 1_000_000_000u128;
    let fwd_value = U256::from(5_000_000_000_000_000u64); // 0.005 ETH → W

    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // Deployed contract address = CREATE(sender, nonce=0).
    let deployed = sender.create(0);
    let runtime = call_forwarder_code(watched);
    let initcode = deploy_initcode(&runtime);

    let gas_limit = 300_000u64;
    // Block 1: deploy the forwarder (CREATE).
    let b1_tx = sign_legacy(
        &s_signer,
        chain_id,
        0,
        TxKind::Create,
        U256::ZERO,
        gas_limit,
        gas_price,
        initcode,
    );
    let b1 = build_block(&chain_spec, 1, chain_spec.genesis_hash(), 12, vec![b1_tx]);

    // Block 2: call the deployed forwarder with value (its code is read,
    // not modified, by block 2).
    let b2_tx = sign_legacy(
        &s_signer,
        chain_id,
        1,
        TxKind::Call(deployed),
        fwd_value,
        gas_limit,
        gas_price,
        Bytes::new(),
    );
    let b2 = build_block(&chain_spec, 2, b1.hash(), 24, vec![b2_tx]);

    // Genuinely execute both blocks against the same evolving state so the
    // committed Chain's BundleState/receipts are consistent by
    // construction.
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let state_db = genesis_state(&chain_spec);
    let outcome = evm_config
        .batch_executor(state_db)
        .execute_batch([&b1, &b2])
        .expect("2-block batch execution");
    let chain = Chain::new(
        vec![b1.clone(), b2.clone()],
        outcome,
        std::collections::BTreeMap::new(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let genesis_hash = handle.genesis.hash();

    // Deterministic direct check for BLOCK 2 specifically: its parent is
    // block 1 (in-chain). The chain's parent is genesis; block 2 must
    // re-exec against (genesis provider + block 1 writes). Block 1's
    // BundleState carries the deployed runtime code; block 2 reads it.
    let parent_state: StateProviderBox = handle
        .provider_factory
        .history_by_block_hash(genesis_hash)
        .expect("genesis state provider");
    let batch2 = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        &chain,
        &b2,
        &cfg,
        &L1Adapter,
        parent_state,
    )
    .expect("block 2 must reconcile against parent-state + in-chain overlay");
    assert!(
        batch2.unattributed.is_empty(),
        "zero residual for block 2 (read-only parent/in-chain state)"
    );
    let b2_internal_in: Vec<&_> = batch2
        .eth
        .iter()
        .filter(|r| {
            r.address == watched
                && matches!(r.direction, passbook_core::model::Direction::In)
                && matches!(r.kind, passbook_core::model::EthKind::Internal)
        })
        .collect();
    assert_eq!(
        b2_internal_in.len(),
        1,
        "block 2: one internal inbound ETH row for W via the block-1-deployed forwarder"
    );
    assert_eq!(b2_internal_in[0].amount_wei, fwd_value);

    // End-to-end: the whole 2-block chain through run_passbook.
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg,
        ledger.clone(),
        || L1Adapter,
    ));
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    wait_finished_height(&mut handle, 2).await;
    driver.abort();

    let g = ledger.lock().unwrap();
    let conn = g.conn();
    let w_lc = format!("{watched:#x}");

    let internal_in: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM eth_transfers \
             WHERE address=?1 AND direction='in' AND kind='internal'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        internal_in, 1,
        "expected one internal inbound ETH row for W (block 2 via block-1 contract)"
    );
    let internal_amt: String = conn
        .query_row(
            "SELECT amount_wei FROM eth_transfers \
             WHERE address=?1 AND direction='in' AND kind='internal'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(internal_amt, fwd_value.to_string());

    let unattributed: i64 = conn
        .query_row("SELECT COUNT(*) FROM unattributed_deltas", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unattributed, 0, "ZERO unattributed residual expected");
}

// ── Task 6.5 — fault-stall, reorg-replace, restart-resume ───────────────

/// Drive `run_passbook` through the test ExEx harness against a custom
/// chain spec. Returns the spawned driver task + the handle so the test can
/// send notifications and observe events / the ledger.
async fn spawn_driver(
    chain_spec: &Arc<ChainSpec>,
    cfg: &PassbookConfig,
    ledger: &Arc<Mutex<Ledger>>,
) -> (
    tokio::task::JoinHandle<eyre::Result<()>>,
    reth_exex_test_utils::TestExExHandle,
) {
    let (mut ctx, handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg.clone(),
        ledger.clone(),
        || L1Adapter,
    ));
    (driver, handle)
}

/// Build a block carrying a non-empty beacon **withdrawals** list (one
/// withdrawal to `wd_recipient` for `wd_gwei` GWEI) AND `beneficiary` set,
/// then genuinely execute it so the committed `Chain`'s `BundleState`
/// reflects BOTH the withdrawal credit and the beneficiary priority-fee
/// credit (post-Shanghai consensus applies withdrawals to state).
// test fixture builder: each arg is an independent block/tx input knob
#[allow(clippy::too_many_arguments)]
fn withdrawal_and_beneficiary_block(
    chain_spec: &Arc<ChainSpec>,
    number: u64,
    parent_hash: B256,
    timestamp: u64,
    beneficiary: Address,
    wd_recipient: Address,
    wd_gwei: u64,
    s_signer: &PrivateKeySigner,
    s_nonce: u64,
    chain_id: u64,
) -> (RecoveredBlock<Block<TransactionSigned>>, Chain) {
    use alloy_eips::eip4895::{Withdrawal, Withdrawals};
    // A plain value transfer S -> sink. The 21000-gas tx pays a priority
    // fee (gas_price 1 gwei vs base_fee 7 wei) to the beneficiary.
    let sink = Address::repeat_byte(0x51);
    let tx = sign_legacy(
        s_signer,
        chain_id,
        s_nonce,
        TxKind::Call(sink),
        U256::from(1u64),
        100_000,
        1_000_000_000u128,
        Bytes::new(),
    );
    let genesis_header = chain_spec.genesis_header().clone();
    let header = Header {
        parent_hash,
        number,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(7),
        timestamp,
        beneficiary,
        difficulty: U256::ZERO,
        mix_hash: B256::ZERO,
        gas_used: 0,
        withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
        ..genesis_header
    };
    let withdrawals = Withdrawals(vec![Withdrawal {
        index: 0,
        validator_index: 7,
        address: wd_recipient,
        amount: wd_gwei,
    }]);
    let body = BlockBody {
        transactions: vec![tx],
        ommers: vec![],
        withdrawals: Some(withdrawals),
    };
    let recovered = RecoveredBlock::try_recover(Block::new(header, body)).expect("recover senders");
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let state_db = genesis_state(chain_spec);
    let exec_out = evm_config
        .executor(state_db)
        .execute(&recovered)
        .expect("block execution");
    let outcome = ExecutionOutcome::single(number, exec_out);
    let chain = Chain::new(
        vec![recovered.clone()],
        outcome,
        std::collections::BTreeMap::new(),
    );
    (recovered, chain)
}

/// A test `ChainExec` that runs the **real, unchanged** L1 per-block
/// pipeline (`process_committed_block_inner`) and then injects a single
/// SYNTHETIC, genuinely-unexplained balance discrepancy for `victim` into
/// the resulting batch via the pure reconciler — i.e. an observed delta
/// with **no captured CALL/SELFDESTRUCT/CREATE frame, no gas, and NOT any
/// recognised system category** (not a beacon withdrawal, not the
/// beneficiary priority-fee block reward, not an OP deposit mint / fee
/// vault). This is precisely the spec's "TRULY unexplained delta ⇒
/// processing-failure/stall" case, constructed so it can never be
/// explained away by the new recognition.
#[derive(Clone, Copy)]
struct UnexplainedInjector {
    victim: Address,
    phantom_wei: i128,
}

impl passbook_core::exex::ChainExec for UnexplainedInjector {
    type Primitives = reth_ethereum::EthPrimitives;
    type ChainSpec = ChainSpec;

    fn process_committed_block(
        &self,
        chain_id: u64,
        chain_spec: Arc<ChainSpec>,
        chain: &Chain,
        block: &RecoveredBlock<reth_ethereum::Block>,
        cfg: &PassbookConfig,
        parent_state: StateProviderBox,
    ) -> Result<passbook_core::ledger::writer::BlockBatch, passbook_core::exex::ProcessingError>
    {
        use alloy_consensus::BlockHeader;
        // Drive the REAL pipeline first (proves the genuine path; its
        // own recognition nets every real system credit to zero).
        let _real = passbook_core::exex::process_committed_block_inner(
            chain_id,
            chain_spec.clone(),
            chain,
            block,
            cfg,
            &L1Adapter,
            parent_state,
        )?;
        // Now feed the pure orchestrator a synthetic balance discrepancy
        // for `victim` with NO explaining flow of ANY kind: no frame, no
        // gas, no system credit. Reconciliation MUST treat this as a
        // TRULY unexplained residual ⇒ ProcessingError::UnexplainedResidual
        // ⇒ the loop stalls (this is the spec's processing-failure case).
        passbook_core::exex::process_block(passbook_core::exex::BlockInputs {
            chain_id,
            block_number: block.header().number(),
            block_hash: block.hash(),
            watched: [self.victim].into_iter().collect(),
            erc20_logs: vec![],
            frames: vec![],
            gas: vec![],
            account_deltas: vec![(self.victim, self.phantom_wei)],
            system_signed: vec![],
        })
    }
}

/// 1. A TRULY unexplained reconciliation residual MUST halt indexing: the
///    loop never emits `FinishedHeight`, never advances `meta.last_block`,
///    writes the diagnostic `unattributed_deltas` row, and keeps retrying
///    (the task is still alive — it did not return or panic).
///
///    REWORKED for B1: the previous fixture relied on a coinbase
///    priority-fee credit to `W`, which is now a *recognised* `kind=system`
///    block-reward (zero residual, no stall) — so it can no longer prove
///    "stall on unexplained". Instead we inject a SYNTHETIC balance
///    discrepancy with no captured flow and no withdrawal/coinbase/
///    deposit/vault explanation (see [`UnexplainedInjector`]) — the exact
///    spec "TRULY unexplained delta" case. All strong assertions are kept.
#[tokio::test(flavor = "multi_thread")]
async fn fault_injected_residual_stalls_without_advancing() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address();

    let chain_id = 0xFA17u64;
    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(U256::ZERO, None));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // A perfectly ordinary, fully-explainable block (S -> sink). The
    // residual comes ONLY from the injected synthetic discrepancy, so the
    // test proves the *truly unexplained* property, not a fixture quirk.
    let tx = sign_legacy(
        &s_signer,
        chain_id,
        0,
        TxKind::Call(Address::repeat_byte(0x51)),
        U256::from(1u64),
        100_000,
        1_000_000_000u128,
        Bytes::new(),
    );
    let recovered = build_block(&chain_spec, 1, chain_spec.genesis_hash(), 12, vec![tx]);
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let exec_out = evm_config
        .executor(genesis_state(&chain_spec))
        .execute(&recovered)
        .expect("block execution");
    let outcome = ExecutionOutcome::single(1, exec_out);
    let chain = Chain::new(
        vec![recovered.clone()],
        outcome,
        std::collections::BTreeMap::new(),
    );

    let phantom_wei = 4_242_424_242_424_242i128;
    let injector = UnexplainedInjector {
        victim: watched,
        phantom_wei,
    };

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // Deterministic core check: the injector pipeline against the real
    // genesis provider MUST surface an UnexplainedResidual for W (proves
    // the scenario genuinely yields a residual, not just "no error").
    let (mut ctx, handle0) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let parent_state: StateProviderBox = handle0
        .provider_factory
        .history_by_block_hash(handle0.genesis.hash())
        .expect("genesis state provider");
    let direct = passbook_core::exex::ChainExec::process_committed_block(
        &injector,
        chain_id,
        chain_spec.clone(),
        &chain,
        &recovered,
        &cfg,
        parent_state,
    );
    match direct {
        Err(passbook_core::exex::ProcessingError::UnexplainedResidual { address, .. }) => {
            assert_eq!(address, watched, "residual must be for W")
        }
        other => panic!(
            "expected an UnexplainedResidual for the synthetic, truly \
             unexplained balance discrepancy for W, got {other:?}"
        ),
    }
    drop(ctx);
    drop(handle0);

    // End-to-end: drive run_passbook and prove it STALLS.
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg.clone(),
        ledger.clone(),
        injector,
    ));
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");

    // Wait long enough for several retry iterations (BACKOFF_START=200ms),
    // then assert the loop did NOT advance.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // (a) NO FinishedHeight was ever emitted.
    let mut emitted_finished = false;
    while let Ok(ev) = handle.events_rx.try_recv() {
        if matches!(ev, reth_ethereum::exex::ExExEvent::FinishedHeight(_)) {
            emitted_finished = true;
        }
    }
    assert!(
        !emitted_finished,
        "run_passbook MUST NOT emit FinishedHeight for an unreconciled block"
    );

    // (b) the diagnostic unattributed_deltas row exists for block 1 / W.
    {
        let g = ledger.lock().unwrap();
        let w_lc = format!("{watched:#x}");
        let n: i64 = g
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM unattributed_deltas \
                 WHERE block_number=1 AND address=?1",
                [&w_lc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 1,
            "expected the diagnostic unattributed_deltas row for the stalled block"
        );

        // (c) meta.last_block was NOT advanced to 1 (no successful block
        //     write happened — it is either absent or < 1).
        let last: Option<String> = g
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .ok();
        assert!(
            last.is_none() || last.as_deref() != Some("1"),
            "meta.last_block must NOT advance to the unreconciled block (got {last:?})"
        );

        // No durable transfer/gas rows for the stalled block.
        for table in ["eth_transfers", "erc20_transfers", "gas_payments"] {
            let c: i64 = g
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE block_number=1"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(c, 0, "no {table} rows for the stalled block 1");
        }
    }

    // (d) the loop is still alive / retrying — it did not return or panic.
    assert!(
        !driver.is_finished(),
        "run_passbook must keep retrying the unreconciled block, not exit"
    );

    driver.abort();
}

/// B1 PROOF: the exact scenario that previously STALLED now SUCCEEDS.
///
/// A single watched address `W` is BOTH (a) a beacon-withdrawal recipient
/// AND (b) the block `beneficiary` receiving the txs' priority fees — two
/// real post-state balance credits with NO captured CALL frame. Pre-fix
/// (B1) this produced a permanent reconciliation-residual stall. Post-fix
/// both are RECOGNISED `kind=system` credits: the block processes to
/// completion (`FinishedHeight` emitted, `meta.last_block` advances),
/// `unattributed_deltas` is EMPTY (zero residual), and the ledger has the
/// `kind=system` `eth_transfers` rows (one `withdrawal`, one
/// `block_reward`).
#[tokio::test(flavor = "multi_thread")]
async fn l1_withdrawal_and_beneficiary_priority_fee_recognized_zero_residual() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address(); // W = beneficiary AND withdrawal recipient

    let chain_id = 0xB100u64;
    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    // W starts at zero: its ONLY balance changes this block are the
    // (uncaptured) withdrawal credit + the (uncaptured) priority fee.
    alloc.insert(watched, acct(U256::ZERO, None));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    let wd_gwei = 32_000_000_000u64; // 32 ETH worth of GWEI
    let (recovered, chain) = withdrawal_and_beneficiary_block(
        &chain_spec,
        1,
        chain_spec.genesis_hash(),
        12,
        watched, // beneficiary
        watched, // withdrawal recipient
        wd_gwei,
        &s_signer,
        0,
        chain_id,
    );

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // Deterministic core check: the REAL pipeline against the real genesis
    // provider MUST reconcile to ZERO (no residual) — the precise B1 fix.
    let (mut ctx, handle0) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let parent_state: StateProviderBox = handle0
        .provider_factory
        .history_by_block_hash(handle0.genesis.hash())
        .expect("genesis state provider");
    let batch = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        &chain,
        &recovered,
        &cfg,
        &L1Adapter,
        parent_state,
    )
    .expect("withdrawal + beneficiary priority fee MUST now reconcile to zero");
    assert!(
        batch.unattributed.is_empty(),
        "ZERO residual expected — both credits are recognised system events"
    );
    let sys_rows: Vec<&_> = batch
        .eth
        .iter()
        .filter(|r| r.address == watched && matches!(r.kind, passbook_core::model::EthKind::System))
        .collect();
    assert_eq!(
        sys_rows.len(),
        2,
        "expected two kind=system rows (withdrawal + block_reward)"
    );
    assert!(
        sys_rows
            .iter()
            .any(|r| r.trace_path.starts_with("system:withdrawal:")),
        "a system:withdrawal row must be present"
    );
    assert!(
        sys_rows
            .iter()
            .any(|r| r.trace_path.starts_with("system:block_reward:")),
        "a system:block_reward row must be present"
    );
    drop(ctx);
    drop(handle0);

    // End-to-end: the block must process to COMPLETION (no stall).
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let (driver, mut handle) = spawn_driver(&chain_spec, &cfg, &ledger).await;
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    wait_finished_height(&mut handle, 1).await; // would hang pre-fix
    driver.abort();

    let g = ledger.lock().unwrap();
    let conn = g.conn();
    let w_lc = format!("{watched:#x}");

    let unattributed: i64 = conn
        .query_row("SELECT COUNT(*) FROM unattributed_deltas", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unattributed, 0, "ZERO unattributed residual expected (B1)");

    let sys_in: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM eth_transfers \
             WHERE address=?1 AND kind='system'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        sys_in, 2,
        "two durable kind=system rows for W (withdrawal + block_reward)"
    );

    let last: String = conn
        .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(last, "1", "meta.last_block advanced — block completed");
}

/// 2. A reorg replaces rows: block at hash A (watched activity → rows,
///    FinishedHeight A) then a reorg reverting A and committing an
///    ALTERNATE block at hash B (same height, different watched activity).
///    A's rows MUST be gone, B's present, no duplicates, meta consistent.
#[tokio::test(flavor = "multi_thread")]
async fn reorg_replaces_rows_no_dup() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address(); // codeless EOA

    let chain_id = 0x9001u64;

    let gas_price = 1_000_000_000u128;
    let amt_a = U256::from(4_000_000_000_000_000u64); // chain A → W
    let amt_b = U256::from(6_000_000_000_000_000u64); // chain B → W (differs)

    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    // SELFDESTRUCT forwarder → W (reliable internal inbound credit).
    let fwd = Address::repeat_byte(0xF0);
    alloc.insert(
        fwd,
        acct(U256::ZERO, Some(selfdestruct_forwarder_code(watched))),
    );
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // Build chain A: block 1 (parent = genesis, a PERSISTED block in the
    // harness provider) — S -> fwd with value, forwarded to W.
    let build = |nonce: u64, value: U256, ts: u64| {
        let tx = sign_legacy(
            &s_signer,
            chain_id,
            nonce,
            TxKind::Call(fwd),
            value,
            200_000,
            gas_price,
            Bytes::new(),
        );
        let recovered = build_block(&chain_spec, 1, chain_spec.genesis_hash(), ts, vec![tx]);
        let evm_config = EthEvmConfig::new(chain_spec.clone());
        let state_db = genesis_state(&chain_spec);
        let exec_out = evm_config
            .executor(state_db)
            .execute(&recovered)
            .expect("block execution");
        let outcome = ExecutionOutcome::single(1, exec_out);
        let chain = Chain::new(
            vec![recovered.clone()],
            outcome,
            std::collections::BTreeMap::new(),
        );
        (recovered, chain)
    };
    // Distinct timestamps ⇒ distinct block hashes A != B.
    let (block_a, chain_a) = build(0, amt_a, 12);
    let (block_b, chain_b) = build(0, amt_b, 24);
    let hash_a = block_a.hash();
    let hash_b = block_b.hash();
    assert_ne!(hash_a, hash_b, "A and B must be distinct block hashes");

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let (driver, mut handle) = spawn_driver(&chain_spec, &cfg, &ledger).await;

    // Commit chain A.
    handle
        .send_notification_chain_committed(chain_a.clone())
        .await
        .expect("send committed A");
    wait_finished_height(&mut handle, 1).await;

    let w_lc = format!("{watched:#x}");
    let a_hex = format!("{hash_a:#x}");
    let b_hex = format!("{hash_b:#x}");

    // A's rows are present.
    {
        let g = ledger.lock().unwrap();
        let a_rows: i64 = g
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM eth_transfers WHERE block_hash=?1",
                [&a_hex],
                |r| r.get(0),
            )
            .unwrap();
        assert!(a_rows >= 1, "chain A must have written eth rows");
        let a_amt: String = g
            .conn()
            .query_row(
                "SELECT amount_wei FROM eth_transfers \
                 WHERE block_hash=?1 AND address=?2 AND direction='in' AND kind='internal'",
                rusqlite::params![&a_hex, &w_lc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_amt, amt_a.to_string(), "A's inbound amount");
    }

    // Reorg: revert A, commit B at the same height.
    handle
        .send_notification_chain_reorged(chain_a.clone(), chain_b.clone())
        .await
        .expect("send reorg A->B");
    wait_finished_height(&mut handle, 1).await;

    {
        let g = ledger.lock().unwrap();

        // All rows keyed to A are GONE (reorg-first delete_blocks).
        for table in [
            "eth_transfers",
            "erc20_transfers",
            "gas_payments",
            "unattributed_deltas",
        ] {
            let a_n: i64 = g
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE block_hash=?1"),
                    [&a_hex],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(a_n, 0, "reverted chain A rows must be gone from {table}");
        }

        // B's rows are present.
        let b_in: i64 = g
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM eth_transfers \
                 WHERE block_hash=?1 AND address=?2 AND direction='in' AND kind='internal'",
                rusqlite::params![&b_hex, &w_lc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b_in, 1, "chain B's inbound internal row must be present");
        let b_amt: String = g
            .conn()
            .query_row(
                "SELECT amount_wei FROM eth_transfers \
                 WHERE block_hash=?1 AND address=?2 AND direction='in' AND kind='internal'",
                rusqlite::params![&b_hex, &w_lc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            b_amt,
            amt_b.to_string(),
            "B's inbound amount differs from A's"
        );

        // No duplicates: exactly one inbound internal eth row total (only B).
        let total_in: i64 = g
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM eth_transfers \
                 WHERE address=?1 AND direction='in' AND kind='internal'",
                [&w_lc],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            total_in, 1,
            "exactly one inbound internal eth row after reorg (no A/B dup)"
        );

        // meta.last_block consistent at height 1.
        let last: String = g
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            last, "1",
            "meta.last_block consistent at the reorged height"
        );
    }

    driver.abort();
}

/// 3. Restart safety: process a block through a TEMP-FILE ledger, drop the
///    loop/ledger, reopen `Ledger::open` on the SAME db path, re-deliver the
///    LAST committed notification to a fresh `run_passbook`. Row counts and
///    `last_block` MUST be unchanged (idempotent INSERT OR REPLACE on
///    natural PKs) — no duplicates, no gap.
#[tokio::test(flavor = "multi_thread")]
async fn restart_resumes_no_gap_no_dup() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address();

    let chain_id = 0x5237u64;
    let gas_price = 1_000_000_000u128;
    let amt = U256::from(2_500_000_000_000_000u64);

    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    let fwd = Address::repeat_byte(0xF0);
    alloc.insert(
        fwd,
        acct(U256::ZERO, Some(selfdestruct_forwarder_code(watched))),
    );
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    let tx = sign_legacy(
        &s_signer,
        chain_id,
        0,
        TxKind::Call(fwd),
        amt,
        200_000,
        gas_price,
        Bytes::new(),
    );
    let recovered = build_block(&chain_spec, 1, chain_spec.genesis_hash(), 12, vec![tx]);
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let exec_out = evm_config
        .executor(genesis_state(&chain_spec))
        .execute(&recovered)
        .expect("block execution");
    let outcome = ExecutionOutcome::single(1, exec_out);
    let chain = Chain::new(
        vec![recovered.clone()],
        outcome,
        std::collections::BTreeMap::new(),
    );

    // Persistent temp-FILE db (NOT in-memory) so it survives the restart.
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // ── Run 1: process block 1 through run_passbook, then drop everything.
    let (n_eth, n_erc20, n_gas, n_unattr, last_block_1) = {
        let ledger = Arc::new(Mutex::new(
            Ledger::open(&db_path, chain_id).expect("ledger open 1"),
        ));
        let (driver, mut handle) = spawn_driver(&chain_spec, &cfg, &ledger).await;
        handle
            .send_notification_chain_committed(chain.clone())
            .await
            .expect("send committed run1");
        wait_finished_height(&mut handle, 1).await;
        driver.abort();
        let _ = driver.await;

        let g = ledger.lock().unwrap();
        let count = |t: &str| -> i64 {
            g.conn()
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        };
        let last: String = g
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        let snap = (
            count("eth_transfers"),
            count("erc20_transfers"),
            count("gas_payments"),
            count("unattributed_deltas"),
            last,
        );
        // ledger (and its only connection) dropped here.
        drop(g);
        snap
    };
    assert!(n_eth >= 1, "run 1 must have written at least one eth row");
    assert_eq!(last_block_1, "1", "run 1 advanced last_block to 1");

    // ── Run 2: reopen the SAME db file, re-deliver the LAST committed
    //    notification to a FRESH run_passbook. Must be idempotent.
    {
        let ledger = Arc::new(Mutex::new(
            Ledger::open(&db_path, chain_id).expect("ledger reopen 2"),
        ));
        // Sanity: the reopened ledger already has run 1's durable state.
        {
            let g = ledger.lock().unwrap();
            let e: i64 = g
                .conn()
                .query_row("SELECT COUNT(*) FROM eth_transfers", [], |r| r.get(0))
                .unwrap();
            assert_eq!(e, n_eth, "reopened ledger retains run 1's eth rows");
        }
        let (driver, mut handle) = spawn_driver(&chain_spec, &cfg, &ledger).await;
        handle
            .send_notification_chain_committed(chain.clone())
            .await
            .expect("re-deliver committed run2");
        wait_finished_height(&mut handle, 1).await;
        driver.abort();
        let _ = driver.await;

        let g = ledger.lock().unwrap();
        let count = |t: &str| -> i64 {
            g.conn()
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            count("eth_transfers"),
            n_eth,
            "no duplicate eth rows after restart"
        );
        assert_eq!(
            count("erc20_transfers"),
            n_erc20,
            "no duplicate erc20 rows after restart"
        );
        assert_eq!(
            count("gas_payments"),
            n_gas,
            "no duplicate gas rows after restart"
        );
        assert_eq!(
            count("unattributed_deltas"),
            n_unattr,
            "no duplicate unattributed rows after restart"
        );
        let last: String = g
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            last, "1",
            "last_block unchanged (no gap, no regression) after restart"
        );
    }
}

/// TOKEN bytecode: emit `LOG3(Transfer, from, to)` with `amount` as data.
fn token_code(from: Address, to: Address, amount: U256) -> Bytes {
    let mut c = Vec::new();
    c.push(0x7f);
    c.extend_from_slice(&amount.to_be_bytes::<32>());
    c.extend_from_slice(&[0x5f, 0x52]); // PUSH0 ; MSTORE
    c.push(0x7f);
    c.extend_from_slice(&addr32(to));
    c.push(0x7f);
    c.extend_from_slice(&addr32(from));
    c.push(0x7f);
    c.extend_from_slice(&TRANSFER_TOPIC0);
    c.extend_from_slice(&[0x60, 0x20, 0x5f, 0xa3, 0x00]); // PUSH1 0x20;PUSH0;LOG3;STOP
    Bytes::from(c)
}

async fn wait_finished_height(handle: &mut reth_exex_test_utils::TestExExHandle, n: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        if let Ok(ev) = handle.events_rx.try_recv() {
            if matches!(
                ev,
                reth_ethereum::exex::ExExEvent::FinishedHeight(h) if h.number == n
            ) {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for run_passbook to emit FinishedHeight({n})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// In-memory genesis-allocation state, used only to *produce* a consistent
/// synthetic committed `Chain` by genuinely executing the blocks (the
/// pipeline under test re-executes against the harness's real parent-state
/// provider, not this).
fn genesis_state(
    chain_spec: &Arc<ChainSpec>,
) -> revm::database::State<revm::database::CacheDB<revm::database::EmptyDB>> {
    use revm::database::{CacheDB, EmptyDB};
    use revm::state::AccountInfo;
    let mut cache: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    for (addr, acct) in chain_spec.genesis().alloc.iter() {
        let code = acct.code.clone().map(revm::bytecode::Bytecode::new_raw);
        let mut info = AccountInfo {
            balance: acct.balance,
            nonce: acct.nonce.unwrap_or(0),
            ..Default::default()
        };
        if let Some(bc) = code {
            info.code_hash = bc.hash_slow();
            info.code = Some(bc);
        }
        cache.insert_account_info(*addr, info);
        if let Some(storage) = &acct.storage {
            for (k, v) in storage {
                cache
                    .insert_account_storage(
                        *addr,
                        U256::from_be_bytes(k.0),
                        U256::from_be_bytes(v.0),
                    )
                    .ok();
            }
        }
    }
    revm::database::State::builder()
        .with_database(cache)
        .with_bundle_update()
        .build()
}

/// Issue #2 (C1) regression: reverted value movements MUST NOT be summed
/// into reconciliation, otherwise an entirely valid block stalls forever.
///
/// One genuinely-executed block with two reverted-value scenarios for the
/// watched address `W`:
///
///   * tx0: `W -> RVRT` w/ value — the target contract unconditionally
///     `REVERT`s. Only gas is actually deducted from `W`; the value never
///     leaves `W`. The captured top-level `W -> RVRT` value frame is a
///     reverted frame and must be excluded from `eth_out`.
///   * tx1: `S -> TRY` w/ value — `TRY` forwards the value to inner `R`
///     via `CALL`; `R` forwards the whole balance to the codeless watched
///     EOA `W` and then `REVERT`s. `TRY` ignores the failed sub-call and
///     `STOP`s, so the *transaction succeeds* while the inner `R -> W`
///     value transfer is rolled back. The captured internal `R -> W` frame
///     is a reverted-subtree frame inside a SUCCESSFUL tx and must be
///     excluded from `eth_in` (the per-tx revert flag alone is `false`
///     here — only per-frame revert tracking catches it).
///
/// Before the fix both reverted frames were summed in, producing a nonzero
/// residual for `W` ⇒ `process_committed_block_inner` would return
/// `UnexplainedResidual` (a permanent false stall). After the fix the
/// residual is zero, no spurious `eth_transfers` rows for `W` are emitted,
/// and only the genuine gas row for tx0 (W is the sender) remains.
#[tokio::test(flavor = "multi_thread")]
async fn reverted_value_transfers_zero_residual() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address(); // S
    let watched = w_signer.address(); // W (codeless EOA)
    let rvrt = Address::repeat_byte(0xD1); // unconditional REVERT contract
    let r_inner = Address::repeat_byte(0xD2); // forward-to-W-then-REVERT
    let try_c = Address::repeat_byte(0xD3); // forwards to R, ignores failure

    let chain_id = 0x2222u64;
    let gas_price = 1_000_000_000u128;
    let tx0_value = U256::from(5_000_000_000_000_000u64); // W -> RVRT (reverts)
    let tx1_value = U256::from(2_000_000_000_000_000u64); // S -> TRY -> R -> W (R reverts)

    let funded = U256::from(10u64).pow(U256::from(18u64)); // 1 ETH each
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    alloc.insert(rvrt, acct(U256::ZERO, Some(always_revert_code())));
    alloc.insert(
        r_inner,
        acct(U256::ZERO, Some(revert_after_forward_code(watched))),
    );
    alloc.insert(try_c, acct(U256::ZERO, Some(call_forwarder_code(r_inner))));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    let gas_limit = 200_000u64;
    // tx0: watched W sends value to a contract that always reverts.
    let tx0 = sign_legacy(
        &w_signer,
        chain_id,
        0,
        TxKind::Call(rvrt),
        tx0_value,
        gas_limit,
        gas_price,
        Bytes::new(),
    );
    // tx1: S sends value to TRY; TRY -> R; R -> W then REVERT; TRY STOPs.
    let tx1 = sign_legacy(
        &s_signer,
        chain_id,
        0,
        TxKind::Call(try_c),
        tx1_value,
        gas_limit,
        gas_price,
        Bytes::new(),
    );

    let recovered = build_block(
        &chain_spec,
        1,
        chain_spec.genesis_hash(),
        12,
        vec![tx0, tx1],
    );

    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let state_db = genesis_state(&chain_spec);
    let exec_out = evm_config
        .executor(state_db)
        .execute(&recovered)
        .expect("block execution");
    let outcome = ExecutionOutcome::single(1, exec_out);
    let chain = Chain::new(
        vec![recovered.clone()],
        outcome,
        std::collections::BTreeMap::new(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let genesis_hash = handle.genesis.hash();
    let parent_state: StateProviderBox = handle
        .provider_factory
        .history_by_block_hash(genesis_hash)
        .expect("genesis state provider");

    let res = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        &chain,
        &recovered,
        &cfg,
        &L1Adapter,
        parent_state,
    );
    let batch = res.expect(
        "reverted value transfers must reconcile to ZERO residual (issue #2): \
         a reverted top-level tx and a reverted internal CALL must not stall a valid block",
    );
    assert!(
        batch.unattributed.is_empty(),
        "no unattributed residual expected — reverted frames excluded"
    );

    // No inbound/outbound eth_transfers rows should be COUNTED for W, and
    // crucially no spurious counted movement at all: any emitted row for W
    // must carry reverted == true (audit trail) and contribute nothing.
    let w_eth: Vec<&_> = batch.eth.iter().filter(|r| r.address == watched).collect();
    for r in &w_eth {
        assert!(
            r.reverted,
            "any eth_transfers row for W here is from a reverted movement"
        );
    }

    // The only genuine watched movement is gas: W is the sender of tx0
    // (gas is charged even on a reverted tx).
    let w_gas: Vec<&_> = batch.gas.iter().filter(|r| r.address == watched).collect();
    assert_eq!(w_gas.len(), 1, "exactly one gas row for W (tx0 sender)");

    // End-to-end through run_passbook: the driver must advance (emit
    // FinishedHeight) — i.e. NOT stall on this valid block.
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg,
        ledger.clone(),
        || L1Adapter,
    ));
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    wait_finished_height(&mut handle, 1).await;
    driver.abort();

    let g = ledger.lock().unwrap();
    let conn = g.conn();
    let w_lc = format!("{watched:#x}");
    let unattributed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM unattributed_deltas WHERE address = ?1",
            [&w_lc],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        unattributed, 0,
        "no unattributed_deltas row for W — block reconciled, no false stall"
    );
}
