//! # `refract-common` — Shared 1e7 Fixed-Point Checked Arithmetic
//!
//! All monetary, token, share, and coverage quantities across Refract Protocol
//! are represented as signed 128-bit integers (`i128`) scaled by **`1e7`
//! (`10_000_000`)**, matching Stellar's 7-decimal stroop convention. Percentage
//! splits and fee rates are expressed in **basis points (`bps`)** out of
//! **`10_000`** (`1 bps = 0.01%`).
//!
//! This `#![no_std]` crate provides pure, storage-free arithmetic primitives
//! for the governance-adjacent contracts (`RefractBackstop`,
//! `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury`) so that every
//! contract enforces an identical overflow and rounding policy.
//!
//! ## Normative Rounding Policy
//!
//! 1. **Rounding must never create unbacked value**: Share minting on deposit,
//!    underlying redemption on withdrawal, and staker/grantee payouts always
//!    round **down (`Rounding::Floor`)** so the sum of claims never exceeds
//!    actual token balances held in custody.
//! 2. **Splits round in favour of solvency**: Every fractional computation
//!    requires an explicit [`Rounding`] argument (`Floor` or `Ceil`) rather
//!    than silently truncating. Callers routing protocol obligations or
//!    required collateral use [`Rounding::Ceil`]; callers disbursing rewards,
//!    shares, or fee slices out of a pool use [`Rounding::Floor`].
//! 3. **Explicit errors over panics**: Every fallible helper returns
//!    `Result<_, MathError>` instead of panicking on overflow, negative inputs,
//!    zero divisors, or out-of-range basis points.

#![no_std]
#![allow(clippy::inconsistent_digit_grouping)]

/// Fixed-point scaling factor (`1e7 = 10_000_000`, 7 decimal places).
pub const PRECISION: i128 = 10_000_000;

/// Basis-point denominator (`10_000 bps = 100%`) as `u32`.
pub const BPS_DENOM: u32 = 10_000;

/// Basis-point denominator (`10_000 bps = 100%`) as `i128`.
pub const BPS_DENOM_I128: i128 = 10_000;

/// Explicit rounding direction for fixed-point division and basis-point splits.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Rounding {
    /// Round toward zero / negative infinity for non-negative values (`floor(a * b / d)`).
    ///
    /// Use when minting shares, redeeming pool capital, or disbursing rewards so
    /// rounding never creates unbacked claims.
    Floor,
    /// Round away from zero / toward positive infinity for non-negative values (`ceil(a * b / d)`).
    ///
    /// Use when computing required collateral, minimum reserves, or protocol obligations.
    Ceil,
}

/// Arithmetic errors returned by [`refract_common`](crate) operations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MathError {
    /// Intermediate or final value overflowed or underflowed `i128` (or target integer bounds).
    Overflow,
    /// Attempted to divide by zero.
    DivisionByZero,
    /// Basis-point parameter exceeded [`BPS_DENOM`] (`10_000`).
    InvalidBps,
    /// Input amount was negative (or non-positive where strictly positive is required).
    NegativeAmount,
    /// Denominator was strictly negative.
    InvalidDenominator,
}

/// Checked addition of two `i128` values.
#[inline]
pub const fn checked_add(a: i128, b: i128) -> Result<i128, MathError> {
    match a.checked_add(b) {
        Some(v) => Ok(v),
        None => Err(MathError::Overflow),
    }
}

/// Checked subtraction of two `i128` values.
#[inline]
pub const fn checked_sub(a: i128, b: i128) -> Result<i128, MathError> {
    match a.checked_sub(b) {
        Some(v) => Ok(v),
        None => Err(MathError::Overflow),
    }
}

/// Checked multiplication of two `i128` values.
#[inline]
pub const fn checked_mul(a: i128, b: i128) -> Result<i128, MathError> {
    match a.checked_mul(b) {
        Some(v) => Ok(v),
        None => Err(MathError::Overflow),
    }
}

/// Checked subtraction over non-negative balances (`a - b >= 0`).
///
/// Rejects negative inputs with [`MathError::NegativeAmount`] and rejects
/// `b > a` with [`MathError::Overflow`].
#[inline]
pub const fn checked_sub_non_negative(a: i128, b: i128) -> Result<i128, MathError> {
    if a < 0 || b < 0 {
        return Err(MathError::NegativeAmount);
    }
    if b > a {
        return Err(MathError::Overflow);
    }
    Ok(a - b)
}

/// Saturating addition of two `i128` values.
#[inline]
pub const fn saturating_add(a: i128, b: i128) -> i128 {
    a.saturating_add(b)
}

/// Saturating subtraction of two `i128` values.
#[inline]
pub const fn saturating_sub(a: i128, b: i128) -> i128 {
    a.saturating_sub(b)
}

/// Saturating subtraction floored at zero (`max(a - b, 0)`) for non-negative inputs.
///
/// Rejects negative `a` or `b` with [`MathError::NegativeAmount`].
#[inline]
pub const fn saturating_sub_floor_zero(a: i128, b: i128) -> Result<i128, MathError> {
    if a < 0 || b < 0 {
        return Err(MathError::NegativeAmount);
    }
    if b >= a {
        Ok(0)
    } else {
        Ok(a - b)
    }
}

/// Computes `(a * b) / denom` for non-negative `a, b` and strictly positive `denom`
/// using the specified [`Rounding`] policy.
#[inline]
pub const fn mul_div(a: i128, b: i128, denom: i128, rounding: Rounding) -> Result<i128, MathError> {
    if denom == 0 {
        return Err(MathError::DivisionByZero);
    }
    if denom < 0 {
        return Err(MathError::InvalidDenominator);
    }
    if a < 0 || b < 0 {
        return Err(MathError::NegativeAmount);
    }
    let prod = match a.checked_mul(b) {
        Some(p) => p,
        None => return Err(MathError::Overflow),
    };
    let quot = prod / denom;
    let rem = prod % denom;
    match rounding {
        Rounding::Floor => Ok(quot),
        Rounding::Ceil => {
            if rem > 0 {
                match quot.checked_add(1) {
                    Some(q) => Ok(q),
                    None => Err(MathError::Overflow),
                }
            } else {
                Ok(quot)
            }
        }
    }
}

/// Computes `floor((a * b) / denom)` for non-negative `a, b` and strictly positive `denom`.
#[inline]
pub const fn mul_div_floor(a: i128, b: i128, denom: i128) -> Result<i128, MathError> {
    mul_div(a, b, denom, Rounding::Floor)
}

/// Computes `ceil((a * b) / denom)` for non-negative `a, b` and strictly positive `denom`.
#[inline]
pub const fn mul_div_ceil(a: i128, b: i128, denom: i128) -> Result<i128, MathError> {
    mul_div(a, b, denom, Rounding::Ceil)
}

/// Saturating variant of [`mul_div`] that clamps `a * b` to `i128::MAX` on multiplication
/// overflow while still rejecting negative inputs and non-positive denominators.
#[inline]
pub const fn saturating_mul_div(
    a: i128,
    b: i128,
    denom: i128,
    rounding: Rounding,
) -> Result<i128, MathError> {
    if denom == 0 {
        return Err(MathError::DivisionByZero);
    }
    if denom < 0 {
        return Err(MathError::InvalidDenominator);
    }
    if a < 0 || b < 0 {
        return Err(MathError::NegativeAmount);
    }
    let prod = a.saturating_mul(b);
    let quot = prod / denom;
    let rem = prod % denom;
    match rounding {
        Rounding::Floor => Ok(quot),
        Rounding::Ceil => {
            if rem > 0 {
                Ok(quot.saturating_add(1))
            } else {
                Ok(quot)
            }
        }
    }
}

/// Applies a basis-point share (`bps / 10_000`) to `amount` using the given [`Rounding`] policy.
///
/// Rejects `bps > 10_000` with [`MathError::InvalidBps`] and `amount < 0` with
/// [`MathError::NegativeAmount`].
#[inline]
pub const fn apply_bps(amount: i128, bps: u32, rounding: Rounding) -> Result<i128, MathError> {
    if bps > BPS_DENOM {
        return Err(MathError::InvalidBps);
    }
    mul_div(amount, bps as i128, BPS_DENOM_I128, rounding)
}

/// Computes the ratio `(numerator / denominator)` expressed in basis points (`0..=u32::MAX`)
/// using the specified [`Rounding`] direction.
///
/// Rejects `numerator < 0`, `denominator <= 0`, and ratios that exceed `u32::MAX` bps.
#[inline]
pub const fn ratio_bps(
    numerator: i128,
    denominator: i128,
    rounding: Rounding,
) -> Result<u32, MathError> {
    let raw = match mul_div(numerator, BPS_DENOM_I128, denominator, rounding) {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    if raw > (u32::MAX as i128) {
        return Err(MathError::Overflow);
    }
    Ok(raw as u32)
}

/// Converts a non-negative whole-unit integer into a `1e7` fixed-point amount.
#[inline]
pub const fn to_1e7(whole_units: i128) -> Result<i128, MathError> {
    if whole_units < 0 {
        return Err(MathError::NegativeAmount);
    }
    match whole_units.checked_mul(PRECISION) {
        Some(v) => Ok(v),
        None => Err(MathError::Overflow),
    }
}

/// Converts a non-negative `1e7` fixed-point amount into whole units using the
/// specified [`Rounding`] direction (`Floor` truncates fractional stroops; `Ceil`
/// rounds up any non-zero fractional stroop).
#[inline]
pub const fn from_1e7(scaled_amount: i128, rounding: Rounding) -> Result<i128, MathError> {
    mul_div(scaled_amount, 1, PRECISION, rounding)
}

/// Computes shares to mint for a deposit of `amount` given `total_shares` and `total_capital`.
///
/// First deposit (`total_shares == 0 || total_capital == 0`) mints shares 1:1.
/// Subsequent deposits round down (`Rounding::Floor`) so depositors cannot extract
/// unbacked share dilution.
#[inline]
pub const fn calc_shares_for_deposit(
    amount: i128,
    total_shares: i128,
    total_capital: i128,
) -> Result<i128, MathError> {
    if amount <= 0 || total_shares < 0 || total_capital < 0 {
        return Err(MathError::NegativeAmount);
    }
    if total_shares == 0 || total_capital == 0 {
        Ok(amount)
    } else {
        mul_div_floor(amount, total_shares, total_capital)
    }
}

/// Computes underlying capital redeemable for burning `shares` given `total_shares`
/// and `total_capital`.
///
/// Rounds down (`Rounding::Floor`) so redemptions never exceed pool solvency.
#[inline]
pub const fn quote_capital_for_shares(
    shares: i128,
    total_shares: i128,
    total_capital: i128,
) -> Result<i128, MathError> {
    if shares <= 0 || total_shares < 0 || total_capital < 0 {
        return Err(MathError::NegativeAmount);
    }
    if total_shares == 0 {
        Ok(0)
    } else {
        mul_div_floor(shares, total_capital, total_shares)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
mod test {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn constants_match_1e7_and_bps_conventions() {
        assert_eq!(PRECISION, 10_000_000);
        assert_eq!(BPS_DENOM, 10_000);
        assert_eq!(BPS_DENOM_I128, 10_000);
    }

    #[test]
    fn checked_arithmetic_handles_bounds_and_errors() {
        assert_eq!(checked_add(100, 250), Ok(350));
        assert_eq!(checked_add(i128::MAX, 1), Err(MathError::Overflow));

        assert_eq!(checked_sub(250, 100), Ok(150));
        assert_eq!(checked_sub(i128::MIN, 1), Err(MathError::Overflow));

        assert_eq!(checked_mul(1_000, 10_000_000), Ok(10_000_000_000));
        assert_eq!(checked_mul(i128::MAX, 2), Err(MathError::Overflow));

        assert_eq!(checked_sub_non_negative(500, 200), Ok(300));
        assert_eq!(checked_sub_non_negative(500, 500), Ok(0));
        assert_eq!(checked_sub_non_negative(200, 500), Err(MathError::Overflow));
        assert_eq!(
            checked_sub_non_negative(-1, 10),
            Err(MathError::NegativeAmount)
        );
        assert_eq!(
            checked_sub_non_negative(10, -1),
            Err(MathError::NegativeAmount)
        );
    }

    #[test]
    fn saturating_helpers_clamp_without_panicking() {
        assert_eq!(saturating_add(i128::MAX, 100), i128::MAX);
        assert_eq!(saturating_sub(i128::MIN, 100), i128::MIN);

        assert_eq!(saturating_sub_floor_zero(500, 200), Ok(300));
        assert_eq!(saturating_sub_floor_zero(200, 500), Ok(0));
        assert_eq!(
            saturating_sub_floor_zero(-5, 10),
            Err(MathError::NegativeAmount)
        );
        assert_eq!(
            saturating_sub_floor_zero(10, -5),
            Err(MathError::NegativeAmount)
        );
    }

    #[test]
    fn mul_div_floor_and_ceil_rounding_and_error_paths() {
        // Exact division: floor == ceil
        assert_eq!(mul_div_floor(100, 30, 10), Ok(300));
        assert_eq!(mul_div_ceil(100, 30, 10), Ok(300));

        // Non-exact division: floor and ceil differ by 1
        assert_eq!(mul_div_floor(10, 1, 3), Ok(3));
        assert_eq!(mul_div_ceil(10, 1, 3), Ok(4));

        // Zero numerator
        assert_eq!(mul_div_floor(0, 999, 7), Ok(0));
        assert_eq!(mul_div_ceil(0, 999, 7), Ok(0));

        // Error cases
        assert_eq!(mul_div_floor(10, 5, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul_div_ceil(10, 5, -3), Err(MathError::InvalidDenominator));
        assert_eq!(mul_div_floor(-1, 5, 3), Err(MathError::NegativeAmount));
        assert_eq!(mul_div_floor(5, -1, 3), Err(MathError::NegativeAmount));
        assert_eq!(mul_div_floor(i128::MAX, 2, 3), Err(MathError::Overflow));
        assert_eq!(mul_div_ceil(i128::MAX, 1, 1), Ok(i128::MAX));
    }

    #[test]
    fn saturating_mul_div_clamps_product_and_checks_inputs() {
        assert_eq!(saturating_mul_div(100, 3, 10, Rounding::Floor), Ok(30));
        assert_eq!(saturating_mul_div(10, 1, 3, Rounding::Ceil), Ok(4));
        assert_eq!(
            saturating_mul_div(i128::MAX, 2, 2, Rounding::Floor),
            Ok(i128::MAX / 2)
        );
        assert_eq!(
            saturating_mul_div(10, 1, 0, Rounding::Floor),
            Err(MathError::DivisionByZero)
        );
        assert_eq!(
            saturating_mul_div(10, 1, -2, Rounding::Floor),
            Err(MathError::InvalidDenominator)
        );
        assert_eq!(
            saturating_mul_div(-10, 1, 2, Rounding::Floor),
            Err(MathError::NegativeAmount)
        );
        assert_eq!(
            saturating_mul_div(10, -1, 2, Rounding::Floor),
            Err(MathError::NegativeAmount)
        );
    }

    #[test]
    fn apply_bps_and_ratio_bps_enforce_bounds_and_rounding() {
        // 15% (1_500 bps) of 1_000_0000000 = 150_0000000
        assert_eq!(
            apply_bps(1_000_0000000, 1_500, Rounding::Floor),
            Ok(150_0000000)
        );
        // Odd amount split: 333 bps of 101 -> 101 * 333 / 10_000 = 33633 / 10_000 = 3.3633
        let floor_cut = apply_bps(101, 333, Rounding::Floor).unwrap();
        let ceil_cut = apply_bps(101, 333, Rounding::Ceil).unwrap();
        assert_eq!(floor_cut, 3);
        assert_eq!(ceil_cut, 4);

        // Bounds
        assert_eq!(apply_bps(500, 0, Rounding::Floor), Ok(0));
        assert_eq!(apply_bps(500, BPS_DENOM, Rounding::Floor), Ok(500));
        assert_eq!(
            apply_bps(500, BPS_DENOM + 1, Rounding::Floor),
            Err(MathError::InvalidBps)
        );
        assert_eq!(
            apply_bps(-1, 500, Rounding::Floor),
            Err(MathError::NegativeAmount)
        );

        // ratio_bps
        assert_eq!(ratio_bps(250, 1_000, Rounding::Floor), Ok(2_500));
        assert_eq!(ratio_bps(1, 3, Rounding::Floor), Ok(3_333));
        assert_eq!(ratio_bps(1, 3, Rounding::Ceil), Ok(3_334));
        assert_eq!(
            ratio_bps(1, 0, Rounding::Floor),
            Err(MathError::DivisionByZero)
        );
        assert_eq!(
            ratio_bps(-1, 100, Rounding::Floor),
            Err(MathError::NegativeAmount)
        );
        assert_eq!(
            ratio_bps(1_000_000, 1, Rounding::Floor),
            Err(MathError::Overflow)
        );
    }

    #[test]
    fn to_1e7_and_from_1e7_conversions() {
        assert_eq!(to_1e7(0), Ok(0));
        assert_eq!(to_1e7(42), Ok(420_000_000));
        assert_eq!(to_1e7(-1), Err(MathError::NegativeAmount));
        assert_eq!(to_1e7(i128::MAX), Err(MathError::Overflow));

        assert_eq!(from_1e7(420_000_000, Rounding::Floor), Ok(42));
        assert_eq!(from_1e7(420_000_001, Rounding::Floor), Ok(42));
        assert_eq!(from_1e7(420_000_001, Rounding::Ceil), Ok(43));
        assert_eq!(
            from_1e7(-1, Rounding::Floor),
            Err(MathError::NegativeAmount)
        );
    }

    #[test]
    fn share_and_capital_helpers_preserve_solvency() {
        // Initial deposit 1:1
        assert_eq!(
            calc_shares_for_deposit(1_000_0000000, 0, 0),
            Ok(1_000_0000000)
        );
        // Proportional deposit after premium accrual (capital = 1_200, shares = 1_000)
        assert_eq!(calc_shares_for_deposit(600, 1_000, 1_200), Ok(500));
        assert_eq!(
            calc_shares_for_deposit(0, 1_000, 1_200),
            Err(MathError::NegativeAmount)
        );

        // Redemption quote
        assert_eq!(quote_capital_for_shares(500, 1_500, 1_800), Ok(600));
        assert_eq!(quote_capital_for_shares(500, 0, 0), Ok(0));
        assert_eq!(
            quote_capital_for_shares(0, 1_000, 1_000),
            Err(MathError::NegativeAmount)
        );
    }

    proptest! {
        #[test]
        fn prop_mul_div_identity_and_floor_ceil_gap(
            a in 0i128..=1_000_000_000_000_000i128,
            b in 1i128..=1_000_000_000_000_000i128,
            d in 1i128..=1_000_000_000_000_000i128,
        ) {
            // Identity: mul_div(a, b, b) == a
            prop_assert_eq!(mul_div_floor(a, b, b).unwrap(), a);
            prop_assert_eq!(mul_div_ceil(a, b, b).unwrap(), a);

            // Floor and ceil differ by at most 1 unit
            let fl = mul_div_floor(a, b, d).unwrap();
            let cl = mul_div_ceil(a, b, d).unwrap();
            prop_assert!(cl >= fl);
            prop_assert!(cl - fl <= 1);
        }

        #[test]
        fn prop_apply_bps_conservation(
            amount in 0i128..=1_000_000_000_000_000i128,
            bps in 0u32..=BPS_DENOM,
        ) {
            let share_floor = apply_bps(amount, bps, Rounding::Floor).unwrap();
            let comp_floor = apply_bps(amount, BPS_DENOM - bps, Rounding::Floor).unwrap();
            let comp_ceil = apply_bps(amount, BPS_DENOM - bps, Rounding::Ceil).unwrap();

            // Rounding::Floor never creates value across a split
            prop_assert!(share_floor + comp_floor <= amount);
            // Floor share + Ceil complement equals exact `amount`
            prop_assert_eq!(share_floor + comp_ceil, amount);
        }

        #[test]
        fn prop_to_from_1e7_roundtrip(
            whole in 0i128..=1_000_000_000_000i128,
            frac in 0i128..PRECISION,
        ) {
            let scaled = to_1e7(whole).unwrap() + frac;
            let fl = from_1e7(scaled, Rounding::Floor).unwrap();
            let cl = from_1e7(scaled, Rounding::Ceil).unwrap();
            prop_assert_eq!(fl, whole);
            if frac == 0 {
                prop_assert_eq!(cl, whole);
            } else {
                prop_assert_eq!(cl, whole + 1);
            }
        }
    }
}
