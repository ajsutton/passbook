/// Current schema version. Bumped to 2 to fix issue #4 (C3): the v1
/// `erc20_transfers` PRIMARY KEY omitted `address`, so a watched→watched
/// ERC20 transfer (two directional rows sharing
/// `(chain_id, block_hash, tx_hash, log_index)`) had one row silently
/// destroyed by `INSERT OR REPLACE`. v2 adds `address` to the PK.
pub const SCHEMA_VERSION: &str = "2";

pub const SCHEMA: &str = r#"
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
  PRIMARY KEY (chain_id, block_hash, tx_hash, log_index, address)
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

/// In-place v1 → v2 migration (issue #4 / C3): rebuild `erc20_transfers`
/// with `address` added to the PRIMARY KEY so a watched→watched transfer's
/// two directional rows no longer collide under `INSERT OR REPLACE`.
/// Existing v1 rows are preserved (they had at most one row per
/// `(chain_id, block_hash, tx_hash, log_index)`, so re-inserting under the
/// wider key is loss-free); a re-index of the affected block on the next
/// run rewrites the previously-clobbered counterpart row idempotently.
/// Run inside the caller's transaction.
pub const MIGRATE_V1_TO_V2: &str = r#"
CREATE TABLE erc20_transfers_v2 (
  chain_id INTEGER NOT NULL, block_number INTEGER NOT NULL, block_hash TEXT NOT NULL,
  tx_hash TEXT NOT NULL, log_index INTEGER NOT NULL, token TEXT NOT NULL,
  from_addr TEXT NOT NULL, to_addr TEXT NOT NULL, amount TEXT NOT NULL,
  address TEXT NOT NULL, direction TEXT NOT NULL,
  PRIMARY KEY (chain_id, block_hash, tx_hash, log_index, address)
);
INSERT INTO erc20_transfers_v2
  SELECT chain_id, block_number, block_hash, tx_hash, log_index, token,
         from_addr, to_addr, amount, address, direction
    FROM erc20_transfers;
DROP TABLE erc20_transfers;
ALTER TABLE erc20_transfers_v2 RENAME TO erc20_transfers;
CREATE INDEX ix_erc20_addr ON erc20_transfers (chain_id, address, block_number);
CREATE INDEX ix_erc20_bh   ON erc20_transfers (block_hash);
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn schema_applies() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(super::SCHEMA).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 5); // meta + 4 data tables
    }

    /// The current schema's `erc20_transfers` PK must include `address` so
    /// a watched→watched transfer's two directional rows coexist (issue #4).
    #[test]
    fn erc20_pk_includes_address() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(super::SCHEMA).unwrap();
        let cols: Vec<String> = {
            let mut s = c
                .prepare(
                    "SELECT name FROM pragma_table_info('erc20_transfers') WHERE pk>0 ORDER BY pk",
                )
                .unwrap();
            let rows = s
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            rows
        };
        assert_eq!(
            cols,
            vec!["chain_id", "block_hash", "tx_hash", "log_index", "address"],
            "erc20_transfers PK must include address (issue #4)"
        );
    }
}
