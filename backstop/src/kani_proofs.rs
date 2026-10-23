//! Kani model-checking proofs for the backstop share arithmetic.
//!
//! Compiled only under `cargo kani`, so `cargo test` and the wasm build do not
//! see them. Symbolic inputs are bounded to realistic magnitudes so the proofs
//! terminate. Run locally with `cargo kani -p refract-backstop`.

use super::RefractBackstop;

/// Upper bound on symbolic amounts and totals, in raw token units.
const BOUND: i128 = 1_000_000_000;

/// Bounded, non-negative inputs for a deposit at some share price.
fn bounded_deposit() -> (i128, i128, i128) {
    let amount: i128 = kani::any();
    let total_shares: i128 = kani::any();
    let total_capital: i128 = kani::any();
    kani::assume(amount > 0 && amount <= BOUND);
    kani::assume(total_shares >= 0 && total_shares <= BOUND);
    kani::assume(total_capital >= 0 && total_capital <= BOUND);
    (amount, total_shares, total_capital)
}

/// A deposit never mints a negative number of shares.
#[kani::proof]
fn deposit_mints_non_negative_shares() {
    let (amount, total_shares, total_capital) = bounded_deposit();
    let shares = RefractBackstop::calc_shares(amount, total_shares, total_capital);
    assert!(shares >= 0);
}

/// Withdrawing any holding of shares returns at most the capital in the pool,
/// so withdrawals cannot take more than exists.
#[kani::proof]
fn withdrawal_never_exceeds_total_capital() {
    let total_shares: i128 = kani::any();
    let total_capital: i128 = kani::any();
    let shares: i128 = kani::any();
    kani::assume(total_shares > 0 && total_shares <= BOUND);
    kani::assume(total_capital >= 0 && total_capital <= BOUND);
    kani::assume(shares >= 0 && shares <= total_shares);

    let out = RefractBackstop::quote_withdrawal(shares, total_shares, total_capital);
    assert!(out >= 0 && out <= total_capital);
}

/// Depositing and then withdrawing the shares just minted, at the same price,
/// never returns more than was deposited. This is the no-value-created rule
/// for a deposit followed by a withdrawal.
///
/// Assumes shares and capital both exist. With no shares, the first depositor
/// is minted 1:1 and can withdraw the capital already in the pool, so the
/// property does not hold in that state and is not claimed for it.
#[kani::proof]
fn deposit_then_withdraw_returns_no_more_than_deposited() {
    let (amount, total_shares, total_capital) = bounded_deposit();
    kani::assume(total_shares > 0 && total_capital > 0);
    let shares = RefractBackstop::calc_shares(amount, total_shares, total_capital);
    kani::assume(shares > 0);

    let out = RefractBackstop::quote_withdrawal(
        shares,
        total_shares + shares,
        total_capital + amount,
    );
    assert!(out <= amount);
}
