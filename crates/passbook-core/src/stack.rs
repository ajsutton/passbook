use alloy_primitives::{Address, U256};

/// Isolates L1-vs-OP differences. Implemented per binary:
/// - ethereum: always returns None for the L1 data fee.
/// - optimism: computes per-tx L1 data fee via reth-optimism-evm.
/// `system_credits` surfaces recognised non-call balance changes
/// (L1 withdrawals/beacon deposits/block rewards, OP deposit mints / fee
/// vaults) so reconciliation attributes them as kind=system.
pub trait StackAdapter: Send + Sync + 'static {
    /// Per-transaction OP L1 data fee, or None on L1.
    fn l1_data_fee_wei(&self, tx_index: usize) -> Option<U256>;

    /// Recognised system balance credits/debits for this block,
    /// as (address, signed_wei) where positive = credit to address.
    fn system_credits(&self) -> Vec<(Address, i128)> { Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    struct NoL1;
    impl StackAdapter for NoL1 {
        fn l1_data_fee_wei(&self, _tx_index: usize) -> Option<U256> { None }
    }
    #[test]
    fn default_adapter_has_no_l1_fee() {
        assert_eq!(NoL1.l1_data_fee_wei(0), None);
    }
}
