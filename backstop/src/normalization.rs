//! Asset Decimal Normalization Module
//!
//! Enforces canonical 1e7 fixed-point representation across Refract Protocol.
//! Translates between external asset decimals (e.g. 6 for USDC, 7 for XLM, 18 for WETH)
//! and internal protocol precision, applying strict directional rounding:
//! - Inflow (deposits/premiums): Ceil rounding protects protocol from underpayment.
//! - Outflow (payouts/withdrawals): Floor rounding protects protocol from dust overpayment.

use soroban_sdk::contracterror;

/// Canonical protocol decimals (1e7, fixed-point precision of Refract Protocol).
pub const PROTOCOL_DECIMALS: u32 = 7;

/// Canonical protocol unit scale (10^7 = 10_000_000).
pub const PROTOCOL_SCALE: i128 = 10_000_000;

/// Maximum supported asset decimals (e.g. 18 for WETH / ERC-20 standard).
pub const MAX_SUPPORTED_DECIMALS: u32 = 18;

/// Precomputed powers of 10 from 10^0 up to 10^18.
pub const POW10: [i128; 19] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
];

/// Directional rounding policy for asset decimal normalization.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Rounding {
    /// Round down (towards zero).
    /// Used for payouts and withdrawals (outflows) to prevent protocol overpayment.
    Floor,
    /// Round up (towards +infinity).
    /// Used for deposits and premiums (inflows) to prevent protocol undercharging.
    Ceil,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum NormalizationError {
    InvalidAmount = 1,
    UnsupportedDecimals = 2,
    MathOverflow = 3,
}

/// Returns 10^decimals for decimals in 0..=18.
pub fn pow10(decimals: u32) -> Result<i128, NormalizationError> {
    if decimals as usize >= POW10.len() {
        return Err(NormalizationError::UnsupportedDecimals);
    }
    Ok(POW10[decimals as usize])
}

/// Converts an asset amount from its native `token_decimals` to canonical protocol units (1e7).
/// Defaults to `Rounding::Ceil` for inflows to ensure the protocol is never underfunded.
pub fn to_protocol_units(amount: i128, token_decimals: u32) -> Result<i128, NormalizationError> {
    to_protocol_units_rounded(amount, token_decimals, Rounding::Ceil)
}

/// Converts an asset amount from `token_decimals` to canonical protocol units (1e7)
/// with explicit rounding direction.
pub fn to_protocol_units_rounded(
    amount: i128,
    token_decimals: u32,
    rounding: Rounding,
) -> Result<i128, NormalizationError> {
    if amount < 0 {
        return Err(NormalizationError::InvalidAmount);
    }
    if amount == 0 {
        return Ok(0);
    }
    if token_decimals > MAX_SUPPORTED_DECIMALS {
        return Err(NormalizationError::UnsupportedDecimals);
    }

    if token_decimals == PROTOCOL_DECIMALS {
        Ok(amount)
    } else if token_decimals < PROTOCOL_DECIMALS {
        let diff = PROTOCOL_DECIMALS - token_decimals;
        let factor = pow10(diff)?;
        amount.checked_mul(factor).ok_or(NormalizationError::MathOverflow)
    } else {
        let diff = token_decimals - PROTOCOL_DECIMALS;
        let factor = pow10(diff)?;
        let q = amount / factor;
        let r = amount % factor;
        if r > 0 && rounding == Rounding::Ceil {
            q.checked_add(1).ok_or(NormalizationError::MathOverflow)
        } else {
            Ok(q)
        }
    }
}

/// Converts a canonical protocol amount (1e7) into native token units at `token_decimals`.
/// Defaults to `Rounding::Floor` for outflows/payouts to prevent protocol overpayment of dust.
pub fn to_token_units(protocol_amount: i128, token_decimals: u32) -> Result<i128, NormalizationError> {
    to_token_units_rounded(protocol_amount, token_decimals, Rounding::Floor)
}

/// Converts a canonical protocol amount (1e7) into native token units at `token_decimals`
/// with explicit rounding direction.
pub fn to_token_units_rounded(
    protocol_amount: i128,
    token_decimals: u32,
    rounding: Rounding,
) -> Result<i128, NormalizationError> {
    if protocol_amount < 0 {
        return Err(NormalizationError::InvalidAmount);
    }
    if protocol_amount == 0 {
        return Ok(0);
    }
    if token_decimals > MAX_SUPPORTED_DECIMALS {
        return Err(NormalizationError::UnsupportedDecimals);
    }

    if token_decimals == PROTOCOL_DECIMALS {
        Ok(protocol_amount)
    } else if token_decimals > PROTOCOL_DECIMALS {
        let diff = token_decimals - PROTOCOL_DECIMALS;
        let factor = pow10(diff)?;
        protocol_amount.checked_mul(factor).ok_or(NormalizationError::MathOverflow)
    } else {
        let diff = PROTOCOL_DECIMALS - token_decimals;
        let factor = pow10(diff)?;
        let q = protocol_amount / factor;
        let r = protocol_amount % factor;
        if r > 0 && rounding == Rounding::Ceil {
            q.checked_add(1).ok_or(NormalizationError::MathOverflow)
        } else {
            Ok(q)
        }
    }
}

/// Canonical inflow converter: converts native token units to protocol units, rounding up (Ceil).
pub fn normalize_inflow(amount: i128, token_decimals: u32) -> Result<i128, NormalizationError> {
    to_protocol_units_rounded(amount, token_decimals, Rounding::Ceil)
}

/// Canonical outflow converter: converts protocol units to native token units, rounding down (Floor).
pub fn normalize_outflow(protocol_amount: i128, token_decimals: u32) -> Result<i128, NormalizationError> {
    to_token_units_rounded(protocol_amount, token_decimals, Rounding::Floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decimals_6_usdc() {
        // 1 USDC = 1_000_000 (6 decimals) -> 10_000_000 (7 decimals)
        let protocol = to_protocol_units(1_000_000, 6).unwrap();
        assert_eq!(protocol, 10_000_000);

        let token_amt = to_token_units(protocol, 6).unwrap();
        assert_eq!(token_amt, 1_000_000);

        // 0.5 USDC = 500_000 -> 5_000_000
        assert_eq!(to_protocol_units(500_000, 6).unwrap(), 5_000_000);
        assert_eq!(to_token_units(5_000_000, 6).unwrap(), 500_000);
    }

    #[test]
    fn test_decimals_7_stellar_native() {
        // 1 XLM = 10_000_000 (7 decimals) -> 10_000_000 (7 decimals)
        let protocol = to_protocol_units(10_000_000, 7).unwrap();
        assert_eq!(protocol, 10_000_000);

        let token_amt = to_token_units(protocol, 7).unwrap();
        assert_eq!(token_amt, 10_000_000);
    }

    #[test]
    fn test_decimals_18_weth() {
        // 1 WETH = 10^18 -> 10_000_000 (7 decimals)
        let one_weth = 1_000_000_000_000_000_000i128;
        let protocol = to_protocol_units(one_weth, 18).unwrap();
        assert_eq!(protocol, 10_000_000);

        let token_amt = to_token_units(protocol, 18).unwrap();
        assert_eq!(token_amt, one_weth);
    }

    #[test]
    fn test_round_trip_invertibility() {
        // All decimals from 7 to 18 should round-trip perfectly for canonical amounts
        let canonical_amount = 50_000_000i128; // 5 units in 1e7
        for d in 7..=18 {
            let token_amt = to_token_units(canonical_amount, d).unwrap();
            let back = to_protocol_units(token_amt, d).unwrap();
            assert_eq!(back, canonical_amount, "Failed round-trip for decimals {}", d);
        }

        // All decimals from 0 to 7 should round-trip perfectly for exact native token amounts
        for d in 0..=7 {
            let native_amount = 42i128 * pow10(d).unwrap();
            let proto = to_protocol_units(native_amount, d).unwrap();
            let back = to_token_units(proto, d).unwrap();
            assert_eq!(back, native_amount, "Failed round-trip for decimals {}", d);
        }
    }

    #[test]
    fn test_reject_unsupported_decimals() {
        assert_eq!(
            to_protocol_units(100, 19),
            Err(NormalizationError::UnsupportedDecimals)
        );
        assert_eq!(
            to_token_units(100, 19),
            Err(NormalizationError::UnsupportedDecimals)
        );
        assert_eq!(
            to_protocol_units(100, 255),
            Err(NormalizationError::UnsupportedDecimals)
        );
    }

    #[test]
    fn test_dust_reconciliation_and_rounding() {
        // Inflow (18 decimals -> 7 decimals, factor = 10^11):
        // 1 wei (minimal dust):
        // Ceil rounds up to 1 protocol unit
        assert_eq!(to_protocol_units_rounded(1, 18, Rounding::Ceil).unwrap(), 1);
        // Floor rounds down to 0
        assert_eq!(to_protocol_units_rounded(1, 18, Rounding::Floor).unwrap(), 0);

        // 10^11 exact: both give 1
        assert_eq!(to_protocol_units_rounded(100_000_000_000, 18, Rounding::Ceil).unwrap(), 1);
        assert_eq!(to_protocol_units_rounded(100_000_000_000, 18, Rounding::Floor).unwrap(), 1);

        // 10^11 + 1: Ceil gives 2, Floor gives 1
        assert_eq!(to_protocol_units_rounded(100_000_000_001, 18, Rounding::Ceil).unwrap(), 2);
        assert_eq!(to_protocol_units_rounded(100_000_000_001, 18, Rounding::Floor).unwrap(), 1);

        // Outflow (7 decimals -> 6 decimals, factor = 10):
        // 15 protocol units = 1.5 token units.
        // Floor rounds down to 1 (protects protocol from dust overpayment)
        assert_eq!(to_token_units_rounded(15, 6, Rounding::Floor).unwrap(), 1);
        // Ceil rounds up to 2
        assert_eq!(to_token_units_rounded(15, 6, Rounding::Ceil).unwrap(), 2);
    }

    #[test]
    fn test_negative_amounts_rejected() {
        assert_eq!(to_protocol_units(-1, 6), Err(NormalizationError::InvalidAmount));
        assert_eq!(to_token_units(-1, 6), Err(NormalizationError::InvalidAmount));
        assert_eq!(to_protocol_units_rounded(-50, 18, Rounding::Ceil), Err(NormalizationError::InvalidAmount));
    }

    #[test]
    fn test_overflow_protection() {
        // Multiplying huge protocol amount by 10^11 for 18 decimals overflows i128
        let huge = i128::MAX / 10;
        assert_eq!(to_token_units(huge, 18), Err(NormalizationError::MathOverflow));
    }
}
