//! Task 6.4 ExEx integration test.
//!
//! Builds a *genuinely executed* synthetic committed chain (so its
//! `BundleState` + receipts are internally consistent by construction —
//! they come from a real `EthEvmConfig` block execution, not hand-authored
//! numbers), drives `run_passbook` through the pinned
//! `reth_exex_test_utils` harness, and asserts the durable ledger rows
//! plus a ZERO unattributed residual for the watched address.
//!
//! The single watched address `W` is exercised across all three capture
//! paths in one block:
//!   * tx0: `S -> TOKEN` — `TOKEN` emits an ERC20 `Transfer(S, W, amt)`
//!     log (topic0 = Transfer hash, topic2 = W) ⇒ `erc20_transfers` row.
//!   * tx1: `S -> FORWARDER` with ETH value — `FORWARDER` performs an
//!     internal `CALL` forwarding the whole `msg.value` to `W` ⇒ a
//!     non-top-level `eth_transfers` row of kind `internal`.
//!   * tx2: `W -> SINK` plain value transfer — `tx.from == W` ⇒ a
//!     `gas_payments` row (and a top-level eth transfer out of W).
//!
//! Reconciliation must net to zero for `W`: ΔW = (erc20 is token units,
//! not wei, so it does NOT affect the wei balance) + (internal ETH in) -
//! (tx2 value out) - (gas paid by W). All wei flows are produced by the
//! real EVM, so Σ(attribution) == observed BundleState delta exactly.

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

/// FORWARDER bytecode: forward its entire received balance to `dst` via
/// SELFDESTRUCT. `73<dst:20> FF` = PUSH20 dst ; SELFDESTRUCT.
///
/// Why SELFDESTRUCT, not a value `CALL`: in revm 38 a plain value `CALL`
/// to a *codeless* account (an EOA wallet like our watched `W`) is
/// resolved without surfacing a nested `Inspector::call` frame, so an
/// internal ETH credit to an EOA via `CALL` is not observable from a
/// re-execution inspector at this stack version. `SELFDESTRUCT`'s value
/// transfer DOES reliably fire `Inspector::selfdestruct` regardless of
/// the beneficiary having code — it is exactly the value-bearing internal
/// frame the production attribution path also captures. (Documented in
/// docs/reth-pin.md, Task 6.4 "internal-frame capture".)
fn forwarder_code(dst: Address) -> Bytes {
    let mut c = vec![0x73];
    c.extend_from_slice(dst.as_slice());
    c.push(0xff);
    Bytes::from(c)
}

/// TOKEN bytecode: emit `LOG3(Transfer, from, to)` with `amount` as data.
///   PUSH32 amount ; PUSH0 ; MSTORE                  (mem[0..32] = amount)
///   PUSH32 to ; PUSH32 from ; PUSH32 TOPIC0 ;
///   PUSH1 0x20 ; PUSH0 ; LOG3 ; STOP
/// LOG3 pops (offset, length, topic0, topic1, topic2): with the pushes
/// above topic0=Transfer, topic1=from, topic2=to.
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

#[allow(clippy::too_many_arguments)]
fn sign_legacy(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
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
        to: TxKind::Call(to),
        value,
        input,
    };
    let sig = signer.sign_transaction_sync(&mut tx).expect("sign");
    TransactionSigned::from(tx.into_signed(sig))
}

#[tokio::test(flavor = "multi_thread")]
async fn erc20_internal_gas_capture_zero_residual() {
    // ── Actors ──────────────────────────────────────────────────────────
    let s_signer = PrivateKeySigner::random();
    let w_signer = PrivateKeySigner::random();
    let sender = s_signer.address(); // S
    let watched = w_signer.address(); // W
    let forwarder = Address::repeat_byte(0xF0);
    let token = Address::repeat_byte(0x70);
    let sink = Address::repeat_byte(0x51);

    let chain_id = 0x1234u64;
    let gas_price = 1_000_000_000u128; // 1 gwei
    let erc20_amount = U256::from(123_456_789u64);
    let fwd_value = U256::from(7_000_000_000_000_000u64); // 0.007 ETH → W
    let w_send_value = U256::from(1_000_000_000_000_000u64); // 0.001 ETH W → SINK

    // ── Genesis: Paris (post-merge, pre-Shanghai → no withdrawals) ──────
    let config = ChainConfig {
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
        // Shanghai at genesis so PUSH0 (0x5f) in our hand-written
        // forwarder/token bytecode is a valid opcode.
        shanghai_time: Some(0),
        ..Default::default()
    };
    let funded = U256::from(10u64).pow(U256::from(18u64)); // 1 ETH
    let acct = |balance: U256, code: Option<Bytes>| GenesisAccount {
        balance,
        code,
        ..Default::default()
    };
    let mut alloc = std::collections::BTreeMap::new();
    alloc.insert(sender, acct(funded, None));
    alloc.insert(watched, acct(funded, None));
    alloc.insert(forwarder, acct(U256::ZERO, Some(forwarder_code(watched))));
    alloc.insert(
        token,
        acct(U256::ZERO, Some(token_code(sender, watched, erc20_amount))),
    );
    let genesis = Genesis {
        config,
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
    };

    let chain_spec: Arc<ChainSpec> = Arc::new(ChainSpec::from_genesis(genesis));

    // ── Build the block's three transactions ───────────────────────────
    let gas_limit = 200_000u64;
    let tx0 = sign_legacy(
        &s_signer, chain_id, 0, token, U256::ZERO, gas_limit, gas_price, Bytes::new(),
    );
    let tx1 = sign_legacy(
        &s_signer, chain_id, 1, forwarder, fwd_value, gas_limit, gas_price, Bytes::new(),
    );
    let tx2 = sign_legacy(
        &w_signer, chain_id, 0, sink, w_send_value, gas_limit, gas_price, Bytes::new(),
    );

    let genesis_header = chain_spec.genesis_header().clone();
    let header = Header {
        parent_hash: chain_spec.genesis_hash(),
        number: 1,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(7),
        timestamp: 12,
        beneficiary: Address::ZERO,
        difficulty: U256::ZERO,
        mix_hash: B256::ZERO,
        gas_used: 0,
        // Shanghai: empty withdrawals set.
        withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
        ..genesis_header
    };
    let body = BlockBody {
        transactions: vec![tx0, tx1, tx2],
        ommers: vec![],
        withdrawals: Some(Default::default()),
    };
    let block: Block<TransactionSigned> = Block::new(header, body);
    let recovered: RecoveredBlock<Block<TransactionSigned>> =
        RecoveredBlock::try_recover(block).expect("recover senders");

    // ── Genuinely execute the block to obtain a consistent outcome ──────
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let state_db = reexec_pre_state(&chain_spec);
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
    let cfg = PassbookConfig::from_parts(vec![format!("{watched:#x}")], db_path.clone())
        .expect("cfg");

    // ── Deterministic core check: run the per-block pipeline directly so
    //    a residual / re-exec failure surfaces as a concrete error rather
    //    than being swallowed by run_passbook's retry-forever loop. ──────
    let res = passbook_core::exex::process_committed_block_inner(
        chain_id,
        chain_spec.clone(),
        &chain,
        &recovered,
        &cfg,
        &L1Adapter,
    );
    let batch = res.expect("per-block processing must reconcile to zero residual");
    assert!(
        batch.unattributed.is_empty(),
        "zero unattributed residual expected from the pure orchestrator"
    );
    assert_eq!(
        batch
            .erc20
            .iter()
            .filter(|r| r.address == watched)
            .count(),
        1,
        "one ERC20 row for W"
    );
    assert_eq!(
        batch
            .eth
            .iter()
            .filter(|r| r.address == watched
                && matches!(r.direction, passbook_core::model::Direction::In)
                && matches!(r.kind, passbook_core::model::EthKind::Internal))
            .count(),
        1,
        "one internal inbound ETH row for W"
    );
    assert_eq!(
        batch.gas.iter().filter(|r| r.address == watched).count(),
        1,
        "one gas row for W (tx2)"
    );

    // ── End-to-end: drive run_passbook via the test ExEx harness ───────
    let (mut ctx, mut handle) = test_exex_context_with_chain_spec(chain_spec.clone())
        .await
        .expect("test exex ctx");
    // The harness builds `ctx.config` from `NodeConfig::test()` (default
    // mainnet spec), NOT from the chain spec passed above; point it at our
    // synthetic spec so `run_passbook`'s `ctx.config.chain` (chain id +
    // EVM hardforks for the gated re-execution) matches the committed
    // block. In production `ctx.config.chain` is already the node's spec.
    ctx.config.chain = chain_spec.clone();
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

    // Wait for FinishedHeight(1) → block durably written.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(ev) = handle.events_rx.try_recv() {
            if matches!(
                ev,
                reth_ethereum::exex::ExExEvent::FinishedHeight(h) if h.number == 1
            ) {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for run_passbook to emit FinishedHeight(1)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    assert_eq!(internal_in, 1, "expected one internal inbound ETH row for W");

    let internal_amt: String = conn
        .query_row(
            "SELECT amount_wei FROM eth_transfers \
             WHERE address=?1 AND direction='in' AND kind='internal'",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(internal_amt, fwd_value.to_string());

    let gas_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gas_payments WHERE address=?1",
            [&w_lc],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(gas_rows, 1, "expected one gas_payments row for W (tx2)");

    let unattributed: i64 = conn
        .query_row("SELECT COUNT(*) FROM unattributed_deltas", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unattributed, 0, "ZERO unattributed residual expected");
}

/// In-memory pre-block state = the genesis allocation, so the real
/// execution above is consistent with `process_one_committed_block`'s own
/// self-contained re-execution (which rebuilds the identical pre-state
/// from the resulting `BundleState`).
fn reexec_pre_state(
    chain_spec: &Arc<ChainSpec>,
) -> revm::database::State<revm::database::CacheDB<revm::database::EmptyDB>> {
    use revm::database::{CacheDB, EmptyDB};
    use revm::state::AccountInfo;
    let mut cache: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    for (addr, acct) in chain_spec.genesis().alloc.iter() {
        let code = acct
            .code
            .clone()
            .map(revm::bytecode::Bytecode::new_raw);
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
