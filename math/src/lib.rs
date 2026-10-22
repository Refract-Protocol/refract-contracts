//! # refract-math
//!
//! One place for every integer division that splits money between parties.
//! The policy is specified in `SPEC.md` §8 ("Rounding, dust and remainder
//! distribution"). In short:
//!
//! * **Multi-party splits** use [`allocate`] (largest-remainder). Outputs
//!   always sum to the input exactly, so a split never creates dust.
//! * **Two-party splits** ("cut" vs "rest") use [`split_bps`] /
//!   [`split_ratio`]. The cut is floored and the rest is computed by
//!   subtraction, so `cut + rest == amount` always holds.
//! * **Single-party floors** (share minting, share redemption) use
//!   [`mul_div_floor`]. They round in the protocol's favour; the residual
//!   stays in the pool it came from and is owned by the remaining holders.
//! * Any balance that no party owns (for example capital left in a vault with
//!   zero outstanding shares) is **dust** and is routed to the treasury dust
//!   account. [`route_dust`] is the accounting helper for that hand-off.
//!
//! Every helper is panic-free: overflow and invalid input return
//! [`MathError`]. Intermediate products use full 256-bit precision, so
//! `amount * numerator / denominator` never overflows when the true result
//! fits in an `i128`.
//!
//! The crate is `no_std` and needs only `alloc` (for the [`allocate`] output
//! vector), which the Soroban SDK's `alloc` feature provides on wasm.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// Basis-point denominator (100% = 10_000 bps).
pub const BPS_DENOM: i128 = 10_000;

/// Errors returned by the allocation helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathError {
    /// An amount or weight was negative.
    Negative,
    /// A denominator was zero.
    DivisionByZero,
    /// The sum of weights does not fit in `u128`, or a result does not fit
    /// in `i128`.
    Overflow,
    /// `total > 0` but every weight is zero, so there is nobody to allocate
    /// to. Callers must route the whole amount to the dust account instead.
    NoRecipients,
    /// A basis-point value was outside `0..=10_000`, or a ratio numerator was
    /// larger than its denominator.
    InvalidRatio,
}

/// Result of a two-party split. `cut + rest == amount` always.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// The floored share sent to the "cut" party.
    pub cut: i128,
    /// Everything else (including any rounding remainder).
    pub rest: i128,
}

/// Splits `total` across `weights` with the largest-remainder method.
///
/// Each recipient first gets `floor(total * w_i / W)` where `W = sum(w)`.
/// The `L = total - sum(floors)` leftover units (always `< n`) go one each to
/// the recipients with the largest fractional remainders. Ties are broken by
/// the lowest index, so the result is fully deterministic.
///
/// Guarantees (checked by tests and by a debug assertion):
/// * `sum(output) == total`;
/// * a zero weight always receives zero;
/// * each output is within one unit of the exact pro-rata share.
///
/// Edge cases: `total == 0` returns all zeros (even with empty or all-zero
/// weights); an empty or all-zero weight list with `total > 0` returns
/// [`MathError::NoRecipients`].
pub fn allocate(total: i128, weights: &[i128]) -> Result<Vec<i128>, MathError> {
    if total < 0 {
        return Err(MathError::Negative);
    }
    let mut weight_sum: u128 = 0;
    for w in weights {
        if *w < 0 {
            return Err(MathError::Negative);
        }
        weight_sum = weight_sum
            .checked_add(*w as u128)
            .ok_or(MathError::Overflow)?;
    }

    let mut out = Vec::with_capacity(weights.len());
    if total == 0 {
        out.resize(weights.len(), 0);
        return Ok(out);
    }
    if weight_sum == 0 {
        return Err(MathError::NoRecipients);
    }

    let total_u = total as u128;
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(weights.len());
    let mut assigned: u128 = 0;
    for (i, w) in weights.iter().enumerate() {
        let (q, r) = mul_div_rem_u128(total_u, *w as u128, weight_sum)?;
        // q <= total because w <= weight_sum, so it fits in i128.
        out.push(q as i128);
        assigned += q;
        if r > 0 {
            remainders.push((r, i));
        }
    }

    // Largest remainder first; lowest index wins ties.
    remainders.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let leftover = total_u - assigned;
    // leftover < number of positive remainders, so this never runs dry.
    for (_, i) in remainders.iter().take(leftover as usize) {
        out[*i] += 1;
    }

    debug_assert_eq!(out.iter().sum::<i128>(), total);
    Ok(out)
}

/// Splits `amount` into a floored `bps` cut and the rest.
///
/// `cut = floor(amount * bps / 10_000)`, `rest = amount - cut`.
pub fn split_bps(amount: i128, bps: i128) -> Result<Split, MathError> {
    if !(0..=BPS_DENOM).contains(&bps) {
        return Err(MathError::InvalidRatio);
    }
    split_ratio(amount, bps, BPS_DENOM)
}

/// Splits `amount` into `floor(amount * numerator / denominator)` and the
/// rest. Requires `0 <= numerator <= denominator`.
pub fn split_ratio(amount: i128, numerator: i128, denominator: i128) -> Result<Split, MathError> {
    if denominator <= 0 {
        return Err(MathError::DivisionByZero);
    }
    if numerator < 0 || numerator > denominator {
        return Err(MathError::InvalidRatio);
    }
    let cut = mul_div_floor(amount, numerator, denominator)?;
    Ok(Split {
        cut,
        rest: amount - cut,
    })
}

/// `floor(a * b / c)` for non-negative inputs with a 256-bit intermediate.
///
/// Used for single-party floors such as share minting and redemption.
/// Returns [`MathError::Overflow`] only when the final result does not fit in
/// an `i128`.
pub fn mul_div_floor(a: i128, b: i128, c: i128) -> Result<i128, MathError> {
    if a < 0 || b < 0 || c < 0 {
        return Err(MathError::Negative);
    }
    if c == 0 {
        return Err(MathError::DivisionByZero);
    }
    let (q, _) = mul_div_rem_u128(a as u128, b as u128, c as u128)?;
    i128::try_from(q).map_err(|_| MathError::Overflow)
}

/// Moves the unowned part of a vault into the dust sink.
///
/// Given a vault `balance` and the amount `owned` by its participants,
/// returns `(new_balance, dust)` where `dust = balance - owned` and
/// `new_balance = owned`. Conservation: `new_balance + dust == balance`.
/// Returns [`MathError::Negative`] if `owned > balance` (an insolvent vault is
/// a bug, not dust).
pub fn route_dust(balance: i128, owned: i128) -> Result<(i128, i128), MathError> {
    if balance < 0 || owned < 0 || owned > balance {
        return Err(MathError::Negative);
    }
    Ok((owned, balance - owned))
}

/// Computes `(floor(a * b / c), (a * b) mod c)` exactly.
fn mul_div_rem_u128(a: u128, b: u128, c: u128) -> Result<(u128, u128), MathError> {
    if c == 0 {
        return Err(MathError::DivisionByZero);
    }
    let (hi, lo) = mul_wide(a, b);
    if hi == 0 {
        return Ok((lo / c, lo % c));
    }
    if hi >= c {
        // The quotient would not fit in 128 bits.
        return Err(MathError::Overflow);
    }
    Ok(div_wide(hi, lo, c))
}

/// Full 128x128 -> 256-bit multiply, returned as `(high, low)`.
fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    const MASK: u128 = u64::MAX as u128;
    let (a_hi, a_lo) = (a >> 64, a & MASK);
    let (b_hi, b_lo) = (b >> 64, b & MASK);

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    let mid = (ll >> 64) + (lh & MASK) + (hl & MASK);
    let lo = (ll & MASK) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}

/// Divides the 256-bit value `(hi, lo)` by `d`. Requires `hi < d`, which
/// guarantees the quotient fits in 128 bits.
fn div_wide(hi: u128, lo: u128, d: u128) -> (u128, u128) {
    let mut rem = hi;
    let mut quot: u128 = 0;
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            quot |= 1u128 << i;
        }
    }
    (quot, rem)
}

#[cfg(test)]
mod tests;
