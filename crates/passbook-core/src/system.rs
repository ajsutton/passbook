//! Recognised, non-call **system** balance changes (spec §(b)/(c)).
//!
//! Some balance deltas a watched address can observe in a block have **no
//! captured CALL/SELFDESTRUCT/CREATE frame** because they are applied by
//! consensus / the protocol, not by EVM call execution:
//!
//! - **L1 beacon-chain withdrawals** — post-Shanghai blocks carry a
//!   withdrawals list; each entry credits `address` with `amount` GWEI.
//! - **L1 post-merge block "reward"** — the block `beneficiary`
//!   (coinbase) is credited the total **priority fee** over the block's
//!   txs (`(effective_gas_price − base_fee) × gas_used`); the base fee is
//!   burned post-EIP-1559. There is NO call frame for this credit.
//! - **OP deposit mints** — an OP deposit transaction can `mint` ETH to
//!   its recipient with no value-CALL frame.
//!
//! The spec (`passbook-spec.md` §(b)/(c)) requires these to be attributed
//! as `kind = system` and to produce **no reconciliation residual** — only
//! a *truly* unexplained delta is the processing-failure/stall case. Each
//! recognised event becomes a [`SystemCredit`]: the pure
//! [`process_block`](crate::exex::process_block) nets it into
//! reconciliation AND records it as a `kind = system` row in
//! `eth_transfers` so it is queryable.
//!
//! The per-chain *extraction* of these credits lives at the chain seam
//! (L1: [`l1_system_credits`] in this crate, reth-ethereum only; OP:
//! `passbook_stack_optimism` deposit-mint extraction, keeping
//! `passbook-core` OP-free). This module owns the neutral type + the L1
//! computation.

use alloy_primitives::{Address, U256};

/// One recognised system balance change for a **watched** address.
///
/// `signed_wei` is the protocol credit (+) / debit (−) in wei.
/// `counterparty` is the system source (e.g. `Address::ZERO` for the
/// beacon-withdrawal / deposit-mint protocol source, or the beneficiary
/// itself for the block-reward credit — purely informational on the row).
/// `source` is a stable short tag used as the `trace_path` of the emitted
/// `kind = system` `eth_transfers` row so different system categories are
/// distinguishable & idempotently keyed within a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemCredit {
    pub address: Address,
    pub signed_wei: i128,
    pub counterparty: Address,
    pub source: String,
}

impl SystemCredit {
    pub fn new(
        address: Address,
        signed_wei: i128,
        counterparty: Address,
        source: impl Into<String>,
    ) -> Self {
        Self {
            address,
            signed_wei,
            counterparty,
            source: source.into(),
        }
    }
}

/// Compute the per-block recognised **L1** system credits for the watched
/// set from a block's withdrawals + the per-tx priority fee to the
/// beneficiary. This is the chain-AGNOSTIC arithmetic; the L1 seam
/// ([`crate::exex::process_committed_block_inner`]) feeds it the already
/// extracted withdrawal list and per-tx `(effective_gas_price, gas_used)`
/// pairs so it stays unit-testable without reth types.
///
/// - **Beacon withdrawals**: for each withdrawal whose recipient ∈
///   `watched`, a `+amount_wei` credit (gwei→wei = amount × 1e9). Pre-pin
///   note: pre-Shanghai blocks have no withdrawals list ⇒ none (forward-
///   only on post-merge networks; pre-merge fixed block rewards are out of
///   scope and intentionally NOT synthesised).
/// - **Beneficiary priority fee**: post-merge there is no fixed block
///   reward — the only protocol credit to `beneficiary` is the sum over
///   included txs of `(effective_gas_price − base_fee_per_gas) ×
///   gas_used`. If `beneficiary ∈ watched`, that total is a `+` credit.
///   (Pre-1559 / missing base fee ⇒ the full `effective_gas_price ×
///   gas_used` is the miner credit; handled by passing `base_fee = 0`.)
pub fn l1_system_credits(
    watched: &std::collections::HashSet<Address>,
    withdrawals: &[(Address, U256)],
    beneficiary: Address,
    base_fee_per_gas: u128,
    txs: &[(u128, u64)], // (effective_gas_price, gas_used) per included tx
) -> Vec<SystemCredit> {
    let mut out: Vec<SystemCredit> = Vec::new();

    // Aggregate withdrawals per recipient (a block can carry several to
    // the same address — one netted row keyed by source="withdrawal").
    let mut wd_total: std::collections::HashMap<Address, U256> = Default::default();
    for (recipient, amount_wei) in withdrawals {
        if watched.contains(recipient) {
            *wd_total.entry(*recipient).or_default() += *amount_wei;
        }
    }
    let mut wd: Vec<(Address, U256)> = wd_total.into_iter().collect();
    wd.sort_by_key(|(a, _)| *a);
    for (recipient, total) in wd {
        let signed = i128::try_from(total).unwrap_or(i128::MAX);
        out.push(SystemCredit::new(
            recipient,
            signed,
            Address::ZERO,
            "withdrawal",
        ));
    }

    // Post-merge beneficiary "reward" = Σ priority fee. Only meaningful
    // when the beneficiary is watched; base fee is burned post-1559.
    if watched.contains(&beneficiary) {
        let mut total: U256 = U256::ZERO;
        for (effective_gas_price, gas_used) in txs {
            let prio = effective_gas_price.saturating_sub(base_fee_per_gas);
            total += U256::from(prio) * U256::from(*gas_used);
        }
        if !total.is_zero() {
            let signed = i128::try_from(total).unwrap_or(i128::MAX);
            out.push(SystemCredit::new(
                beneficiary,
                signed,
                beneficiary,
                "block_reward",
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    #[test]
    fn withdrawal_to_watched_is_a_system_credit() {
        let w = Address::repeat_byte(0xaa);
        let other = Address::repeat_byte(0xbb);
        let watched = [w].into_iter().collect();
        let creds = l1_system_credits(
            &watched,
            &[
                (w, U256::from(1_000_000_000u64)),     // 1 gwei → 1e9 wei
                (other, U256::from(5_000_000_000u64)), // not watched
            ],
            Address::ZERO,
            7,
            &[],
        );
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].address, w);
        assert_eq!(creds[0].signed_wei, 1_000_000_000i128);
        assert_eq!(creds[0].source, "withdrawal");
    }

    #[test]
    fn multiple_withdrawals_same_recipient_net_into_one() {
        let w = Address::repeat_byte(0xaa);
        let watched = [w].into_iter().collect();
        let creds = l1_system_credits(
            &watched,
            &[(w, U256::from(10u64)), (w, U256::from(32u64))],
            Address::ZERO,
            7,
            &[],
        );
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].signed_wei, 42i128);
    }

    #[test]
    fn beneficiary_priority_fee_is_a_system_credit() {
        let w = Address::repeat_byte(0xcc);
        let watched = [w].into_iter().collect();
        // base_fee 7; one tx at 1 gwei eff price, 21000 gas.
        // priority = (1e9 - 7) * 21000.
        let creds = l1_system_credits(&watched, &[], w, 7, &[(1_000_000_000u128, 21_000u64)]);
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].source, "block_reward");
        let expect = (1_000_000_000u128 - 7) as i128 * 21_000i128;
        assert_eq!(creds[0].signed_wei, expect);
    }

    #[test]
    fn unwatched_beneficiary_yields_nothing() {
        let w = Address::repeat_byte(0xcc);
        let watched = [w].into_iter().collect();
        let creds = l1_system_credits(
            &watched,
            &[],
            Address::repeat_byte(0x01), // beneficiary not watched
            7,
            &[(1_000_000_000u128, 21_000u64)],
        );
        assert!(creds.is_empty());
    }

    #[test]
    fn zero_priority_fee_emits_no_block_reward_row() {
        let w = Address::repeat_byte(0xcc);
        let watched = [w].into_iter().collect();
        // effective price == base fee ⇒ zero priority ⇒ no row.
        let creds = l1_system_credits(
            &watched,
            &[],
            w,
            1_000_000_000u128,
            &[(1_000_000_000u128, 21_000u64)],
        );
        assert!(creds.is_empty());
    }
}
