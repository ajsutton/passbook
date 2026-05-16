use crate::model::UnattributedDeltaRow;
use alloy_primitives::{Address, B256, U256};

pub struct ReconcileInput {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub address: Address,
    /// observed post-state balance delta (new - old), signed wei.
    pub observed_delta: i128,
    pub eth_in: U256,
    pub eth_out: U256,
    pub gas_paid: U256,
    /// recognised system credit (+) / debit (-) in wei.
    pub system_signed: i128,
}

/// attributed = eth_in - eth_out - gas_paid + system_signed.
/// Returns Some(row) iff |observed - attributed| != 0. A returned row means
/// the caller MUST treat the block as a processing failure (do not advance,
/// do not emit FinishedHeight) and persist this row as the diagnostic.
pub fn reconcile_account(i: ReconcileInput) -> Option<UnattributedDeltaRow> {
    let to_i = |u: U256| -> i128 { u.try_into().unwrap_or(i128::MAX) };
    let attributed: i128 = to_i(i.eth_in)
        .saturating_sub(to_i(i.eth_out))
        .saturating_sub(to_i(i.gas_paid))
        .saturating_add(i.system_signed);
    let residual = i.observed_delta - attributed;
    if residual == 0 {
        return None;
    }
    Some(UnattributedDeltaRow {
        chain_id: i.chain_id,
        block_number: i.block_number,
        block_hash: i.block_hash,
        address: i.address,
        observed_wei: U256::from(i.observed_delta.unsigned_abs()),
        attributed_wei: U256::from(attributed.unsigned_abs()),
        residual_wei: U256::from(residual.unsigned_abs()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};

    #[test]
    fn balanced_account_has_no_residual() {
        let addr = Address::repeat_byte(0xaa);
        let r = reconcile_account(ReconcileInput {
            chain_id: 1,
            block_number: 5,
            block_hash: B256::ZERO,
            address: addr,
            observed_delta: 100i128,
            eth_in: U256::from(150),
            eth_out: U256::from(20),
            gas_paid: U256::from(30),
            system_signed: 0i128,
        });
        assert!(r.is_none()); // 150 - 20 - 30 == 100
    }

    #[test]
    fn imbalance_yields_unattributed_row() {
        let addr = Address::repeat_byte(0xaa);
        let r = reconcile_account(ReconcileInput {
            chain_id: 1,
            block_number: 5,
            block_hash: B256::ZERO,
            address: addr,
            observed_delta: 100i128,
            eth_in: U256::from(10),
            eth_out: U256::ZERO,
            gas_paid: U256::ZERO,
            system_signed: 0i128,
        })
        .unwrap();
        assert_eq!(r.residual_wei, U256::from(90)); // |100 - 10|
    }
}
