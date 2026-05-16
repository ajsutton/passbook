use alloy_primitives::{Address, B256, U256};
use std::collections::HashSet;
use crate::erc20::{decode_transfer, RawLog};
use crate::inspector::CapturedFrame;
use crate::model::{Direction, Erc20TransferRow, GasPaymentRow};
use crate::reconcile::{reconcile_account, ReconcileInput};
use crate::ledger::writer::BlockBatch;

pub struct BlockInputs {
    pub chain_id: u64, pub block_number: u64, pub block_hash: B256,
    pub watched: HashSet<Address>,
    pub erc20_logs: Vec<(Option<B256>, u64, RawLog)>,      // (tx_hash, log_index, log)
    pub frames: Vec<(Option<B256>, bool, CapturedFrame)>,  // (tx_hash, reverted, frame)
    pub gas: Vec<GasPaymentRow>,
    pub account_deltas: Vec<(Address, i128)>,              // watched accounts touched
    pub system_signed: Vec<(Address, i128)>,              // recognised system credits
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[allow(dead_code)] // constructed in Task 6.3
    #[error("erc20 decode failure at block {block}")]
    Decode { block: u64 },
    #[error("unexplained reconciliation residual for {address} at block {block}: {residual}")]
    UnexplainedResidual { block: u64, address: Address, residual: i128 },
}

/// Pure: deterministic transform of one block's inputs into a durable batch.
/// Any unexplained residual ⇒ Err (caller must NOT advance / emit FinishedHeight).
pub fn process_block(i: BlockInputs) -> Result<BlockBatch, ProcessingError> {
    // (a) ERC20
    let mut erc20 = Vec::new();
    for (tx, log_index, log) in &i.erc20_logs {
        if let Some(d) = decode_transfer(log, &i.watched) {
            for (addr, dir) in d.matched {
                erc20.push(Erc20TransferRow {
                    chain_id: i.chain_id, block_number: i.block_number,
                    block_hash: i.block_hash,
                    tx_hash: tx.expect("erc20 log always in a tx"),
                    log_index: *log_index, token: d.token, from: d.from, to: d.to,
                    amount: d.amount, address: addr, direction: dir });
            }
        }
    }
    // (b) native frames
    let mut eth = Vec::new();
    let mut eth_in: std::collections::HashMap<Address, U256> = Default::default();
    let mut eth_out: std::collections::HashMap<Address, U256> = Default::default();
    for (tx, reverted, f) in &i.frames {
        let fr = [f.clone()];
        let rows = crate::attribution::attribute_eth_frames(
            i.chain_id, i.block_number, i.block_hash, *tx, *reverted, &fr, &i.watched);
        for r in &rows {
            match r.direction {
                Direction::In  => *eth_in.entry(r.address).or_default()  += r.amount_wei,
                Direction::Out => *eth_out.entry(r.address).or_default() += r.amount_wei,
            }
        }
        eth.extend(rows);
    }
    // gas per watched address
    let mut gas_paid: std::collections::HashMap<Address, U256> = Default::default();
    for g in &i.gas { *gas_paid.entry(g.address).or_default() += g.total_wei; }

    // (c) reconciliation — every touched watched address must balance
    let sys: std::collections::HashMap<Address, i128> =
        i.system_signed.iter().copied().collect();
    for (addr, observed) in &i.account_deltas {
        if !i.watched.contains(addr) { continue; }
        if let Some(_row) = reconcile_account(ReconcileInput {
            chain_id: i.chain_id, block_number: i.block_number,
            block_hash: i.block_hash, address: *addr, observed_delta: *observed,
            eth_in: eth_in.get(addr).copied().unwrap_or(U256::ZERO),
            eth_out: eth_out.get(addr).copied().unwrap_or(U256::ZERO),
            gas_paid: gas_paid.get(addr).copied().unwrap_or(U256::ZERO),
            system_signed: sys.get(addr).copied().unwrap_or(0),
        }) {
            return Err(ProcessingError::UnexplainedResidual {
                block: i.block_number, address: *addr, residual: *observed });
        }
    }
    Ok(BlockBatch {
        chain_id: i.chain_id, block_number: i.block_number, block_hash: i.block_hash,
        eth, erc20, gas: i.gas, unattributed: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256};

    #[test]
    fn clean_block_produces_batch() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id:1, block_number:10, block_hash:B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![], frames: vec![], gas: vec![],
            account_deltas: vec![(w, 0i128)], system_signed: vec![],
        };
        let r = process_block(inp).expect("clean");
        assert_eq!(r.block_number, 10);
        assert!(r.unattributed.is_empty());
    }

    #[test]
    fn unexplained_residual_is_processing_error() {
        let w = Address::repeat_byte(0xcc);
        let inp = BlockInputs {
            chain_id:1, block_number:10, block_hash:B256::repeat_byte(3),
            watched: [w].into_iter().collect(),
            erc20_logs: vec![], frames: vec![], gas: vec![],
            account_deltas: vec![(w, 999i128)], system_signed: vec![],
        };
        let err = process_block(inp).unwrap_err();
        assert!(matches!(err, ProcessingError::UnexplainedResidual { .. }));
    }
}
