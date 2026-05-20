pub mod queries;
pub mod schema;
pub mod writer;

use rusqlite::Connection;
use std::path::Path;

pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Open (creating if absent) and apply durability pragmas + schema.
    /// pragmas: WAL (concurrent reads), synchronous=FULL (never lose a
    /// committed row on power loss — write rate is trivial so fsync cost is
    /// irrelevant), busy_timeout 30s (retry-until-success writer never sees
    /// spurious SQLITE_BUSY), foreign_keys ON (per-connection default is off).
    pub fn open(path: &Path, chain_id: u64) -> eyre::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
        let exists: bool = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='meta'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0;
        if !exists {
            conn.execute_batch(schema::SCHEMA)?;
            conn.execute(
                "INSERT INTO meta(k,v) VALUES('schema_version',?1)",
                [schema::SCHEMA_VERSION],
            )?;
            conn.execute(
                "INSERT INTO meta(k,v) VALUES('chain_id',?1)",
                [chain_id.to_string()],
            )?;
        } else {
            let v: String =
                conn.query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                    r.get(0)
                })?;
            // Forward, in-place migrations. v1 → v2 (issue #4 / C3) adds
            // `address` to the `erc20_transfers` PRIMARY KEY. v2 → v3
            // (issue #15) adds the `cause` discriminator column to
            // `unattributed_deltas` distinguishing skip-path markers from
            // live-mode reconcile residuals. Wrap each DDL + version bump
            // in its own transaction so an interrupted upgrade never
            // leaves a half-migrated DB at the wrong version; the v1 → v3
            // path runs both in order.
            if v == "1" {
                let tx = conn.transaction()?;
                tx.execute_batch(schema::MIGRATE_V1_TO_V2)?;
                tx.execute("UPDATE meta SET v='2' WHERE k='schema_version'", [])?;
                tx.commit()?;
            }
            let v: String =
                conn.query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                    r.get(0)
                })?;
            if v == "2" {
                let tx = conn.transaction()?;
                tx.execute_batch(schema::MIGRATE_V2_TO_V3)?;
                tx.execute("UPDATE meta SET v='3' WHERE k='schema_version'", [])?;
                tx.commit()?;
            }
            let v: String =
                conn.query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                    r.get(0)
                })?;
            if v != schema::SCHEMA_VERSION {
                eyre::bail!("unsupported schema version {v}");
            }
            // Fail-closed chain-id guard: an existing ledger is bound to the
            // chain it was created for. `delete_blocks`/queries are
            // `chain_id`-scoped, so reopening with a different `--chain`
            // would silently mix chains in one DB. Abort loudly (spec's
            // stated preference) rather than corrupt a mixed ledger.
            let stored_chain_id: String =
                conn.query_row("SELECT v FROM meta WHERE k='chain_id'", [], |r| r.get(0))?;
            if stored_chain_id != chain_id.to_string() {
                eyre::bail!(
                    "chain-id mismatch: existing ledger is for chain {stored_chain_id}, \
                     but configured chain is {chain_id}"
                );
            }
        }
        Ok(Self { conn })
    }
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn open_sets_wal_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.db");
        let l = Ledger::open(&path, 1).unwrap();
        let jm: String = l
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm.to_lowercase(), "wal");
        let v: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, schema::SCHEMA_VERSION);
    }

    /// Issue #7 (I3): reopening an existing ledger with a different
    /// `chain_id` must fail loudly rather than silently mix chains.
    #[test]
    fn reopen_with_mismatched_chain_id_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.db");
        // Create the ledger bound to chain 1.
        drop(Ledger::open(&path, 1).unwrap());
        // Reopening with the SAME chain id succeeds.
        drop(Ledger::open(&path, 1).expect("reopen with same chain id"));
        // Reopening with a DIFFERENT chain id is a hard error and the
        // stored chain_id is left untouched.
        let msg = match Ledger::open(&path, 999) {
            Ok(_) => panic!("reopen with mismatched chain id must fail"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("chain-id mismatch"), "unexpected error: {msg}");
        let stored: String = {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.query_row("SELECT v FROM meta WHERE k='chain_id'", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(stored, "1", "stored chain_id must be left unchanged");
    }

    /// Issue #4 (C3): opening a legacy v1 DB must transparently migrate it
    /// to v2 — bumping the version AND adding `address` to the
    /// `erc20_transfers` PRIMARY KEY — while preserving existing rows.
    #[test]
    fn opening_v1_db_migrates_to_v2_preserving_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        // Hand-build a v1 database (old schema + old PK + a row).
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                r#"
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
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
"#,
            )
            .unwrap();
            c.execute("INSERT INTO meta(k,v) VALUES('schema_version','1')", [])
                .unwrap();
            c.execute("INSERT INTO meta(k,v) VALUES('chain_id','1')", [])
                .unwrap();
            c.execute(
                "INSERT INTO erc20_transfers VALUES \
                 (1,7,'0xbh','0xtx',0,'0xtok','0xfrom','0xto','99','0xfrom','out')",
                [],
            )
            .unwrap();
        }
        // Opening it must migrate forward to the current schema version
        // (chains v1 → v2 → v3) without error.
        let l = Ledger::open(&path, 1).unwrap();
        let v: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            v,
            schema::SCHEMA_VERSION,
            "v1 DB must be migrated forward to the current version"
        );
        // The pre-existing row survived the migration.
        let n: i64 = l
            .conn()
            .query_row("SELECT count(*) FROM erc20_transfers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "existing erc20 row preserved across migration");
        // And the new PK now includes `address`.
        let pk: Vec<String> = {
            let mut s = l
                .conn()
                .prepare(
                    "SELECT name FROM pragma_table_info('erc20_transfers') \
                     WHERE pk>0 ORDER BY pk",
                )
                .unwrap();
            s.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            pk,
            vec!["chain_id", "block_hash", "tx_hash", "log_index", "address"]
        );
        // Re-opening an already-current DB is a no-op (idempotent).
        drop(l);
        let l2 = Ledger::open(&path, 1).unwrap();
        let v2: String = l2
            .conn()
            .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v2, schema::SCHEMA_VERSION);
    }

    /// Issue #15: opening a legacy v2 DB must transparently migrate it
    /// forward to v3 by adding the `cause` column to
    /// `unattributed_deltas`. The only producer of marker rows at v2 was
    /// the worker-loop reconcile-residual stall (the skip path was added
    /// in the same change set as this column), so existing rows are
    /// backfilled as `cause = 'unexplained_residual'`; that label may
    /// over-attribute a small number of pre-fix skip-path markers to
    /// "reconcile residual" (operator surfaces a needlessly alarming
    /// row), but the reverse — labelling a real stall as a skip marker —
    /// is the worse error. We err on the side of "investigate".
    #[test]
    fn opening_v2_db_migrates_to_v3_backfills_cause_and_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_v2.db");
        // Hand-build a v2 database with the OLD `unattributed_deltas` (no
        // `cause` column) and a pre-existing marker row.
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                r#"
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
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
"#,
            )
            .unwrap();
            c.execute("INSERT INTO meta(k,v) VALUES('schema_version','2')", [])
                .unwrap();
            c.execute("INSERT INTO meta(k,v) VALUES('chain_id','1')", [])
                .unwrap();
            c.execute(
                "INSERT INTO unattributed_deltas VALUES \
                 (1,42,'0xbh','0xab','7','0','7')",
                [],
            )
            .unwrap();
        }
        // Opening must migrate v2 → v3 transparently.
        let l = Ledger::open(&path, 1).unwrap();
        let v: String = l
            .conn()
            .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, "3", "v2 DB must be migrated to v3");
        // The pre-existing row survived and was backfilled with the
        // conservative `unexplained_residual` cause.
        let (n, cause): (i64, String) = l
            .conn()
            .query_row(
                "SELECT count(*), MAX(cause) FROM unattributed_deltas",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "pre-existing marker preserved");
        assert_eq!(
            cause, "unexplained_residual",
            "historic rows backfilled as the conservative cause"
        );
    }
}
