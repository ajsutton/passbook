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

/// Why an `unattributed_deltas` row exists. Two genuinely different
/// operational meanings share one table:
///
/// - [`UnattributedDeltaCause::ParentStateUnavailable`] — skip-mode
///   marker: the gated re-execution could not obtain parent state, so
///   the inspector frames that would attribute the watched-account
///   delta are unavailable. `residual_wei` is what is left after netting
///   any recognised `system_signed` credits against the observed delta.
///   Expected to occur briefly during staged-pipeline backfill; not a
///   processing failure (the ExEx advances). Operators auditing
///   attribution completeness can re-index these blocks once parent
///   state is available, OR accept the partial batch + the marker as a
///   declared gap.
/// - [`UnattributedDeltaCause::UnexplainedResidual`] — live-mode
///   diagnostic: `process_block`'s reconcile loop found an
///   `observed_delta ≠ attributed_sum` and the ExEx STALLED on the
///   block (no advance). The row records the stall for the health
///   query; the ExEx retries the SAME block forever. This is a hard
///   processing failure that must be investigated.
///
/// Stored as the SQLite-side `cause` column (TEXT NOT NULL); the
/// `as_str` / `from_str` pair is the single source for the on-disk
/// spelling, mirroring [`Direction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnattributedDeltaCause {
    /// Skip-path marker (parent state unavailable).
    ParentStateUnavailable,
    /// Live-mode reconcile residual (block STALLED).
    UnexplainedResidual,
}

impl UnattributedDeltaCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ParentStateUnavailable => "parent_state_unavailable",
            Self::UnexplainedResidual => "unexplained_residual",
        }
    }
}

impl std::str::FromStr for UnattributedDeltaCause {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "parent_state_unavailable" => Ok(Self::ParentStateUnavailable),
            "unexplained_residual" => Ok(Self::UnexplainedResidual),
            other => Err(format!("unknown unattributed_deltas cause: {other}")),
        }
    }
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
    /// Discriminator: skip-path marker vs reconcile stall. See
    /// [`UnattributedDeltaCause`].
    pub cause: UnattributedDeltaCause,
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

    /// Issue #15: the `cause` discriminator's on-disk spelling is part of
    /// the schema; round-trip both variants to nail it down.
    #[test]
    fn unattributed_delta_cause_roundtrips_as_str() {
        assert_eq!(
            UnattributedDeltaCause::ParentStateUnavailable.as_str(),
            "parent_state_unavailable"
        );
        assert_eq!(
            UnattributedDeltaCause::UnexplainedResidual.as_str(),
            "unexplained_residual"
        );
        assert_eq!(
            UnattributedDeltaCause::from_str("parent_state_unavailable").unwrap(),
            UnattributedDeltaCause::ParentStateUnavailable
        );
        assert_eq!(
            UnattributedDeltaCause::from_str("unexplained_residual").unwrap(),
            UnattributedDeltaCause::UnexplainedResidual
        );
        assert!(UnattributedDeltaCause::from_str("nope").is_err());
    }
}
