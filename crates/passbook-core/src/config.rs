use alloy_primitives::Address;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct PassbookConfig {
    pub watched: HashSet<Address>,
    pub db_path: PathBuf,
}

impl PassbookConfig {
    /// Malformed address ⇒ Err (binary maps this to "abort node startup").
    /// Empty list ⇒ Ok with empty set (binary treats as ExEx-disabled).
    pub fn from_parts(addrs: Vec<String>, db_path: PathBuf) -> eyre::Result<Self> {
        let mut watched = HashSet::new();
        for a in addrs {
            let a = a.trim();
            if a.is_empty() {
                continue;
            }
            let addr = Address::from_str(a)
                .map_err(|e| eyre::eyre!("invalid watched address {a:?}: {e}"))?;
            watched.insert(addr);
        }
        if watched.len() > 10 {
            tracing::warn!(
                n = watched.len(),
                "watched set larger than design target (<10)"
            );
        }
        Ok(Self { watched, db_path })
    }
    pub fn enabled(&self) -> bool {
        !self.watched.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_valid_addresses() {
        let c = PassbookConfig::from_parts(
            vec!["0x0000000000000000000000000000000000000001".into()],
            "/tmp/x.db".into(),
        )
        .unwrap();
        assert_eq!(c.watched.len(), 1);
    }
    #[test]
    fn rejects_malformed_address() {
        assert!(PassbookConfig::from_parts(vec!["nope".into()], "/tmp/x.db".into()).is_err());
    }
    #[test]
    fn empty_list_is_disabled() {
        assert!(PassbookConfig::from_parts(vec![], "/tmp/x.db".into())
            .unwrap()
            .watched
            .is_empty());
    }
}
