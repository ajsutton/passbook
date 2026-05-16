use alloy_primitives::{Address, B256, U256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}
impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}
impl std::str::FromStr for Direction {
    type Err = eyre::Report;
    fn from_str(s: &str) -> eyre::Result<Self> {
        match s {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            _ => Err(eyre::eyre!("bad direction {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EthKind {
    TopLevel,
    Internal,
    System,
}
impl EthKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TopLevel => "top_level",
            Self::Internal => "internal",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EthTransferRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_hash: Option<B256>,
    pub trace_path: String,
    pub address: Address,
    pub direction: Direction,
    pub counterparty: Address,
    pub amount_wei: U256,
    pub kind: EthKind,
    pub reverted: bool,
}

#[derive(Debug, Clone)]
pub struct Erc20TransferRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    pub token: Address,
    pub from: Address,
    pub to: Address,
    pub amount: U256,
    pub address: Address,
    pub direction: Direction,
}

#[derive(Debug, Clone)]
pub struct GasPaymentRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub address: Address,
    pub gas_used: u64,
    pub effective_gas_price: u128,
    pub l2_fee_wei: U256,
    pub l1_fee_wei: Option<U256>,
    pub total_wei: U256,
}

#[derive(Debug, Clone)]
pub struct UnattributedDeltaRow {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub address: Address,
    pub observed_wei: U256,
    pub attributed_wei: U256,
    pub residual_wei: U256,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    #[test]
    fn direction_roundtrips_as_str() {
        assert_eq!(Direction::In.as_str(), "in");
        assert_eq!(Direction::Out.as_str(), "out");
        assert_eq!(Direction::from_str("in").unwrap(), Direction::In);
    }
}
