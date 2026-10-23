//! Shared amount normalization between SEP-41 token decimals and the protocol's
//! 1e7 fixed-point convention.
//!
//! Tokens state their amounts in their own number of decimals, for example 7
//! for XLM or 6 for USDC. The protocol stores amounts with 7 decimals. Every
//! payout path converts through this module, so the same conversion is used
//! everywhere and a rounding rule is stated once. Converting to fewer decimals
//! rounds toward zero, which never pays out more than the amount given. Overflow
//! is an error, not a wrap.

/// Decimals used by the protocol's fixed-point convention.
pub const PROTOCOL_DECIMALS: u32 = 7;

/// Most decimals a token may declare. Larger values overflow `10^decimals` in i128.
pub const MAX_DECIMALS: u32 = 18;

/// Why a conversion was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountError {
    /// The token declares more decimals than `MAX_DECIMALS`.
    UnsupportedDecimals,
    /// The converted amount does not fit in an i128.
    Overflow,
}

/// Converts `amount` from `from` decimals to `to` decimals. Scaling down rounds
/// toward zero.
pub fn convert(amount: i128, from: u32, to: u32) -> Result<i128, AmountError> {
    if from > MAX_DECIMALS || to > MAX_DECIMALS {
        return Err(AmountError::UnsupportedDecimals);
    }
    if from == to {
        return Ok(amount);
    }
    if from < to {
        let factor = 10i128.pow(to - from);
        return amount.checked_mul(factor).ok_or(AmountError::Overflow);
    }
    let factor = 10i128.pow(from - to);
    Ok(amount / factor)
}

/// Converts a token amount into the protocol's 7-decimal fixed point.
pub fn to_protocol(amount: i128, token_decimals: u32) -> Result<i128, AmountError> {
    convert(amount, token_decimals, PROTOCOL_DECIMALS)
}

/// Converts a protocol amount back to a token's decimals. Rounds toward zero.
pub fn from_protocol(amount: i128, token_decimals: u32) -> Result<i128, AmountError> {
    convert(amount, PROTOCOL_DECIMALS, token_decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_up_is_exact_and_scaling_down_rounds_toward_zero() {
        assert_eq!(convert(1_500_000, 6, 7), Ok(15_000_000));
        assert_eq!(convert(15_000_009, 7, 6), Ok(1_500_000));
        assert_eq!(convert(-15_000_009, 7, 6), Ok(-1_500_000));
        assert_eq!(convert(42, 7, 7), Ok(42));
    }

    #[test]
    fn a_token_with_more_decimals_than_the_protocol_round_trips_within_one_unit() {
        // 18-decimal token: 1 unit is 10^11 protocol units.
        let protocol = to_protocol(123_456_789_012_345_678, 18).unwrap();
        assert_eq!(protocol, 1_234_567);
        assert_eq!(from_protocol(protocol, 18), Ok(123_456_700_000_000_000));
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        assert_eq!(convert(i128::MAX, 0, 7), Err(AmountError::Overflow));
    }

    #[test]
    fn unsupported_decimals_are_refused() {
        assert_eq!(
            convert(1, MAX_DECIMALS + 1, 7),
            Err(AmountError::UnsupportedDecimals)
        );
        assert_eq!(convert(1, 7, 19), Err(AmountError::UnsupportedDecimals));
    }
}
