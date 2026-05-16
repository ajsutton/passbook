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

use std::collections::HashSet;
use crate::inspector::CapturedFrame;
use crate::model::{Direction, EthKind, EthTransferRow};

/// Top-level frames use kind=TopLevel (caller passes a `is_top_level`
/// flag via trace_path "tx:<i>"); internal frames use Internal. Here we
/// treat trace_path starting with "tx:" as top-level.
pub fn attribute_eth_frames(
    chain_id: u64, block_number: u64, block_hash: B256,
    tx_hash: Option<B256>, reverted: bool,
    frames: &[CapturedFrame], watched: &HashSet<Address>,
) -> Vec<EthTransferRow> {
    let mut out = Vec::new();
    for f in frames {
        let kind = if f.trace_path.starts_with("tx:") {
            EthKind::TopLevel } else { EthKind::Internal };
        if watched.contains(&f.to) {
            out.push(EthTransferRow {
                chain_id, block_number, block_hash, tx_hash, trace_path: f.trace_path.clone(),
                address: f.to, direction: Direction::In, counterparty: f.from,
                amount_wei: f.value, kind, reverted });
        }
        if watched.contains(&f.from) {
            out.push(EthTransferRow {
                chain_id, block_number, block_hash, tx_hash,
                trace_path: format!("{}:out", f.trace_path),
                address: f.from, direction: Direction::Out, counterparty: f.to,
                amount_wei: f.value, kind, reverted });
        }
    }
    out
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

    #[test]
    fn frames_attribute_in_and_out_for_watched() {
        use crate::inspector::{CapturedFrame, FrameKind};
        let w = Address::repeat_byte(0xcc);
        let frames = vec![
            CapturedFrame { from: Address::repeat_byte(1), to: w,
                value: U256::from(9), kind: FrameKind::Call, trace_path:"0".into() },
            CapturedFrame { from: w, to: Address::repeat_byte(2),
                value: U256::from(3), kind: FrameKind::SelfDestruct, trace_path:"1".into() },
        ];
        let watch = [w].into_iter().collect();
        let rows = attribute_eth_frames(
            1, 7, B256::ZERO, Some(B256::repeat_byte(1)), false, &frames, &watch);
        assert_eq!(rows.len(), 2);
        let (inb, outb): (Vec<_>,Vec<_>) =
            rows.iter().partition(|r| r.direction == crate::model::Direction::In);
        assert_eq!(inb[0].amount_wei, U256::from(9));
        assert_eq!(inb[0].kind, crate::model::EthKind::Internal);
        assert_eq!(outb[0].amount_wei, U256::from(3));
    }
}
