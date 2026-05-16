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

#[cfg(test)]
mod tests {
    #[test]
    fn schema_applies() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(super::SCHEMA_V1).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 5); // meta + 4 data tables
    }
}
