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
            let addr = parse_watched_address(a)?;
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

/// Parse a single watched address, enforcing EIP-55 checksum integrity.
///
/// Per EIP-55, an all-lowercase or all-uppercase hex address carries no
/// checksum information and is accepted as-is. A *mixed-case* address embeds
/// a checksum, so we validate it and abort on any mismatch — catching a
/// single mistyped-but-hex-valid character that would otherwise silently
/// resolve to the wrong address.
fn parse_watched_address(a: &str) -> eyre::Result<Address> {
    let addr =
        Address::from_str(a).map_err(|e| eyre::eyre!("invalid watched address {a:?}: {e}"))?;

    // Strip an optional "0x"/"0X" prefix before classifying case.
    let hex = a
        .strip_prefix("0x")
        .or_else(|| a.strip_prefix("0X"))
        .unwrap_or(a);
    let has_lower = hex.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = hex.chars().any(|c| c.is_ascii_uppercase());

    if has_lower && has_upper && a != addr.to_checksum_buffer(None).as_str() {
        return Err(eyre::eyre!(
            "watched address {a:?} has an invalid EIP-55 checksum \
             (expected {}); a mistyped character may have produced the wrong \
             address",
            addr.to_checksum_buffer(None).as_str()
        ));
    }
    Ok(addr)
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
    fn accepts_all_lowercase_address() {
        // No checksum info ⇒ accepted per EIP-55.
        assert!(PassbookConfig::from_parts(
            vec!["0xd8da6bf26964af9d7eed9e03e53415d37aa96045".into()],
            "/tmp/x.db".into(),
        )
        .is_ok());
    }
    #[test]
    fn accepts_all_uppercase_address() {
        assert!(PassbookConfig::from_parts(
            vec!["0xD8DA6BF26964AF9D7EED9E03E53415D37AA96045".into()],
            "/tmp/x.db".into(),
        )
        .is_ok());
    }
    #[test]
    fn accepts_valid_eip55_checksummed_address() {
        assert!(PassbookConfig::from_parts(
            vec!["0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".into()],
            "/tmp/x.db".into(),
        )
        .is_ok());
    }
    #[test]
    fn rejects_bad_eip55_checksum() {
        // Valid checksum is 0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045.
        // Flip one nibble's case (D->d at position 4): still valid hex, but
        // the EIP-55 checksum no longer matches ⇒ must abort.
        let bad = "0xd8da6BF26964aF9D7eEd9e03E53415D37aA96045".to_string();
        let res = PassbookConfig::from_parts(vec![bad], "/tmp/x.db".into());
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("EIP-55 checksum"));
    }
    #[test]
    fn empty_list_is_disabled() {
        assert!(PassbookConfig::from_parts(vec![], "/tmp/x.db".into())
            .unwrap()
            .watched
            .is_empty());
    }
}
