use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Clone, Args)]
pub struct PassbookArgs {
    /// Comma-separated watched addresses (≤10). Absent ⇒ ExEx disabled.
    #[arg(
        long = "passbook.addresses",
        env = "PASSBOOK_ADDRESSES",
        value_delimiter = ',',
        default_value = ""
    )]
    pub addresses: Vec<String>,

    /// Ledger SQLite path.
    #[arg(
        long = "passbook.db-path",
        env = "PASSBOOK_DB_PATH",
        default_value = "/data/passbook.db"
    )]
    pub db_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        p: PassbookArgs,
    }
    #[test]
    fn parses_flags() {
        let w = W::parse_from([
            "x",
            "--passbook.addresses",
            "0x0000000000000000000000000000000000000001",
            "--passbook.db-path",
            "/tmp/p.db",
        ]);
        assert_eq!(w.p.addresses.len(), 1);
        assert_eq!(w.p.db_path.to_str().unwrap(), "/tmp/p.db");
    }

    #[test]
    fn defaults_when_absent() {
        let w = W::parse_from(["x"]);
        // Absent ⇒ empty or [""]; PassbookConfig::from_parts trims+skips empties,
        // so either form yields an empty watched set (ExEx disabled).
        assert!(w.p.addresses.iter().all(|s| s.trim().is_empty()) || w.p.addresses.is_empty());
        assert_eq!(w.p.db_path.to_str().unwrap(), "/data/passbook.db");
    }
}
