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
    let (batch_erc20_len, batch_internal_in_amounts, batch_gas_len) = {
        let gps = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
            Ok(handle.provider_factory.history_by_block_hash(genesis_hash)?)
        };
        let batch = passbook_core::exex::process_committed_block_inner(
            chain_id,
            chain_spec.clone(),
            &chain,
            &recovered,
            &cfg,
            &L1Adapter,
            &gps,
        )
        .expect("per-block processing must reconcile to zero residual");
        assert!(
            batch.unattributed.is_empty(),
            "zero unattributed residual expected from the pure orchestrator"
        );
        let erc20_len = batch.erc20.iter().filter(|r| r.address == watched).count();
        let internal_in: Vec<U256> = batch
            .eth
            .iter()
            .filter(|r| {
                r.address == watched
                    && matches!(r.direction, passbook_core::model::Direction::In)
                    && matches!(r.kind, passbook_core::model::EthKind::Internal)
            })
            .map(|r| r.amount_wei)
            .collect();
        let gas_len = batch.gas.iter().filter(|r| r.address == watched).count();
        (erc20_len, internal_in, gas_len)
    };
    assert_eq!(
        batch_erc20_len, 1,
        "one ERC20 row for W"
    );
    let mut got = batch_internal_in_amounts;
    assert_eq!(
        got.len(),
        2,
        "two internal inbound ETH rows for W (SELFDESTRUCT + plain CALL)"
    );
    got.sort();
    let mut want = [sd_value, call_value];
    want.sort();
    assert_eq!(
        got, want,
        "internal-in amounts = SELFDESTRUCT + CALL values"
    );
    assert_eq!(
        batch_gas_len, 1,
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

    let chain_tip_hash = recovered.hash();
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    flush_lag(&handle, &chain_spec, chain_tip_hash, 1).await;

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
    let (b2_internal_in_len, b2_internal_in_amount) = {
        let gps = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
            Ok(handle.provider_factory.history_by_block_hash(genesis_hash)?)
        };
        let batch2 = passbook_core::exex::process_committed_block_inner(
            chain_id,
            chain_spec.clone(),
            &chain,
            &b2,
            &cfg,
            &L1Adapter,
            &gps,
        )
        .expect("block 2 must reconcile against parent-state + in-chain overlay");
        assert!(
            batch2.unattributed.is_empty(),
            "zero residual for block 2 (read-only parent/in-chain state)"
        );
        let b2_internal_in: Vec<_> = batch2
            .eth
            .iter()
            .filter(|r| {
                r.address == watched
                    && matches!(r.direction, passbook_core::model::Direction::In)
                    && matches!(r.kind, passbook_core::model::EthKind::Internal)
            })
            .collect();
        (b2_internal_in.len(), b2_internal_in.first().map(|r| r.amount_wei))
    };
    assert_eq!(
        b2_internal_in_len,
        1,
        "block 2: one internal inbound ETH row for W via the block-1-deployed forwarder"
    );
    assert_eq!(b2_internal_in_amount, Some(fwd_value));

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
    let b2_hash = b2.hash();
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    flush_lag(&handle, &chain_spec, b2_hash, 2).await;
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
        get_parent_state: &passbook_core::exex::ParentStateFn<'_>,
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
            get_parent_state,
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
    let direct = {
        let genesis_hash0 = handle0.genesis.hash();
        let gps = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
            Ok(handle0.provider_factory.history_by_block_hash(genesis_hash0)?)
        };
        passbook_core::exex::ChainExec::process_committed_block(
            &injector,
            chain_id,
            chain_spec.clone(),
            &chain,
            &recovered,
            &cfg,
            &gps,
        )
    };
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
    let batch = {
        let handle0_genesis_hash = handle0.genesis.hash();
        let gps = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
            Ok(handle0.provider_factory.history_by_block_hash(handle0_genesis_hash)?)
        };
        passbook_core::exex::process_committed_block_inner(
            chain_id,
            chain_spec.clone(),
            &chain,
            &recovered,
            &cfg,
            &L1Adapter,
            &gps,
        )
        .expect("withdrawal + beneficiary priority fee MUST now reconcile to zero")
    };
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
    let recovered_hash = recovered.hash();
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    flush_lag(&handle, &chain_spec, recovered_hash, 1).await;
    wait_finished_height(&mut handle, 1).await; // would hang pre-fix without lag flush
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
    // The lag flush (block 2) is also written durably; last_block reflects both blocks.
    assert!(
        last == "1" || last == "2",
        "meta.last_block advanced — block completed (got {last})"
    );
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
    flush_lag(&handle, &chain_spec, hash_a, 1).await;
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
///    LAST committed notification to a fresh `run_passbook`. On restart,
///    `set_notifications_with_head` configures the resume head from the ledger;
///    the re-delivered already-durable notification is FILTERED by the
///    `ExExNotificationsWithHead` stream (never re-processed). No-dup/no-gap
///    is proven via stream filtering: no `FinishedHeight` is emitted and row
///    counts + `last_block` are unchanged.
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
        flush_lag(&handle, &chain_spec, recovered.hash(), 1).await;
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
    // The lag flush (block 2) is also written; last_block reflects both.
    assert_eq!(last_block_1, "2", "run 1 advanced last_block through the lag-flush block");

    // ── Run 2: reopen the SAME db file and start a fresh run_passbook.
    //    With Task 10, `set_notifications_with_head` is called with the
    //    ledger's high-water mark (block 2). The ExEx stream skips any
    //    notification whose committed tip is <= the resume head, so
    //    re-delivering old blocks is both unnecessary and a no-op. The
    //    durability assertion is made directly from the ledger — the key
    //    invariant is that durable state is UNCHANGED (idempotent on
    //    restart), not that the ExEx re-processes the blocks.
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
        // Run 2 proves: on restart, `set_notifications_with_head` configures
        // `ExExNotificationsWithHead` with the resume head (block 2, the
        // ledger high-water mark). When we re-deliver the block-1 committed
        // notification (tip number = 1 <= 2 = resume head), the stream MUST
        // filter it — `run_passbook` never processes it and never emits
        // `FinishedHeight`. The unchanged post-abort row counts confirm no
        // duplicate rows were written and `last_block` was not bumped.
        let (driver, mut handle) = spawn_driver(&chain_spec, &cfg, &ledger).await;

        // Re-deliver the already-processed block-1 notification. Its committed
        // tip number is 1, which is <= 2 (the resume head stored in the ledger),
        // so `ExExNotificationsWithHead::poll_next` must filter it.
        handle
            .send_notification_chain_committed(chain.clone())
            .await
            .expect("re-deliver committed run2");

        // Assert the notification was filtered: NO FinishedHeight must be
        // emitted within the timeout window. We replicate the single-recv
        // from wait_finished_height (same `handle.events_rx` channel) but
        // wrap it in a timeout and assert Err (elapsed = no event arrived).
        // Filtering in `ExExNotificationsWithHead::poll_next` is synchronous
        // (no I/O); yield so the spawned driver is scheduled and has actually
        // polled the re-delivered notification before we assert no event —
        // otherwise a never-scheduled driver could make this pass vacuously.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // `handle.events_rx` carries `ExExEvent`, whose only variant is
        // `FinishedHeight`; any successful recv here means filtering failed.
        let filtered = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            handle.events_rx.recv(),
        )
        .await;
        assert!(
            filtered.is_err(),
            "ExExNotificationsWithHead must filter the re-delivered block-1 \
             notification (tip=1 <= resume_head=2) — no FinishedHeight expected, \
             but one was emitted: {filtered:?}"
        );

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
        // The lag flush (block 2) is written idempotently in both runs;
        // last_block reflects blocks 1 and 2 — unchanged between runs.
        assert_eq!(
            last, last_block_1,
            "last_block unchanged (no gap, no regression) after restart"
        );
    }
}

/// Issue #7 (I3): a node that has captured a block into a temp-FILE ledger
/// for one chain, then restarts pointed at the SAME db path but with a
/// DIFFERENT `--chain`, must fail `Ledger::open` loudly instead of silently
/// mixing chains. Mirrors the restart-safety scenario but flips the chain id
/// on the reopen and asserts the abort + that durable state is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn restart_with_different_chain_id_aborts() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address();

    let chain_id = 0x5237u64;
    let other_chain_id = 0x9999u64;
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

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // ── Run 1: capture block 1 into the ledger bound to `chain_id`.
    let n_eth = {
        let ledger = Arc::new(Mutex::new(
            Ledger::open(&db_path, chain_id).expect("ledger open 1"),
        ));
        let (driver, mut handle) = spawn_driver(&chain_spec, &cfg, &ledger).await;
        handle
            .send_notification_chain_committed(chain.clone())
            .await
            .expect("send committed run1");
        flush_lag(&handle, &chain_spec, recovered.hash(), 1).await;
        wait_finished_height(&mut handle, 1).await;
        driver.abort();
        let _ = driver.await;

        let g = ledger.lock().unwrap();
        let e: i64 = g
            .conn()
            .query_row("SELECT COUNT(*) FROM eth_transfers", [], |r| r.get(0))
            .unwrap();
        e
    };
    assert!(n_eth >= 1, "run 1 must have written at least one eth row");

    // ── Run 2: reopen the SAME db file with a DIFFERENT chain id. This must
    //    be a hard error — not a silent mixed-chain ledger.
    let err_msg = match Ledger::open(&db_path, other_chain_id) {
        Ok(_) => panic!("reopening with a mismatched chain id must fail"),
        Err(e) => e.to_string(),
    };
    assert!(
        err_msg.contains("chain-id mismatch"),
        "unexpected error: {err_msg}"
    );

    // Durable state is untouched: reopening with the ORIGINAL chain id still
    // works and retains run 1's rows.
    let ledger = Ledger::open(&db_path, chain_id).expect("reopen with original chain id");
    let e: i64 = ledger
        .conn()
        .query_row("SELECT COUNT(*) FROM eth_transfers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(e, n_eth, "rejected reopen left durable state intact");
    let stored: String = ledger
        .conn()
        .query_row("SELECT v FROM meta WHERE k='chain_id'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, chain_id.to_string(), "stored chain_id unchanged");
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

/// Wait until `run_passbook` emits `FinishedHeight(h)` with `h.number >= n`.
///
/// The one-notification lag (Task 7) means `FinishedHeight(N)` is not
/// emitted immediately after the notification with tip = N; it is emitted
/// when the NEXT notification arrives. Tests call `flush_lag` before this
/// helper to trigger the lag release, which may produce a `FinishedHeight`
/// at height N or N+1 (the flush block). Accepting `h.number >= n` lets
/// us use either the exact tip or the flush block's height as the signal
/// that tip N is durably committed (the write always precedes the lag emit).
async fn wait_finished_height(handle: &mut reth_exex_test_utils::TestExExHandle, n: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        if let Ok(ev) = handle.events_rx.try_recv() {
            if matches!(
                ev,
                reth_ethereum::exex::ExExEvent::FinishedHeight(h) if h.number >= n
            ) {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for run_passbook to emit FinishedHeight(>={n})"
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

/// Send a minimal empty committed block to flush the one-notification
/// `FinishedHeight` lag introduced in Task 7. `run_passbook` holds back
/// `FinishedHeight(N)` until the NEXT notification arrives; this helper
/// supplies that next notification (an empty block at height `tip + 1` with
/// no watched activity) so tests can `wait_finished_height(&mut handle, n)`
/// immediately after sending the real notification + this flush.
///
/// The empty block is genuinely executed against the genesis state (no txs,
/// no watched-account changes, no re-execution), so the pipeline processes
/// it in the fast path and emits `FinishedHeight(tip)` as expected.
async fn flush_lag(
    handle: &reth_exex_test_utils::TestExExHandle,
    chain_spec: &Arc<ChainSpec>,
    tip_hash: B256,
    tip_number: u64,
) {
    let flush_block = build_block(chain_spec, tip_number + 1, tip_hash, tip_number * 12 + 1000, vec![]);
    let evm_config = reth_ethereum::evm::EthEvmConfig::new(chain_spec.clone());
    let exec_out = evm_config
        .executor(genesis_state(chain_spec))
        .execute(&flush_block)
        .expect("empty flush block execution");
    let outcome = reth_ethereum::provider::ExecutionOutcome::single(tip_number + 1, exec_out);
    let flush_chain = reth_ethereum::provider::Chain::new(
        vec![flush_block],
        outcome,
        std::collections::BTreeMap::new(),
    );
    handle
        .send_notification_chain_committed(flush_chain)
        .await
        .expect("send flush notification");
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
    {
        let gps = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
            Ok(handle.provider_factory.history_by_block_hash(genesis_hash)?)
        };
        let batch = passbook_core::exex::process_committed_block_inner(
            chain_id,
            chain_spec.clone(),
            &chain,
            &recovered,
            &cfg,
            &L1Adapter,
            &gps,
        )
        .expect(
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
    }

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
    let recovered_hash = recovered.hash();
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    flush_lag(&handle, &chain_spec, recovered_hash, 1).await;
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

// ── Issue #3 (C2) — DB-write fault must STALL, not kill the ExEx task ────

/// C2 PROOF: a durable-write failure (disk-full / SQLITE_BUSY past the
/// busy_timeout / I/O error — modelled here by a `query_only` connection so
/// every `write_block` transaction fails identically and repeatably) MUST
/// make `run_passbook` STALL — retry forever with bounded backoff — never
/// `?` the error out of the loop and terminate indexing.
///
/// The committed block is perfectly ordinary and fully reconciled (the real
/// `|| L1Adapter` pipeline, ZERO residual): the ONLY thing that can fail is
/// the durable `write_block`. Pre-fix the `write_block(...)?` propagated and
/// `run_passbook` returned (the explicit anti-requirement). Post-fix the
/// loop stalls: no `FinishedHeight`, `meta.last_block` never advances, the
/// driver task is still alive (mirrors
/// `fault_injected_residual_stalls_without_advancing`).
#[tokio::test(flavor = "multi_thread")]
async fn fault_injected_db_write_error_stalls_without_advancing() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address();

    let chain_id = 0xDB17u64;
    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(U256::ZERO, None));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // A perfectly ordinary, fully-explainable block (S -> sink): the real
    // L1 pipeline reconciles it to ZERO residual, so the only possible
    // failure in run_passbook is the durable write itself.
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

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // Open the ledger, then INJECT the DB fault: `query_only=ON` makes
    // every subsequent write (the whole `write_block` transaction) fail
    // with "attempt to write a readonly database" — a faithful stand-in
    // for disk-full / persistent SQLITE_BUSY / I/O error.
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    ledger
        .lock()
        .unwrap()
        .conn()
        .pragma_update(None, "query_only", "ON")
        .expect("inject query_only fault");

    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg.clone(),
        ledger.clone(),
        || L1Adapter,
    ));
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");

    // Several retry iterations (BACKOFF_START=200ms) must elapse with the
    // loop stalling — NOT exiting.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // (a) NO FinishedHeight was ever emitted (the block is not durable).
    let mut emitted_finished = false;
    while let Ok(ev) = handle.events_rx.try_recv() {
        if matches!(ev, reth_ethereum::exex::ExExEvent::FinishedHeight(_)) {
            emitted_finished = true;
        }
    }
    assert!(
        !emitted_finished,
        "run_passbook MUST NOT emit FinishedHeight when the durable write failed"
    );

    // (b) the driver is still alive / retrying — it did NOT `?` the DB
    //     error out of the loop and return (the C2 anti-requirement).
    assert!(
        !driver.is_finished(),
        "run_passbook must STALL on a DB-write error, not exit the task"
    );

    // (c) nothing was durably written: meta.last_block never advanced and
    //     no transfer/gas rows exist for the block. Clear query_only so we
    //     can read (read-only stays consistent; we only relax to assert).
    {
        let g = ledger.lock().unwrap();
        g.conn()
            .pragma_update(None, "query_only", "OFF")
            .expect("clear query_only");
        let last: Option<String> = g
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .ok();
        assert!(
            last.is_none() || last.as_deref() != Some("1"),
            "meta.last_block must NOT advance when the write failed (got {last:?})"
        );
        for table in ["eth_transfers", "erc20_transfers", "gas_payments"] {
            let c: i64 = g
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE block_number=1"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(c, 0, "no {table} rows for the un-written stalled block 1");
        }
    }

    driver.abort();
}

// ── Issue #8 (I4) — a POISONED ledger mutex must STALL/recover, not crash ──

/// I4 PROOF: if some OTHER holder of the shared `Arc<Mutex<Ledger>>` (e.g. an
/// RPC reader — rpc.rs — or any task) panics while holding the lock, the
/// mutex becomes POISONED. Pre-fix the writer's `ledger.lock().unwrap()`
/// panicked on the poison, terminating the ExEx task: indexing permanently
/// dead with no retry/stall (violating spec lines 211-213) and an RPC
/// failure thereby killing block processing (violating spec line 216).
///
/// Post-fix the writer recovers the poisoned guard (`into_inner`) and the
/// durable write proceeds normally. The block is perfectly ordinary and
/// fully reconciled (the real `|| L1Adapter` pipeline, ZERO residual): with
/// the poison recovered the writer MUST durably write it and emit
/// `FinishedHeight` exactly as if the mutex had never been poisoned — the
/// RPC-side panic is fully isolated from the writer.
#[tokio::test(flavor = "multi_thread")]
async fn poisoned_ledger_mutex_does_not_crash_writer() {
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address();

    let chain_id = 0x8417u64;
    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(U256::ZERO, None));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // A perfectly ordinary, fully-explainable block (S -> sink): the real
    // L1 pipeline reconciles it to ZERO residual, so the ONLY thing under
    // test is whether a poisoned mutex crashes the writer.
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

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));

    // POISON the shared mutex BEFORE the writer ever runs: a panic in
    // another thread while it holds the lock (faithful stand-in for a
    // panicking RPC reader, rpc.rs, the issue #8 scenario).
    let l2 = ledger.clone();
    let _ = std::thread::spawn(move || {
        let _g = l2.lock().unwrap();
        panic!("RPC reader panicked while holding the ledger lock");
    })
    .join();
    assert!(
        ledger.lock().is_err(),
        "precondition: the shared ledger mutex is poisoned"
    );

    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg.clone(),
        ledger.clone(),
        || L1Adapter,
    ));
    let recovered_hash = recovered.hash();
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    // Flush the one-notification lag so FinishedHeight(1) is emitted within
    // the sleep window below (the lag holds it until the next notification).
    flush_lag(&handle, &chain_spec, recovered_hash, 1).await;

    // Give the (poison-recovered) writer time to durably write + advance.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // (a) the driver did NOT panic/exit on the poisoned mutex (pre-fix:
    //     `lock().unwrap()` panicked, the task died here).
    assert!(
        !driver.is_finished(),
        "run_passbook must NOT crash on a poisoned ledger mutex (#8)"
    );

    // (b) FinishedHeight WAS emitted — the poison was recovered and the
    //     block durably written, so the writer advances normally. The
    //     RPC-side panic is fully isolated from block processing
    //     (spec line 216).
    let mut emitted_finished = false;
    while let Ok(ev) = handle.events_rx.try_recv() {
        if matches!(ev, reth_ethereum::exex::ExExEvent::FinishedHeight(_)) {
            emitted_finished = true;
        }
    }
    assert!(
        emitted_finished,
        "writer must recover the poisoned mutex and emit FinishedHeight (#8)"
    );

    // (c) the block was actually durably written despite the poison.
    {
        let g = ledger.lock().unwrap_or_else(|e| e.into_inner());
        let last: Option<String> = g
            .conn()
            .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
            .ok();
        // The lag flush (block 2) may also be written; last_block is ≥ 1.
        assert!(
            matches!(last.as_deref(), Some("1") | Some("2")),
            "meta.last_block must advance to the durably-written block (got {last:?})"
        );
    }

    driver.abort();
}

/// Issue #4 (C3): a single block where BOTH parties of an ERC20 transfer
/// AND both parties of a native ETH transfer are watched.
///
/// Actors: W1 and W2 are two watched EOAs.
///  - tx0: W1 → TOKEN. TOKEN emits `Transfer(W1, W2, erc20_amount)`. Both
///    W1 and W2 are watched ⇒ `decode_transfer` returns
///    `[(W2,In),(W1,Out)]` ⇒ `process_block` pushes TWO `erc20` rows
///    sharing `(chain_id, block_hash, tx_hash, log_index)`, distinct only
///    in `address`/`direction`. Pre-fix the v1 PK (omitting `address`)
///    made `INSERT OR REPLACE` destroy the inbound row; both must now
///    persist.
///  - tx1: W1 → W2 with `eth_value` (native, watched→watched). The
///    analogous ETH case is safe (distinct `:out` trace_path suffix) but
///    must be covered too: exactly one in row for W2 and one out row for
///    W1, both surviving.
///
/// The whole block must still reconcile to ZERO unattributed residual
/// (ERC20 moves no ETH; the only ETH movement is the W1→W2 transfer plus
/// W1's gas).
#[tokio::test(flavor = "multi_thread")]
async fn watched_to_watched_erc20_and_eth_no_row_loss_zero_residual() {
    let w1_signer = PrivateKeySigner::random();
    let w2_signer = PrivateKeySigner::random();
    let w1 = w1_signer.address(); // watched sender
    let w2 = w2_signer.address(); // watched recipient
    let token = Address::repeat_byte(0x70);

    let chain_id = 0x1234u64;
    let gas_price = 1_000_000_000u128; // 1 gwei
    let erc20_amount = U256::from(555_000_111u64);
    let eth_value = U256::from(2_000_000_000_000_000u64); // 0.002 ETH W1 → W2

    let funded = U256::from(10u64).pow(U256::from(18u64)); // 1 ETH
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(w1, acct(funded, None));
    alloc.insert(w2, acct(funded, None));
    // TOKEN emits Transfer(W1 -> W2) — both watched.
    alloc.insert(
        token,
        acct(U256::ZERO, Some(token_code(w1, w2, erc20_amount))),
    );
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    let gas_limit = 200_000u64;
    // tx0: W1 -> TOKEN ⇒ ERC20 Transfer(W1, W2, amount) log.
    let tx0 = sign_legacy(
        &w1_signer,
        chain_id,
        0,
        TxKind::Call(token),
        U256::ZERO,
        gas_limit,
        gas_price,
        Bytes::new(),
    );
    // tx1: W1 -> W2 native value (watched → watched ETH).
    let tx1 = sign_legacy(
        &w1_signer,
        chain_id,
        1,
        TxKind::Call(w2),
        eth_value,
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
    let cfg = PassbookConfig::from_parts(
        vec![format!("{w1:#x}"), format!("{w2:#x}")],
        db_path.clone(),
    )
    .expect("cfg");

    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let genesis_hash = handle.genesis.hash();

    // Deterministic core check against the real genesis-state provider.
    {
        let gps = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
            Ok(handle.provider_factory.history_by_block_hash(genesis_hash)?)
        };
        let batch = passbook_core::exex::process_committed_block_inner(
            chain_id,
            chain_spec.clone(),
            &chain,
            &recovered,
            &cfg,
            &L1Adapter,
            &gps,
        )
        .expect("per-block processing must reconcile to zero residual");
        assert!(
            batch.unattributed.is_empty(),
            "zero unattributed residual expected"
        );
        // BOTH directional ERC20 rows for the single watched→watched log.
        let erc20_in: Vec<&_> = batch
            .erc20
            .iter()
            .filter(|r| r.address == w2 && matches!(r.direction, passbook_core::model::Direction::In))
            .collect();
        let erc20_out: Vec<&_> = batch
            .erc20
            .iter()
            .filter(|r| r.address == w1 && matches!(r.direction, passbook_core::model::Direction::Out))
            .collect();
        assert_eq!(erc20_in.len(), 1, "one inbound ERC20 row for W2");
        assert_eq!(erc20_out.len(), 1, "one outbound ERC20 row for W1");
        assert_eq!(erc20_in[0].amount, erc20_amount);
        assert_eq!(erc20_out[0].amount, erc20_amount);
        // The two rows share the collision PK columns but differ in address.
        assert_eq!(erc20_in[0].tx_hash, erc20_out[0].tx_hash);
        assert_eq!(erc20_in[0].log_index, erc20_out[0].log_index);
        assert_ne!(erc20_in[0].address, erc20_out[0].address);
    }

    // ── End-to-end through run_passbook + the durable ledger ───────────
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg,
        ledger.clone(),
        || L1Adapter,
    ));
    let recovered_hash = recovered.hash();
    handle
        .send_notification_chain_committed(chain)
        .await
        .expect("send committed");
    flush_lag(&handle, &chain_spec, recovered_hash, 1).await;
    wait_finished_height(&mut handle, 1).await;
    driver.abort();

    let g = ledger.lock().unwrap();
    let conn = g.conn();
    let w1_lc = format!("{w1:#x}");
    let w2_lc = format!("{w2:#x}");

    // The crux: BOTH directional ERC20 rows survived the durable write.
    let erc20_total: i64 = conn
        .query_row("SELECT COUNT(*) FROM erc20_transfers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        erc20_total, 2,
        "watched→watched ERC20 must keep BOTH rows (issue #4)"
    );
    let erc20_w2_in: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM erc20_transfers WHERE address=?1 AND direction='in'",
            [&w2_lc],
            |r| r.get(0),
        )
        .unwrap();
    let erc20_w1_out: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM erc20_transfers WHERE address=?1 AND direction='out'",
            [&w1_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        (erc20_w2_in, erc20_w1_out),
        (1, 1),
        "exactly one inbound (W2) and one outbound (W1) ERC20 row"
    );

    // ETH watched→watched: one out row for W1, one in row for W2.
    let eth_w1_out: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM eth_transfers WHERE address=?1 AND direction='out'",
            [&w1_lc],
            |r| r.get(0),
        )
        .unwrap();
    let eth_w2_in: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM eth_transfers WHERE address=?1 AND direction='in'",
            [&w2_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(eth_w1_out, 1, "one outbound ETH row for W1");
    assert_eq!(eth_w2_in, 1, "one inbound ETH row for W2");
    let eth_in_amt: String = conn
        .query_row(
            "SELECT amount_wei FROM eth_transfers WHERE address=?1 AND direction='in'",
            [&w2_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(eth_in_amt, eth_value.to_string());

    let unattributed: i64 = conn
        .query_row("SELECT COUNT(*) FROM unattributed_deltas", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unattributed, 0, "ZERO unattributed residual expected");
}

// ── DRY helper ──────────────────────────────────────────────────────────────

/// Build a `get_parent_state` thunk that calls
/// `handle.provider_factory.history_by_block_hash(hash)` each time it is
/// invoked. Eliminates the closure boilerplate in new tests; existing six sites
/// above are left unchanged (they predate this helper) to avoid any risk of
/// behaviour-altering rewrites in already-reviewed test code.
///
/// Note: `ProviderFactory<NodeTypesWithDBAdapter<TestNode, TmpDB>>` (the type
/// of `TestExExHandle::provider_factory`) has `history_by_block_hash` as an
/// INHERENT method but does NOT implement the `StateProviderFactory` trait, so
/// the helper takes the full `TestExExHandle` rather than a generic factory.
fn genesis_state_thunk(
    handle: &reth_exex_test_utils::TestExExHandle,
    hash: B256,
) -> impl Fn() -> eyre::Result<reth_ethereum::storage::StateProviderBox> + '_
{
    move || Ok(handle.provider_factory.history_by_block_hash(hash)?)
}

// ── Integration test: ParentStateUnavailable writes partial batch and marker ──

/// When get_parent_state() fails on a watched-account-change block,
/// the seam must:
///  - skip the inspector frame re-execution (no frame-derived
///    eth_transfers rows for the watched address);
///  - still capture erc20 transfers, gas payments, and recognised
///    system credits for the block;
///  - emit one unattributed_deltas marker per watched-changed address
///    recording the observed BundleState delta;
///  - return Ok(batch) — never stall, never error.
/// Mirrors the live-mode default exercised by every other integration
/// test (those use a working genesis_state_thunk and capture frames).
#[tokio::test(flavor = "multi_thread")]
async fn parent_state_unavailable_writes_partial_batch_and_marker() {
    // ── Actors ──────────────────────────────────────────────────────────
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address();
    let watched = w_signer.address(); // W (codeless EOA)

    // SELFDESTRUCT forwarder → W (produces an internal inbound ETH frame for
    // W, triggering `any_watched_changed = true` and thus the gated
    // `get_parent_state()` call — the exact path under test).
    let fwd = Address::repeat_byte(0xF0);

    let chain_id = 0xB4D1u64;
    let gas_price = 1_000_000_000u128;
    let fwd_value = U256::from(4_000_000_000_000_000u64); // 0.004 ETH → W via SELFDESTRUCT

    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    alloc.insert(
        fwd,
        acct(U256::ZERO, Some(selfdestruct_forwarder_code(watched))),
    );
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // ── Build and execute the block ──────────────────────────────────────
    let tx = sign_legacy(
        &s_signer,
        chain_id,
        0,
        TxKind::Call(fwd),
        fwd_value,
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

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg =
        PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone()).expect("cfg");

    // ── Failing thunk: every call returns Err (never recovers — the
    //    new design needs no retry; one call is enough).
    let fail_thunk = || -> eyre::Result<reth_ethereum::storage::StateProviderBox> {
        eyre::bail!("injected: parent state unavailable (simulating staged-backfill)")
    };

    // Single call — under the new design returns Ok(partial batch).
    let batch = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        &chain,
        &recovered,
        &cfg,
        &L1Adapter,
        &fail_thunk,
    )
    .expect("skip path must return Ok partial batch, never error");

    // Frames-derived eth_transfers rows for the watched address must
    // be ABSENT (we couldn't capture frames). kind=System rows for the
    // watched address may still appear if the fixture generates any —
    // count only non-System kinds.
    use passbook_core::model::EthKind;
    let frame_rows_for_watched: usize = batch
        .eth
        .iter()
        .filter(|r| r.address == watched && !matches!(r.kind, EthKind::System))
        .count();
    assert_eq!(
        frame_rows_for_watched, 0,
        "no frame-derived rows on skip"
    );

    // At least one unattributed marker for the watched-changed address.
    let markers_for_watched: Vec<_> = batch
        .unattributed
        .iter()
        .filter(|m| m.address == watched)
        .collect();
    assert!(
        !markers_for_watched.is_empty(),
        "skip path must emit ≥1 unattributed_deltas marker per watched-changed address"
    );
    assert_eq!(markers_for_watched[0].block_number, batch.block_number);
    assert_eq!(markers_for_watched[0].block_hash, batch.block_hash);
    assert_eq!(markers_for_watched[0].attributed_wei, alloy_primitives::U256::ZERO);
    assert!(
        markers_for_watched[0].residual_wei > alloy_primitives::U256::ZERO,
        "marker records non-zero residual = observed BundleState delta"
    );
}

/// Issue #14 (C4): when `run_passbook` detects a notification gap
/// (committed.first() > high_water + 1) and the provider cannot serve
/// the gap-block headers, the driver MUST NOT silently advance past
/// the gap. It retries the header fetch forever with bounded backoff;
/// the ledger high-water stays put, no FinishedHeight is emitted, and
/// no block_not_delivered markers nor block-5 rows are written.
///
/// This is the safety contract the fix exists to enforce: never silently
/// advance across an unprocessed block range. The marker-write happy
/// path is comprehensively covered by the Task 2 (writer.rs) and Task 3
/// (gap_range) unit tests.
///
/// Test setup (harness-aware):
///   * The `reth_exex_test_utils` harness only inserts the genesis block
///     into `provider_factory`. `ctx.provider().block_hash(n)` for any
///     n > 0 returns `Ok(None)` — there are no headers to fetch.
///   * We pre-populate the ledger's `meta.last_block = 1` directly via
///     SQL, simulating a prior crash after writing block 1.
///   * We send a committed notification for synthetic block 5; the
///     driver computes gap_range = 2..=4, attempts `block_hash(2)`,
///     receives `Ok(None)`, and enters the retry-forever loop.
///   * We wait ~3s and assert the ledger and event channel are
///     undisturbed.
#[tokio::test(flavor = "multi_thread")]
async fn gap_on_restart_stalls_when_provider_cannot_serve_gap_headers() {
    // ── Actors and chain spec ──────────────────────────────────────────
    let watched = Address::repeat_byte(0xCC);
    let chain_id = 0x1234u64;
    let funded = U256::from(10u64).pow(U256::from(18u64));
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(watched, acct(funded, None));
    let chain_spec: Arc<ChainSpec> =
        Arc::new(ChainSpec::from_genesis(make_genesis(chain_id, alloc)));

    // ── Ledger pre-populated with high-water = block 1 ────────────────
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("passbook.db");
    let cfg = PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone())
        .expect("cfg");
    let ledger = Arc::new(Mutex::new(
        Ledger::open(&db_path, chain_id).expect("ledger open"),
    ));
    let prior_hash = B256::repeat_byte(0xAA); // any non-genesis hash
    {
        let g = ledger.lock().unwrap();
        let conn = g.conn();
        conn.execute(
            "INSERT INTO meta(k,v) VALUES('last_block','1')
              ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta(k,v) VALUES('last_block_hash',?1)
              ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            [format!("{prior_hash:#x}")],
        )
        .unwrap();
    }

    // ── Harness ────────────────────────────────────────────────────────
    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    ctx.config.chain = chain_spec.clone();
    let genesis_hash = handle.genesis.hash();

    // ── Build a single synthetic block at height 5 (with parent set to
    //   the genesis hash; the contents are immaterial — the gap-fill
    //   stalls before this block is processed). ─────────────────────────
    let block_5 = build_block(&chain_spec, 5, genesis_hash, 60, vec![]);
    let evm_config = reth_ethereum::evm::EthEvmConfig::new(chain_spec.clone());
    let exec_out = evm_config
        .executor(genesis_state(&chain_spec))
        .execute(&block_5)
        .expect("empty block-5 execution");
    let outcome = reth_ethereum::provider::ExecutionOutcome::single(5, exec_out);
    let chain_5 = reth_ethereum::provider::Chain::new(
        vec![block_5],
        outcome,
        std::collections::BTreeMap::new(),
    );

    // ── Spawn run_passbook ─────────────────────────────────────────────
    let driver = tokio::spawn(passbook_core::exex::run_passbook(
        ctx,
        cfg,
        ledger.clone(),
        || L1Adapter,
    ));

    // ── Send the gap-creating notification ─────────────────────────────
    handle
        .send_notification_chain_committed(chain_5)
        .await
        .expect("send committed");

    // ── Give the driver time to enter the gap-fill loop and attempt
    //   the first block_hash(2) call. The 3s window gives the driver task
    //   ample time to be scheduled, attempt block_hash(2), get Ok(None),
    //   and re-enter the retry-with-backoff loop several times even on a
    //   loaded CI runner. ──────────────────────────────────────────────
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── Driver MUST still be alive (stalled in the gap-fill retry
    //   loop). If it has exited, all four assertions below pass
    //   vacuously and the test gives a false green. ────────────────────
    assert!(
        !driver.is_finished(),
        "run_passbook must be alive and retrying (gap-fill stall), not exited"
    );

    // ── Assert: meta.last_block is STILL 1 (no advance). ───────────────
    let g = ledger.lock().unwrap();
    let conn = g.conn();
    let lb: String = conn
        .query_row("SELECT v FROM meta WHERE k='last_block'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        lb, "1",
        "ledger high-water must NOT have advanced (gap-fill stalled on missing header)"
    );

    // ── Assert: zero block_not_delivered markers (header fetch never
    //   succeeded, so write_gap_block_marker was never called). ─────────
    let n_markers: i64 = conn
        .query_row(
            "SELECT count(*) FROM unattributed_deltas WHERE cause = 'block_not_delivered'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_markers, 0, "no markers should be written (header fetch failed)");

    // ── Assert: zero rows for block 5 (the per-block loop never ran). ─
    let n_block_5: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM eth_transfers WHERE block_number = 5)
                  + (SELECT count(*) FROM erc20_transfers WHERE block_number = 5)
                  + (SELECT count(*) FROM gas_payments WHERE block_number = 5)
                  + (SELECT count(*) FROM unattributed_deltas WHERE block_number = 5)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_block_5, 0, "block 5 must not have been processed (gap-fill blocked the per-block loop)");

    drop(g); // release ledger lock before the channel check

    // ── Assert: no FinishedHeight emitted for any height >= 5 (gap-fill
    //   stalled before reaching the lag_finished call at the end of the
    //   committed_chain branch). The channel might contain prior emits
    //   (none in this test, but be lenient on the lower bound). ─────────
    let mut saw_advance = false;
    while let Ok(ev) = handle.events_rx.try_recv() {
        let reth_ethereum::exex::ExExEvent::FinishedHeight(h) = ev;
        if h.number >= 5 {
            saw_advance = true;
            break;
        }
    }
    assert!(
        !saw_advance,
        "FinishedHeight for block 5 or higher must NOT be emitted while gap-fill is stalled"
    );

    driver.abort();
}
