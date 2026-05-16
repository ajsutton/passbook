//! L1 (Ethereum) `StackAdapter`.
//!
//! A stateless, unit adapter: vanilla Ethereum has no OP-style L1 data
//! fee, so `l1_data_fee_wei` always returns `None`. Recognised L1 system
//! balance credits (beacon withdrawals + the post-merge beneficiary
//! priority-fee block reward) are NOT surfaced through this adapter — they
//! need block/receipt input and are computed at the `ChainExec` seam
//! (`passbook_core::system::l1_system_credits`). The L1 binary supplies it
//! to
//! `run_passbook` via a `|| EthereumStack` closure
//! (`make_adapter: impl Fn() -> S + Send + Sync + 'static`), so the
//! type is `Clone`/`Copy`/`Default` to satisfy that consumption trivially.

use alloy_primitives::U256;
use passbook_core::stack::StackAdapter;

/// Stateless L1 stack adapter (vanilla Ethereum).
#[derive(Debug, Clone, Copy, Default)]
pub struct EthereumStack;

impl StackAdapter for EthereumStack {
    fn l1_data_fee_wei(&self, _tx_index: usize) -> Option<U256> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use passbook_core::stack::StackAdapter;
    #[test]
    fn ethereum_adapter_never_has_l1_fee() {
        assert_eq!(EthereumStack.l1_data_fee_wei(0), None);
    }
}
