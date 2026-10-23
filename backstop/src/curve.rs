//! Utilization-based premium curve for backstop capital.
//!
//! Utilization is the share of backstop capital currently exposed, in basis
//! points. The curve maps it to a premium multiplier that scales the premium
//! share routed to backstop stakers. Multipliers use 1e7 fixed point, so
//! [`ONE`] (1.0) leaves the premium share unchanged. The curve is piecewise
//! linear with one kink, and the result is clamped to governance-set bounds.
//! The slopes are never negative, so the multiplier never decreases as
//! utilization rises.

use soroban_sdk::contracttype;

/// Fixed-point scale for multipliers. `ONE` means a multiplier of 1.0.
pub const ONE: i128 = 10_000_000;

/// Utilization denominator: 10_000 basis points is full utilization.
pub const MAX_UTILIZATION_BPS: i128 = 10_000;

/// Largest multiplier a curve may use, 10.0x. Keeps a misconfigured curve
/// from routing more than ten times the configured premium share.
pub const MAX_MULTIPLIER: i128 = 10 * ONE;

/// Governance-set parameters for the premium curve.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PremiumCurve {
    /// Utilization, in basis points, where the slope changes from `slope_low`
    /// to `slope_high`.
    pub kink_bps: i128,
    /// Multiplier at zero utilization, in 1e7 fixed point.
    pub base: i128,
    /// Multiplier increase per basis point below the kink, in 1e7 fixed point.
    pub slope_low: i128,
    /// Multiplier increase per basis point above the kink, in 1e7 fixed point.
    pub slope_high: i128,
    /// Lower clamp on the multiplier, in 1e7 fixed point.
    pub min_multiplier: i128,
    /// Upper clamp on the multiplier, in 1e7 fixed point.
    pub max_multiplier: i128,
}

impl PremiumCurve {
    /// A curve that always returns 1.0, so the premium share is unchanged.
    pub fn flat() -> Self {
        Self {
            kink_bps: 0,
            base: ONE,
            slope_low: 0,
            slope_high: 0,
            min_multiplier: ONE,
            max_multiplier: ONE,
        }
    }

    /// Whether the parameters are inside the hard bounds.
    pub fn is_valid(&self) -> bool {
        (0..=MAX_UTILIZATION_BPS).contains(&self.kink_bps)
            && (0..=ONE).contains(&self.slope_low)
            && (0..=ONE).contains(&self.slope_high)
            && self.base >= 0
            && (0..=MAX_MULTIPLIER).contains(&self.min_multiplier)
            && (0..=MAX_MULTIPLIER).contains(&self.max_multiplier)
            && self.min_multiplier <= self.max_multiplier
    }

    /// Premium multiplier at `utilization_bps`, in 1e7 fixed point. Utilization
    /// is clamped to `[0, MAX_UTILIZATION_BPS]`, and the result is clamped to
    /// `[min_multiplier, max_multiplier]`.
    pub fn multiplier(&self, utilization_bps: i128) -> i128 {
        let utilization = utilization_bps.clamp(0, MAX_UTILIZATION_BPS);
        let below_kink = utilization.min(self.kink_bps);
        let above_kink = (utilization - self.kink_bps).max(0);
        let raw = self.base + self.slope_low * below_kink + self.slope_high * above_kink;
        raw.clamp(self.min_multiplier, self.max_multiplier)
    }
}

/// Basis-point denominator for the premium share.
const BPS_DENOM: i128 = 10_000;

/// The part of `amount` routed to stakers: `amount * share_bps * multiplier`,
/// with `share_bps` in basis points and `multiplier` in 1e7 fixed point. The
/// result is never more than `amount`. Returns `None` on overflow.
pub fn premium_cut(amount: i128, share_bps: i128, multiplier: i128) -> Option<i128> {
    amount
        .checked_mul(share_bps)?
        .checked_mul(multiplier)?
        .checked_div(BPS_DENOM * ONE)
        .map(|cut| cut.min(amount))
}

/// Utilization in basis points: `exposure / total_capital`, capped at full.
/// With no capital, any exposure counts as full utilization.
pub fn utilization_bps(exposure: i128, total_capital: i128) -> i128 {
    if exposure <= 0 {
        return 0;
    }
    if total_capital <= 0 {
        return MAX_UTILIZATION_BPS;
    }
    exposure
        .saturating_mul(MAX_UTILIZATION_BPS)
        .checked_div(total_capital)
        .unwrap_or(MAX_UTILIZATION_BPS)
        .min(MAX_UTILIZATION_BPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PremiumCurve {
        PremiumCurve {
            kink_bps: 5_000,
            base: ONE,
            slope_low: 10_000,
            slope_high: 40_000,
            min_multiplier: ONE,
            max_multiplier: MAX_MULTIPLIER,
        }
    }

    #[test]
    fn flat_curve_keeps_premium_share() {
        let curve = PremiumCurve::flat();
        assert!(curve.is_valid());
        for u in 0..=MAX_UTILIZATION_BPS {
            assert_eq!(curve.multiplier(u), ONE);
        }
    }

    #[test]
    fn multiplier_follows_both_slopes() {
        let curve = sample();
        assert_eq!(curve.multiplier(0), ONE);
        assert_eq!(curve.multiplier(5_000), ONE + 5_000 * 10_000);
        assert_eq!(curve.multiplier(6_000), ONE + 5_000 * 10_000 + 1_000 * 40_000);
    }

    #[test]
    fn multiplier_is_monotone_and_bounded() {
        let curve = sample();
        let mut previous = curve.multiplier(0);
        for u in 0..=MAX_UTILIZATION_BPS {
            let m = curve.multiplier(u);
            assert!(m >= previous, "decreased at utilization {u}");
            assert!(m >= curve.min_multiplier && m <= curve.max_multiplier);
            previous = m;
        }
    }

    #[test]
    fn utilization_is_clamped_to_full() {
        let curve = sample();
        assert_eq!(curve.multiplier(-1), curve.multiplier(0));
        assert_eq!(
            curve.multiplier(MAX_UTILIZATION_BPS + 1),
            curve.multiplier(MAX_UTILIZATION_BPS)
        );
    }

    #[test]
    fn rejects_out_of_bound_parameters() {
        let mut curve = sample();
        curve.slope_low = -1;
        assert!(!curve.is_valid());

        let mut curve = sample();
        curve.min_multiplier = 2 * ONE;
        curve.max_multiplier = ONE;
        assert!(!curve.is_valid());

        let mut curve = sample();
        curve.kink_bps = MAX_UTILIZATION_BPS + 1;
        assert!(!curve.is_valid());

        let mut curve = sample();
        curve.max_multiplier = MAX_MULTIPLIER + 1;
        assert!(!curve.is_valid());
    }

    #[test]
    fn premium_cut_never_exceeds_amount() {
        // Flat curve at 100% share: the whole premium is routed.
        assert_eq!(premium_cut(100, 10_000, ONE), Some(100));
        // Flat curve at 25% share.
        assert_eq!(premium_cut(100, 2_500, ONE), Some(25));
        // A 10x multiplier on a 100% share would route 1000, so it is clamped.
        assert_eq!(premium_cut(100, 10_000, MAX_MULTIPLIER), Some(100));
        // Overflow is reported, not wrapped.
        assert_eq!(premium_cut(i128::MAX, 10_000, ONE), None);
    }

    #[test]
    fn utilization_from_exposure() {
        assert_eq!(utilization_bps(0, 1_000), 0);
        assert_eq!(utilization_bps(500, 1_000), 5_000);
        assert_eq!(utilization_bps(1_000, 1_000), MAX_UTILIZATION_BPS);
        assert_eq!(utilization_bps(2_000, 1_000), MAX_UTILIZATION_BPS);
        assert_eq!(utilization_bps(1, 0), MAX_UTILIZATION_BPS);
        assert_eq!(utilization_bps(0, 0), 0);
    }
}
