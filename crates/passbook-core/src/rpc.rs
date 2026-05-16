//! `passbook` JSON-RPC namespace.
//!
//! Registered into reth's RPC server by the L1/OP binaries (Tasks 8.4/8.5)
//! via `extend_rpc_modules` using the generated `PassbookApiServer` trait's
//! `into_rpc()`.
//!
//! ## jsonrpsee 0.26 API (load-bearing — see `docs/reth-pin.md`)
//!
//! - macro: `#[rpc(server, namespace = "passbook")]` on a `pub trait`. With
//!   `namespace = "passbook"` and the default `_` separator, methods are
//!   exposed as `passbook_health` / `passbook_getTransfers`.
//! - The macro generates a trait named `<Trait>Server`, i.e.
//!   `PassbookApiServer`, with a provided `into_rpc(self) -> RpcModule<Self>`.
//! - `RpcResult<T>` is `jsonrpsee::core::RpcResult` =
//!   `Result<T, jsonrpsee::types::ErrorObjectOwned>`.
//! - The server impl block **must** carry `#[async_trait::async_trait]`
//!   (jsonrpsee 0.26 still desugars async trait methods through async-trait;
//!   `jsonrpsee::core::async_trait` re-exports the same macro — either works,
//!   `async-trait` is a direct workspace dep so we use it directly).
//! - Errors are constructed with
//!   `ErrorObjectOwned::owned(code, message, data)` where
//!   `owned<S: Serialize>(i32, impl Into<String>, Option<S>)`.
//!
//! ## Errors are never swallowed
//!
//! Every fallible step — mutex lock (poison) and the SQLite query — is mapped
//! through [`err`] into a JSON-RPC error object (code `-32000`) and returned
//! to the caller. No `unwrap`, no `Default`, no empty-result-on-error.

use crate::ledger::queries::{get_transfers, health, Health, TransfersPage};
use crate::ledger::Ledger;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use std::sync::{Arc, Mutex};

#[rpc(server, namespace = "passbook")]
pub trait PassbookApi {
    #[method(name = "health")]
    async fn health(&self) -> RpcResult<Health>;

    #[method(name = "getTransfers")]
    async fn get_transfers(
        &self,
        address: String,
        from_block: Option<u64>,
        to_block: Option<u64>,
        kind: Option<String>,
        cursor: Option<u64>,
    ) -> RpcResult<TransfersPage>;
}

/// Shares the **same** `Arc<Mutex<Ledger>>` the ExEx writer holds. This side
/// is read-only; the ExEx is the sole writer. The mutex serialises the single
/// rusqlite `Connection` (SQLite WAL gives concurrent readers at the file
/// level, but one `Connection` is not `Sync` for concurrent use).
#[derive(Clone)]
pub struct PassbookRpc {
    pub ledger: Arc<Mutex<Ledger>>,
    pub chain_id: u64,
}

/// Map any displayable error (lock poison, query failure) into a JSON-RPC
/// error object. Application error range code `-32000`.
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
        &self,
        address: String,
        from_block: Option<u64>,
        to_block: Option<u64>,
        kind: Option<String>,
        cursor: Option<u64>,
    ) -> RpcResult<TransfersPage> {
        let l = self.ledger.lock().map_err(err)?;
        get_transfers(
            l.conn(),
            self.chain_id,
            &address,
            from_block,
            to_block,
            kind.as_deref(),
            cursor,
            500,
        )
        .map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::writer::{write_block, BlockBatch};
    use crate::model::*;
    use alloy_primitives::{Address, B256, U256};

    fn rpc() -> (PassbookRpc, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Ledger::open(&dir.path().join("rpc.db"), 1).unwrap();
        // Insert one known eth row at block 9.
        let addr = Address::repeat_byte(0xaa);
        let b = BlockBatch {
            chain_id: 1,
            block_number: 9,
            block_hash: B256::repeat_byte(9),
            eth: vec![EthTransferRow {
                chain_id: 1,
                block_number: 9,
                block_hash: B256::repeat_byte(9),
                tx_hash: Some(B256::repeat_byte(3)),
                trace_path: "0".into(),
                address: addr,
                direction: Direction::In,
                counterparty: Address::repeat_byte(0xbb),
                amount_wei: U256::from(123u64),
                kind: EthKind::TopLevel,
                reverted: false,
            }],
            erc20: vec![],
            gas: vec![],
            unattributed: vec![],
        };
        write_block(l.conn_mut(), &b).unwrap();
        // Set last_block AFTER the block write (write_block also updates it).
        l.conn()
            .execute(
                "INSERT INTO meta(k,v) VALUES('last_block','77')
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                [],
            )
            .unwrap();
        (
            PassbookRpc {
                ledger: Arc::new(Mutex::new(l)),
                chain_id: 1,
            },
            dir,
        )
    }

    #[tokio::test]
    async fn health_returns_last_block() {
        let (r, _t) = rpc();
        let h = PassbookApiServer::health(&r).await.unwrap();
        assert_eq!(h.last_block, Some(77));
        assert_eq!(h.chain_id, Some(1));
    }

    #[tokio::test]
    async fn get_transfers_returns_known_row() {
        let (r, _t) = rpc();
        let addr = format!("{:#x}", Address::repeat_byte(0xaa));
        let page = PassbookApiServer::get_transfers(&r, addr, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        let row = &page.rows[0];
        assert_eq!(row.category, "eth");
        assert_eq!(row.block_number, 9);
        assert_eq!(row.amount, "123");
        assert_eq!(row.kind.as_deref(), Some("top_level"));
        assert_eq!(page.next_cursor, None);
    }

    /// Error path: a poisoned mutex must surface as a JSON-RPC error, never
    /// a panic or a silently-empty result.
    #[tokio::test]
    async fn poisoned_lock_is_a_jsonrpc_error_not_swallowed() {
        let (r, _t) = rpc();
        let r2 = r.clone();
        // Poison the mutex.
        let _ = std::thread::spawn(move || {
            let _g = r2.ledger.lock().unwrap();
            panic!("poison");
        })
        .join();
        let res = PassbookApiServer::health(&r).await;
        let e = res.expect_err("must be an error, not a swallowed empty value");
        assert_eq!(e.code(), -32000);
    }
}
