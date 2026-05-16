use alloy_primitives::U256;

/// Isolates the L1-vs-OP **per-tx gas** difference. Implemented per binary:
/// - ethereum: always returns `None` for the L1 data fee.
/// - optimism: computes per-tx L1 data fee via reth-optimism-evm.
///
/// Recognised **non-call system balance changes** (L1
/// withdrawals/block-reward, OP deposit mints) are NOT surfaced through
/// this adapter — they require the block/receipts as input, so they are
/// computed at the [`ChainExec`](crate::exex::ChainExec) seam and fed into
/// [`BlockInputs::system_signed`](crate::exex::BlockInputs) as
/// [`SystemCredit`](crate::system::SystemCredit)s. See [`crate::system`].
pub trait StackAdapter: Send + Sync + 'static {
    /// Per-transaction OP L1 data fee, or None on L1.
    fn l1_data_fee_wei(&self, tx_index: usize) -> Option<U256>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    struct NoL1;
    impl StackAdapter for NoL1 {
        fn l1_data_fee_wei(&self, _tx_index: usize) -> Option<U256> {
            None
        }
    }
    #[test]
    fn default_adapter_has_no_l1_fee() {
        assert_eq!(NoL1.l1_data_fee_wei(0), None);
    }
}
