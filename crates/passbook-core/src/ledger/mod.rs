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
