pub const MAX_FEE_BPS: u32 = 1_000;
const BPS_DENOMINATOR: i128 = 10_000;

pub fn assess_fee(gross: i128, fee_bps: u32) -> Option<(i128, i128)> {
    if gross < 0 || fee_bps > MAX_FEE_BPS {
        return None;
    }

    let rate = i128::from(fee_bps);
    let fee = (gross / BPS_DENOMINATOR)
        .checked_mul(rate)?
        .checked_add((gross % BPS_DENOMINATOR).checked_mul(rate)? / BPS_DENOMINATOR)?;
    Some((gross.checked_sub(fee)?, fee))
}