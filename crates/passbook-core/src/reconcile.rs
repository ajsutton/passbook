use crate::model::{UnattributedDeltaCause, UnattributedDeltaRow};
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

/// A reconciliation input could not be represented in `i128`, or the
/// attribution sum overflowed `i128`. This is NOT a balanced account: an
/// out-of-range magnitude means we cannot prove the address reconciles, so
/// the caller MUST treat it as a processing failure exactly like an
/// unexplained residual (do not advance / do not emit FinishedHeight),
/// never as a silently-clamped "balanced" result.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("reconcile arithmetic out of i128 range")]
pub struct ReconcileOverflow;

/// attributed = eth_in - eth_out - gas_paid + system_signed.
///
/// `Ok(None)` iff the address provably balances (`observed - attributed == 0`).
/// `Ok(Some(row))` iff `|observed - attributed| != 0` — a genuine residual.
/// `Err(ReconcileOverflow)` iff any input exceeds `i128` or the sum overflows;
/// previously this path silently clamped to `i128::MAX`, which could mask a
/// genuine discrepancy by collapsing an out-of-range value into an apparently
/// balanced (or wrongly-sized) residual. We now refuse to clamp.
///
/// A returned row OR an error means the caller MUST treat the block as a
/// processing failure and persist a diagnostic; only `Ok(None)` may advance.
pub fn reconcile_account(
    i: ReconcileInput,
) -> Result<Option<UnattributedDeltaRow>, ReconcileOverflow> {
    let to_i = |u: U256| -> Result<i128, ReconcileOverflow> {
        i128::try_from(u).map_err(|_| ReconcileOverflow)
    };
    let eth_in = to_i(i.eth_in)?;
    let eth_out = to_i(i.eth_out)?;
    let gas_paid = to_i(i.gas_paid)?;
    let attributed: i128 = eth_in
        .checked_sub(eth_out)
        .and_then(|v| v.checked_sub(gas_paid))
        .and_then(|v| v.checked_add(i.system_signed))
        .ok_or(ReconcileOverflow)?;
    let residual = i
        .observed_delta
        .checked_sub(attributed)
        .ok_or(ReconcileOverflow)?;
    if residual == 0 {
        return Ok(None);
    }
    Ok(Some(UnattributedDeltaRow {
        chain_id: i.chain_id,
        block_number: i.block_number,
        block_hash: i.block_hash,
        address: i.address,
        observed_wei: U256::from(i.observed_delta.unsigned_abs()),
        attributed_wei: U256::from(attributed.unsigned_abs()),
        residual_wei: U256::from(residual.unsigned_abs()),
        // Live-mode reconcile residual: the ExEx stalls on this block.
        // Skip-path markers are constructed at the skip-path call site
        // and tagged `ParentStateUnavailable` there.
        cause: UnattributedDeltaCause::UnexplainedResidual,
    }))
}

/// Pure helper for the skip-mode partial batch (issue #15): given the
/// observed signed BundleState delta for a watched address and the net
/// recognised `system_signed` for that same address, return
/// `(observed_wei, attributed_wei, residual_wei)` for the
/// `unattributed_deltas` marker.
///
/// Sign handling — explicit, not naive subtraction:
/// - If observed and system_signed agree in direction (or system is
///   zero), the system credit attributes some of the delta; the residual
///   is the saturating magnitude gap (system over-attribution clamps to
///   zero residual).
/// - If they disagree in direction, the system credit does NOT attribute
///   the observed delta (e.g. observed inflow but the only recognised
///   system event is a debit); the full observed magnitude is the
///   residual and `attributed_wei` is zero.
///
/// `residual_wei == 0` means the system credit covered the observed
/// delta; callers SHOULD skip emitting a marker in that case.
pub fn skip_mode_attribution(observed_signed: i128, system_signed: i128) -> (u128, u128, u128) {
    let observed_wei = observed_signed.unsigned_abs();
    let signs_agree = system_signed == 0 || observed_signed.signum() == system_signed.signum();
    if !signs_agree {
        return (observed_wei, 0, observed_wei);
    }
    let system_mag = system_signed.unsigned_abs();
    // System fully or over-attributes ⇒ residual is zero. The attributed
    // amount is clamped to the observed magnitude so the marker (if
    // emitted) never claims more attribution than there was observed.
    let attributed_wei = system_mag.min(observed_wei);
    let residual_wei = observed_wei - attributed_wei;
    (observed_wei, attributed_wei, residual_wei)
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
        })
        .expect("in-range inputs must not overflow");
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
        .expect("in-range inputs must not overflow")
        .expect("imbalance must yield a row");
        assert_eq!(r.residual_wei, U256::from(90)); // |100 - 10|
    }

    /// A value exceeding `i128::MAX` must NOT be silently clamped to
    /// `i128::MAX` (which could collapse a genuine discrepancy into an
    /// apparently balanced or wrongly-sized residual). It must surface as a
    /// hard `ReconcileOverflow` so the caller halts the block (issue #10).
    #[test]
    fn out_of_range_input_is_a_hard_error_not_a_clamp() {
        let addr = Address::repeat_byte(0xbb);
        let huge = U256::from(i128::MAX) + U256::from(1u8); // i128::MAX + 1
        let err = reconcile_account(ReconcileInput {
            chain_id: 1,
            block_number: 7,
            block_hash: B256::ZERO,
            address: addr,
            observed_delta: 0i128,
            eth_in: huge,
            eth_out: U256::ZERO,
            gas_paid: U256::ZERO,
            system_signed: 0i128,
        });
        // The old clamp would have made attributed == i128::MAX and, with
        // observed_delta 0, wrapped/panicked on the subtraction or produced
        // a bogus residual — never an honest overflow. It must now be a
        // hard error, and crucially NOT a (silently-clamped) `Ok(None)`.
        assert!(
            matches!(err, Err(ReconcileOverflow)),
            "out-of-range input must be a hard error, not a silent clamp"
        );
    }

    /// Issue #15: skip-mode helper for a deposit-mint-funded watched
    /// block. The system credit and observed delta agree in direction
    /// and magnitude — residual is zero, marker should be SKIPPED.
    #[test]
    fn skip_mode_full_system_attribution_yields_zero_residual() {
        let (obs, attr, res) =
            skip_mode_attribution(89_916_500_000_000_000, 89_916_500_000_000_000);
        assert_eq!(obs, 89_916_500_000_000_000);
        assert_eq!(attr, 89_916_500_000_000_000);
        assert_eq!(res, 0);
    }

    /// Issue #15: partial same-sign attribution — residual is the
    /// magnitude gap (`observed − attributed`), not the full observed.
    #[test]
    fn skip_mode_partial_same_sign_attribution_records_gap() {
        let (obs, attr, res) = skip_mode_attribution(1_000, 300);
        assert_eq!(obs, 1_000);
        assert_eq!(attr, 300);
        assert_eq!(res, 700);
    }

    /// Issue #15: same-sign over-attribution (system claims MORE than
    /// observed) clamps `attributed` to the observed magnitude so the
    /// row never reports more attribution than there was observed —
    /// residual saturates at zero.
    #[test]
    fn skip_mode_over_attribution_clamps_to_zero_residual() {
        let (obs, attr, res) = skip_mode_attribution(500, 700);
        assert_eq!(obs, 500);
        assert_eq!(attr, 500, "attributed clamps to observed magnitude");
        assert_eq!(res, 0);
    }

    /// Issue #15: sign-disagreement — the system credit does NOT
    /// attribute the observed delta; `attributed = 0` and the full
    /// observed is residual.
    #[test]
    fn skip_mode_sign_disagreement_yields_full_observed_residual() {
        let (obs, attr, res) = skip_mode_attribution(500, -200);
        assert_eq!(obs, 500);
        assert_eq!(attr, 0);
        assert_eq!(res, 500);
        // And the reverse — observed debit, system credits inflow.
        let (obs, attr, res) = skip_mode_attribution(-500, 200);
        assert_eq!(obs, 500);
        assert_eq!(attr, 0);
        assert_eq!(res, 500);
    }

    /// Issue #15: no system credit ⇒ no attribution ⇒ full observed
    /// becomes residual (matches the pre-fix skip-path behaviour for
    /// blocks with no recognised system event).
    #[test]
    fn skip_mode_no_system_credit_keeps_full_observed() {
        let (obs, attr, res) = skip_mode_attribution(500, 0);
        assert_eq!(obs, 500);
        assert_eq!(attr, 0);
        assert_eq!(res, 500);
    }

    /// The attribution sum itself overflowing `i128` (each input in range,
    /// but `system_signed` pushes the running total past `i128::MAX`) must
    /// also be a hard error rather than a saturating mask.
    #[test]
    fn attribution_sum_overflow_is_a_hard_error() {
        let addr = Address::repeat_byte(0xcc);
        let err = reconcile_account(ReconcileInput {
            chain_id: 1,
            block_number: 8,
            block_hash: B256::ZERO,
            address: addr,
            observed_delta: 0i128,
            eth_in: U256::from(i128::MAX),
            eth_out: U256::ZERO,
            gas_paid: U256::ZERO,
            system_signed: i128::MAX, // i128::MAX + i128::MAX overflows
        });
        assert!(matches!(err, Err(ReconcileOverflow)));
    }
}
