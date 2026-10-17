//! Pure accounting arithmetic and `#[cfg(kani)]` formal verification harnesses
//! for `RefractBackstop` and `RefractTreasury` accounting invariants (#292).

pub const BPS_DENOM: i128 = 10_000;
pub const MAX_BOUNDED_AMOUNT: i128 = 1_000_000_000_000;

/// Pure share-minting math for a backstop deposit.
pub fn pure_calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> Option<i128> {
    if amount <= 0 || total_shares < 0 || total_capital < 0 {
        return None;
    }
    if total_shares == 0 || total_capital == 0 {
        Some(amount)
    } else {
        amount.checked_mul(total_shares)?.checked_div(total_capital)
    }
}

/// Pure withdrawal quote math for burning `shares`.
pub fn pure_quote_withdrawal(
    shares: i128,
    total_shares: i128,
    total_capital: i128,
) -> Option<i128> {
    if shares <= 0 || total_shares < 0 || total_capital < 0 || shares > total_shares {
        return None;
    }
    if total_shares == 0 {
        Some(0)
    } else {
        shares.checked_mul(total_capital)?.checked_div(total_shares)
    }
}

/// Pure shortfall haircut math: drains up to `shortfall` from `total_capital`
/// without burning shares, returning `(covered, remaining_capital)`.
pub fn pure_apply_haircut(shortfall: i128, total_capital: i128) -> Option<(i128, i128)> {
    if shortfall <= 0 || total_capital < 0 {
        return None;
    }
    let covered = if shortfall > total_capital {
        total_capital
    } else {
        shortfall
    };
    let remaining = total_capital.checked_sub(covered)?;
    Some((covered, remaining))
}

/// Pure premium accrual math: routes `amount * premium_share_bps / 10_000` into `total_capital`.
pub fn pure_accrue_premium(
    amount: i128,
    premium_share_bps: i128,
    total_capital: i128,
) -> Option<(i128, i128)> {
    if amount <= 0 || !(0..=BPS_DENOM).contains(&premium_share_bps) || total_capital < 0 {
        return None;
    }
    let cut = amount
        .checked_mul(premium_share_bps)?
        .checked_div(BPS_DENOM)?;
    let new_capital = total_capital.checked_add(cut)?;
    Some((cut, new_capital))
}

/// Pure treasury budget allocation check: ensures recorded budget allocations
/// never exceed `held_balance`.
pub fn pure_allocate_treasury_budget(
    held_balance: i128,
    existing_allocated_sum: i128,
    new_allocation: i128,
) -> Option<i128> {
    if held_balance < 0 || existing_allocated_sum < 0 || new_allocation <= 0 {
        return None;
    }
    if existing_allocated_sum > held_balance {
        return None;
    }
    let updated_sum = existing_allocated_sum.checked_add(new_allocation)?;
    if updated_sum > held_balance {
        None
    } else {
        Some(updated_sum)
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Assumption: inputs are bounded by `MAX_BOUNDED_AMOUNT` (10^12 stroops).
    #[kani::proof]
    fn proof_deposit_and_withdraw_never_create_capital() {
        let total_shares: i128 = kani::any();
        let total_capital: i128 = kani::any();
        let deposit_amount: i128 = kani::any();

        kani::assume(total_shares > 0 && total_shares <= MAX_BOUNDED_AMOUNT);
        kani::assume(total_capital > 0 && total_capital <= MAX_BOUNDED_AMOUNT);
        kani::assume(deposit_amount > 0 && deposit_amount <= MAX_BOUNDED_AMOUNT);

        if let Some(minted) = pure_calc_shares(deposit_amount, total_shares, total_capital) {
            if minted > 0 {
                let new_shares = total_shares + minted;
                let new_capital = total_capital + deposit_amount;
                let redeemed = pure_quote_withdrawal(minted, new_shares, new_capital).unwrap_or(0);
                assert!(redeemed <= deposit_amount);
                assert!(new_capital - redeemed >= total_capital);
            }
        }
    }

    #[kani::proof]
    fn proof_premium_accrual_never_decreases_share_price() {
        let total_shares: i128 = kani::any();
        let total_capital: i128 = kani::any();
        let premium: i128 = kani::any();
        let bps: i128 = kani::any();

        kani::assume(total_shares > 0 && total_shares <= MAX_BOUNDED_AMOUNT);
        kani::assume(total_capital >= 0 && total_capital <= MAX_BOUNDED_AMOUNT);
        kani::assume(premium > 0 && premium <= MAX_BOUNDED_AMOUNT);
        kani::assume((0..=BPS_DENOM).contains(&bps));

        let (cut, new_capital) = pure_accrue_premium(premium, bps, total_capital).unwrap();
        assert!(cut >= 0);
        assert!(new_capital >= total_capital);
        assert!(new_capital * total_shares >= total_capital * total_shares);
    }

    #[kani::proof]
    fn proof_haircut_conserves_capital_bounds() {
        let shortfall: i128 = kani::any();
        let total_capital: i128 = kani::any();

        kani::assume(shortfall > 0 && shortfall <= MAX_BOUNDED_AMOUNT);
        kani::assume(total_capital >= 0 && total_capital <= MAX_BOUNDED_AMOUNT);

        let (covered, remaining) = pure_apply_haircut(shortfall, total_capital).unwrap();
        assert!(covered >= 0 && covered <= total_capital && covered <= shortfall);
        assert!(remaining >= 0);
        assert_eq!(covered + remaining, total_capital);
    }

    #[kani::proof]
    fn proof_treasury_budgets_never_exceed_held_balance() {
        let held: i128 = kani::any();
        let allocated: i128 = kani::any();
        let req: i128 = kani::any();

        kani::assume(held >= 0 && held <= MAX_BOUNDED_AMOUNT);
        kani::assume(allocated >= 0 && allocated <= held);
        kani::assume(req > 0 && req <= MAX_BOUNDED_AMOUNT);

        if let Some(new_sum) = pure_allocate_treasury_budget(held, allocated, req) {
            assert!(new_sum <= held);
            assert_eq!(new_sum, allocated + req);
        } else {
            assert!(allocated + req > held);
        }
    }
}
