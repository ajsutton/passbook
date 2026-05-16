//! OP-Stack `StackAdapter`.
//!
//! [`OptimismStack`] is a *plain table* of per-transaction L1 data fees for
//! ONE block, precomputed by the OP binary (Task 8.5) so `passbook-core`
//! stays entirely OP-free. The table is positional: index = tx position in
//! the block. Deposit transactions (and the L1-info tx at index 0) carry no
//! L1 data fee and are recorded as `None`.
//!
//! # How the OP binary fills the table (verified reth-optimism-evm API)
//!
//! Pinned `reth-optimism-evm` (optimism monorepo rev `27bf9194`, crate
//! `1.11.3`; source `rust/op-reth/crates/evm/src/l1.rs`):
//!
//! - `reth_optimism_evm::extract_l1_info::<B>(body: &B) -> Result<L1BlockInfo,
//!   OpBlockExecutionError>` where `B: reth_primitives_traits::BlockBody`
//!   (`l1.rs:25`). Parses the first (L1-info) tx of the L2 block into an
//!   `op_revm::L1BlockInfo`.
//! - Trait `reth_optimism_evm::RethL1BlockInfo` (`l1.rs:295`, impl'd for
//!   `op_revm::L1BlockInfo` at `l1.rs:325`) provides:
//!   ```text
//!   fn l1_tx_data_fee(
//!       &mut self,
//!       chain_spec: impl reth_optimism_forks::OpHardforks,
//!       timestamp: u64,
//!       input: &[u8],          // EIP-2718 encoded raw tx bytes
//!       is_deposit: bool,
//!   ) -> Result<alloy_primitives::U256, reth_execution_errors::BlockExecutionError>
//!   ```
//!   `is_deposit == true` short-circuits to `U256::ZERO` (`l1.rs:333`).
//!
//! The OP binary, per committed block, calls `extract_l1_info(&block.body)`
//! once, then for each tx calls `l1_tx_data_fee(&chain_spec,
//! block.timestamp, &tx.encoded_2718(), tx.is_deposit())`, mapping the
//! L1-info tx / deposits to `None` and a real fee to `Some(fee)`, and feeds
//! the resulting `Vec<Option<U256>>` into [`OptimismStack::from_fees`].
//!
//! [`build_block_l1_fee_table`] is the binary-side glue: it owns the
//! deposit→`None` rule and the positional-table construction so Task 8.5
//! only supplies a per-tx fee closure (which closes over the extracted
//! `L1BlockInfo` + chain spec + timestamp). Keeping the `OpHardforks` /
//! `L1BlockInfo` / `BlockExecutionError` types out of this crate's public
//! signature is deliberate: those crates are not direct dependencies (the
//! committed `Cargo.lock` must not change), and they are reachable at the
//! Task 8.5 call site where the concrete chain-spec type is known.

mod op_chain;
pub use op_chain::OpChainExec;

use passbook_core::stack::StackAdapter;
use alloy_primitives::U256;

/// Per-tx L1 data fees for ONE block, precomputed by the OP binary via
/// reth-optimism-evm. Deposit txs → None. A plain table so core stays OP-free.
#[derive(Debug, Clone, Default)]
pub struct OptimismStack { fees: Vec<Option<U256>> }

impl OptimismStack {
    pub fn from_fees(fees: Vec<Option<U256>>) -> Self { Self { fees } }
}

impl StackAdapter for OptimismStack {
    fn l1_data_fee_wei(&self, tx_index: usize) -> Option<U256> {
        self.fees.get(tx_index).copied().flatten()
    }
}

/// Build the positional per-tx L1-data-fee table for one OP block.
///
/// Binary-side glue for Task 8.5. The caller iterates the block's
/// transactions and supplies, for each, `(is_deposit, raw_2718)` where
/// `raw_2718` is the EIP-2718 encoded transaction bytes. `fee_of` is the
/// caller's closure that wraps the verified
/// `reth_optimism_evm::RethL1BlockInfo::l1_tx_data_fee` call (closing over
/// the extracted `op_revm::L1BlockInfo`, the chain spec, and the block
/// timestamp). This function owns the invariant that deposit transactions
/// (and, in practice, the L1-info tx at index 0, which is itself a deposit)
/// contribute no L1 data fee and are recorded as `None`; any other tx whose
/// `fee_of` yields `None` (e.g. the underlying call returned an error) is
/// also recorded as `None` rather than panicking.
///
/// Kept generic and OP-type-free so this crate's `Cargo.lock` footprint is
/// unchanged; wiring + integration coverage live in Task 8.5.
pub fn build_block_l1_fee_table<I, F>(txs: I, mut fee_of: F) -> OptimismStack
where
    I: IntoIterator<Item = (bool, Vec<u8>)>,
    F: FnMut(&[u8]) -> Option<U256>,
{
    let fees = txs
        .into_iter()
        .map(|(is_deposit, raw_2718)| {
            if is_deposit {
                None
            } else {
                fee_of(&raw_2718)
            }
        })
        .collect();
    OptimismStack::from_fees(fees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use passbook_core::stack::StackAdapter;

    #[test]
    fn optimism_adapter_exposes_precomputed_l1_fees() {
        let a = OptimismStack::from_fees(vec![Some(U256::from(500))]);
        assert_eq!(a.l1_data_fee_wei(0), Some(U256::from(500)));
        assert_eq!(a.l1_data_fee_wei(9), None);
    }

    #[test]
    fn build_table_marks_deposits_none_and_passes_through_fees() {
        // tx0: deposit (L1-info) -> None even though closure would yield a fee
        // tx1: normal -> Some(fee)
        // tx2: normal but closure yields None -> None
        let txs = vec![
            (true, vec![0x7e]),
            (false, vec![0x02, 0xaa]),
            (false, vec![0x02, 0xbb]),
        ];
        let table = build_block_l1_fee_table(txs, |raw| {
            if raw == [0x02, 0xaa] { Some(U256::from(777)) } else { None }
        });
        assert_eq!(table.l1_data_fee_wei(0), None);
        assert_eq!(table.l1_data_fee_wei(1), Some(U256::from(777)));
        assert_eq!(table.l1_data_fee_wei(2), None);
    }
}
