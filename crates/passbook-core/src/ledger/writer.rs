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

#[cfg(test)]
mod tests {
    use super::*;
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
}
