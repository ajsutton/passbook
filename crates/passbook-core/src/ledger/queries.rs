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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn health_reports_last_block() {
        let dir = tempfile::tempdir().unwrap();
        let l = crate::ledger::Ledger::open(&dir.path().join("q.db"), 1).unwrap();
        l.conn().execute(
            "INSERT INTO meta(k,v) VALUES('last_block','42')
             ON CONFLICT(k) DO UPDATE SET v=excluded.v", []).unwrap();
        assert_eq!(health(l.conn()).unwrap().last_block, Some(42));
    }
}
