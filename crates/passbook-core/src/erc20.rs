use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use std::collections::HashSet;
use crate::model::Direction;

/// keccak256("Transfer(address,address,uint256)")
pub static TRANSFER_TOPIC0: B256 = B256::new([
    0xdd,0xf2,0x52,0xad,0x1b,0xe2,0xc8,0x9b,0x69,0xc2,0xb0,0x68,0xfc,0x37,0x8d,0xaa,
    0x95,0x2b,0xa7,0xf1,0x63,0xc4,0xa1,0x16,0x28,0xf5,0x5a,0x4d,0xf5,0x23,0xb3,0xef]);

/// Minimal node-generic log shape (decouples core from reth log types;
/// `exex.rs` maps reth logs into this).
#[derive(Debug, Clone)]
pub struct RawLog { pub address: Address, pub topics: Vec<B256>, pub data: Bytes }

#[derive(Debug, Clone)]
pub struct DecodedTransfer {
    pub token: Address, pub from: Address, pub to: Address, pub amount: U256,
    /// Which watched addresses matched and the direction for each.
    pub matched: Vec<(Address, Direction)>,
}

fn topic_to_address(t: &B256) -> Address { Address::from_slice(&t.as_slice()[12..]) }

/// Returns Some when topic0 is Transfer and from|to ∈ watched.
pub fn decode_transfer(log: &RawLog, watched: &HashSet<Address>) -> Option<DecodedTransfer> {
    if log.topics.len() != 3 || log.topics[0] != TRANSFER_TOPIC0 { return None; }
    let from = topic_to_address(&log.topics[1]);
    let to = topic_to_address(&log.topics[2]);
    if log.data.len() < 32 { return None; }
    let amount = U256::from_be_slice(&log.data[..32]);
    let mut matched = Vec::new();
    if watched.contains(&to)   { matched.push((to,   Direction::In));  }
    if watched.contains(&from) { matched.push((from, Direction::Out)); }
    if matched.is_empty() { return None; }
    Some(DecodedTransfer { token: log.address, from, to, amount, matched })
}

#[allow(dead_code)]
fn _compile_time_topic_check() {
    debug_assert_eq!(TRANSFER_TOPIC0,
        keccak256("Transfer(address,address,uint256)".as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bytes, U256};

    fn topic_addr(a: Address) -> B256 {
        let mut b = [0u8; 32]; b[12..].copy_from_slice(a.as_slice()); B256::from(b)
    }

    #[test]
    fn decodes_inbound_transfer_for_watched_to() {
        let watched = Address::repeat_byte(0xcc);
        let from = Address::repeat_byte(0x11);
        let token = Address::repeat_byte(0x99);
        let log = RawLog {
            address: token,
            topics: vec![TRANSFER_TOPIC0, topic_addr(from), topic_addr(watched)],
            data: Bytes::from(U256::from(1234).to_be_bytes::<32>().to_vec()),
        };
        let watch = [watched].into_iter().collect();
        let out = decode_transfer(&log, &watch).unwrap();
        assert_eq!(out.from, from);
        assert_eq!(out.to, watched);
        assert_eq!(out.amount, U256::from(1234));
        assert_eq!(out.matched, vec![(watched, crate::model::Direction::In)]);
    }

    #[test]
    fn ignores_non_transfer_and_unwatched() {
        let watch = [Address::repeat_byte(0xcc)].into_iter().collect();
        let other = RawLog { address: Address::ZERO,
            topics: vec![B256::repeat_byte(1)], data: Default::default() };
        assert!(decode_transfer(&other, &watch).is_none());
    }

    #[test]
    fn topic0_matches_keccak() {
        assert_eq!(TRANSFER_TOPIC0,
            alloy_primitives::keccak256("Transfer(address,address,uint256)"));
    }
}
