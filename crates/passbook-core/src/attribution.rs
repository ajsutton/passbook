use alloy_primitives::{Address, B256, U256};
use crate::model::GasPaymentRow;

pub struct GasInput {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub tx_hash: B256, pub tx_from: Address,
    pub gas_used: u64, pub effective_gas_price: u128,
    pub l1_fee_wei: Option<U256>,
}

/// Charged whenever tx.from ∈ watched, even on reverted txs.
pub fn compute_gas_payment(i: GasInput) -> GasPaymentRow {
    let l2 = U256::from(i.gas_used) * U256::from(i.effective_gas_price);
    let total = l2 + i.l1_fee_wei.unwrap_or(U256::ZERO);
    GasPaymentRow {
        chain_id: i.chain_id, block_number: i.block_number, block_hash: i.block_hash,
        tx_hash: i.tx_hash, address: i.tx_from, gas_used: i.gas_used,
        effective_gas_price: i.effective_gas_price, l2_fee_wei: l2,
        l1_fee_wei: i.l1_fee_wei, total_wei: total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};

    #[test]
    fn gas_payment_includes_l1_when_present() {
        let g = compute_gas_payment(GasInput {
            chain_id:1, block_number:7, block_hash:B256::ZERO,
            tx_hash:B256::repeat_byte(1), tx_from:Address::repeat_byte(0xaa),
            gas_used:21000, effective_gas_price:1_000_000_000u128,
            l1_fee_wei: Some(U256::from(500)) });
        assert_eq!(g.l2_fee_wei, U256::from(21000u64) * U256::from(1_000_000_000u64));
        assert_eq!(g.total_wei, g.l2_fee_wei + U256::from(500));
        assert_eq!(g.l1_fee_wei, Some(U256::from(500)));
    }
}
