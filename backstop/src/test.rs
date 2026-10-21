#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{
        storage::{Instance as _, Persistent as _},
        Address as _, Events as _, Ledger as _,
    },
    token::{StellarAssetClient, TokenClient},
    xdr::{LedgerKey, ScVal},
    IntoVal, Symbol, TryFromVal, Val,
};
use std::{cell::RefCell, vec::Vec as StdVec};

const COOLDOWN: u64 = 7 * 24 * 60 * 60;

struct Setup {
    env: Env,
    id: Address,
    pool: Address,
    admin: Address,
    token: Address,
    holders: RefCell<StdVec<Address>>,
}

impl Setup {
    fn new() -> Self {
        Self::with_bps(5_000)
    }

    fn with_bps(bps: i128) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| {
            l.timestamp = 1_000_000;
            l.sequence_number = 100;
        });
        let id = env.register_contract(None, RefractBackstop);
        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        RefractBackstopClient::new(&env, &id).initialize(&pool, &admin, &token, &bps, &COOLDOWN);
        Setup {
            env,
            id,
            pool,
            admin,
            token,
            holders: RefCell::new(StdVec::new()),
        }
    }

    fn client(&self) -> RefractBackstopClient<'_> {
        RefractBackstopClient::new(&self.env, &self.id)
    }

    fn token(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.token)
    }

    fn staker(&self, funds: i128) -> Address {
        let a = Address::generate(&self.env);
        StellarAssetClient::new(&self.env, &self.token).mint(&a, &funds);
        self.holders.borrow_mut().push(a.clone());
        a
    }

    fn advance(&self, secs: u64) {
        self.env.ledger().with_mut(|l| l.timestamp += secs);
    }

    fn advance_ledgers(&self, n: u32) {
        self.keep_token_alive(n);
        self.env.ledger().with_mut(|l| {
            l.sequence_number += n;
            l.timestamp += n as u64 * 5;
        });
    }

    /// The mock token is not under test; keep its instance live across
    /// ledger jumps so only the backstop's own entries can expire.
    fn keep_token_alive(&self, n: u32) {
        if n < PERSISTENT_BUMP_AMOUNT {
            let mut holders = self.holders.borrow().clone();
            holders.push(self.id.clone());
            holders.push(self.pool.clone());
            self.env.as_contract(&self.token, || {
                let st = self.env.storage();
                st.instance().extend_ttl(n + 1, n + 1);
                for h in holders {
                    let key = (Symbol::new(&self.env, "Balance"), h);
                    if st.persistent().has(&key) {
                        st.persistent().extend_ttl(&key, n + 1, n + 1);
                    }
                }
            });
        }
    }
}

// ─── Initialization ─────────────────────────────────────────────────────

#[test]
fn initialize_validates_and_runs_once() {
    let s = Setup::new();
    let c = s.client();
    assert_eq!(c.config().cooldown_secs, COOLDOWN);
    assert_eq!(c.config().pool, s.pool);
    assert_eq!(
        c.try_initialize(&s.pool, &s.admin, &s.token, &0, &0),
        Err(Ok(BackstopError::AlreadyInitialized))
    );

    let env = Env::default();
    let id = env.register_contract(None, RefractBackstop);
    let c = RefractBackstopClient::new(&env, &id);
    let a = Address::generate(&env);
    assert_eq!(c.try_state(), Err(Ok(BackstopError::NotInitialized)));
    assert_eq!(c.try_extend_ttl(), Err(Ok(BackstopError::NotInitialized)));
    assert_eq!(
        c.try_initialize(&a, &a, &a, &10_001, &0),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );
    assert_eq!(
        c.try_initialize(&a, &a, &a, &-1, &0),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );
    assert_eq!(
        c.try_initialize(&a, &a, &a, &0, &(MAX_COOLDOWN_SECS + 1)),
        Err(Ok(BackstopError::InvalidCooldown))
    );
}

// ─── Deposits ───────────────────────────────────────────────────────────

#[test]
fn deposit_mints_shares_at_current_price() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    let b = s.staker(1_000);
    assert_eq!(c.deposit(&a, &1_000), 1_000);
    assert_eq!(c.try_deposit(&b, &0), Err(Ok(BackstopError::InvalidAmount)));

    // A loss halves the share price; B's deposit buys twice the shares.
    c.apply_loss(&s.pool, &500);
    assert_eq!(c.deposit(&b, &1_000), 2_000);
    assert_eq!(c.value_of(&a), 500);
    assert_eq!(c.value_of(&b), 1_000);
    assert_eq!(c.shares_of(&b), 2_000);
}

#[test]
fn deposit_rejected_when_fully_written_down() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    let b = s.staker(1_000);
    c.deposit(&a, &1_000);
    c.apply_loss(&s.pool, &1_000);
    // A still holds 1_000 worthless shares. B's deposit would be diluted.
    assert_eq!(c.try_deposit(&b, &1_000), Err(Ok(BackstopError::Insolvent)));
}

#[test]
fn deposit_rounding_to_zero_shares_is_rejected() {
    let s = Setup::with_bps(10_000);
    let c = s.client();
    let a = s.staker(10);
    let b = s.staker(10);
    let payer = s.staker(1_000);
    c.deposit(&a, &1);
    c.receive_premium(&payer, &999); // 1 share now worth 1_000
    assert_eq!(
        c.try_deposit(&b, &10),
        Err(Ok(BackstopError::InvalidAmount))
    );
}

// ─── Unstake lifecycle ──────────────────────────────────────────────────

#[test]
fn full_lifecycle_stake_request_loss_execute() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(6_000);
    let b = s.staker(4_000);
    c.deposit(&a, &6_000);
    c.deposit(&b, &4_000);

    let ready_at = c.request_unstake(&a, &6_000);
    assert_eq!(ready_at, s.env.ledger().timestamp() + COOLDOWN);
    let rec = c.stake_of(&a);
    assert_eq!(
        (rec.active_shares, rec.pending_shares, rec.ready_at),
        (0, 6_000, ready_at)
    );
    assert_eq!(c.state().pending_shares, 6_000);

    // Loss during the cooldown: 10% of capital. A is still in the base.
    assert_eq!(c.apply_loss(&s.pool, &1_000), 1_000);
    assert_eq!(s.token().balance(&s.pool), 1_000);

    s.advance(COOLDOWN);
    assert_eq!(c.execute_unstake(&a), 5_400); // 6_000 - 10%
    assert_eq!(s.token().balance(&a), 5_400);
    assert_eq!(c.stake_of(&a), StakeRecord::default());
    assert_eq!(c.value_of(&b), 3_600);

    let st = c.state();
    assert_eq!(st.total_shares, 4_000);
    assert_eq!(st.pending_shares, 0);
    assert_eq!(st.total_capital, 3_600);
    assert_eq!(st.total_losses_covered, 1_000);
    assert_eq!(s.token().balance(&s.id), st.total_capital);
}

#[test]
fn execute_before_ready_at_fails_and_double_execute_rejected() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);
    c.request_unstake(&a, &400);

    s.advance(COOLDOWN - 1);
    assert_eq!(
        c.try_execute_unstake(&a),
        Err(Ok(BackstopError::CooldownNotElapsed))
    );
    s.advance(1);
    assert_eq!(c.execute_unstake(&a), 400);
    assert_eq!(
        c.try_execute_unstake(&a),
        Err(Ok(BackstopError::NoPendingUnstake))
    );
    assert_eq!(c.stake_of(&a).active_shares, 600);
}

#[test]
fn request_validation() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);
    assert_eq!(
        c.try_request_unstake(&a, &0),
        Err(Ok(BackstopError::InvalidAmount))
    );
    assert_eq!(
        c.try_request_unstake(&a, &1_001),
        Err(Ok(BackstopError::InsufficientShares))
    );
    c.request_unstake(&a, &100);
    assert_eq!(
        c.try_request_unstake(&a, &100),
        Err(Ok(BackstopError::UnstakeAlreadyPending))
    );
}

#[test]
fn cancel_returns_shares_to_active() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);
    assert_eq!(
        c.try_cancel_unstake(&a),
        Err(Ok(BackstopError::NoPendingUnstake))
    );
    c.request_unstake(&a, &300);
    assert_eq!(c.cancel_unstake(&a), 300);
    assert_eq!(
        c.stake_of(&a),
        StakeRecord {
            active_shares: 1_000,
            pending_shares: 0,
            ready_at: 0
        }
    );
    assert_eq!(c.state().pending_shares, 0);
    s.advance(COOLDOWN);
    assert_eq!(
        c.try_execute_unstake(&a),
        Err(Ok(BackstopError::NoPendingUnstake))
    );
}

#[test]
fn haircut_applies_whether_loss_lands_before_or_after_request() {
    // Loss *before* the request.
    let s1 = Setup::new();
    let c1 = s1.client();
    let a1 = s1.staker(1_000);
    let b1 = s1.staker(1_000);
    c1.deposit(&a1, &1_000);
    c1.deposit(&b1, &1_000);
    c1.apply_loss(&s1.pool, &500);
    c1.request_unstake(&a1, &1_000);
    s1.advance(COOLDOWN);
    let before = c1.execute_unstake(&a1);

    // Loss *after* the request, during the cooldown.
    let s2 = Setup::new();
    let c2 = s2.client();
    let a2 = s2.staker(1_000);
    let b2 = s2.staker(1_000);
    c2.deposit(&a2, &1_000);
    c2.deposit(&b2, &1_000);
    c2.request_unstake(&a2, &1_000);
    c2.apply_loss(&s2.pool, &500);
    s2.advance(COOLDOWN);
    let after = c2.execute_unstake(&a2);

    // Requesting early does not dodge the loss.
    assert_eq!(before, 750);
    assert_eq!(after, 750);
}

#[test]
fn loss_after_execute_does_not_touch_exited_staker() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    let b = s.staker(1_000);
    c.deposit(&a, &1_000);
    c.deposit(&b, &1_000);
    c.request_unstake(&a, &1_000);
    s.advance(COOLDOWN);
    assert_eq!(c.execute_unstake(&a), 1_000);
    c.apply_loss(&s.pool, &400);
    assert_eq!(s.token().balance(&a), 1_000);
    assert_eq!(c.value_of(&b), 600);
}

#[test]
fn cooldown_change_is_not_retroactive() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    let b = s.staker(1_000);
    c.deposit(&a, &1_000);
    c.deposit(&b, &1_000);
    let ready_a = c.request_unstake(&a, &1_000);

    let stranger = Address::generate(&s.env);
    assert_eq!(
        c.try_set_cooldown(&stranger, &1),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(
        c.try_set_cooldown(&s.admin, &(MAX_COOLDOWN_SECS + 1)),
        Err(Ok(BackstopError::InvalidCooldown))
    );
    c.set_cooldown(&s.admin, &(COOLDOWN * 2));
    assert_eq!(c.config().cooldown_secs, COOLDOWN * 2);
    assert_eq!(c.stake_of(&a).ready_at, ready_a);

    // New requests use the new cooldown; old ones keep theirs.
    let ready_b = c.request_unstake(&b, &1_000);
    assert_eq!(ready_b, s.env.ledger().timestamp() + COOLDOWN * 2);
    s.advance(COOLDOWN);
    assert_eq!(c.execute_unstake(&a), 1_000);
    assert_eq!(
        c.try_execute_unstake(&b),
        Err(Ok(BackstopError::CooldownNotElapsed))
    );

    // Shortening the cooldown does not pull B's date forward either.
    c.set_cooldown(&s.admin, &0);
    assert_eq!(
        c.try_execute_unstake(&b),
        Err(Ok(BackstopError::CooldownNotElapsed))
    );
    s.advance(COOLDOWN);
    assert_eq!(c.execute_unstake(&b), 1_000);
}

// ─── Losses ─────────────────────────────────────────────────────────────

#[test]
fn apply_loss_is_pool_only_and_validates_amount() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);
    assert_eq!(
        c.try_apply_loss(&s.admin, &10),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(
        c.try_apply_loss(&a, &10),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(
        c.try_apply_loss(&s.pool, &0),
        Err(Ok(BackstopError::InvalidAmount))
    );
    assert_eq!(
        c.try_apply_loss(&s.pool, &-5),
        Err(Ok(BackstopError::InvalidAmount))
    );
}

#[test]
fn loss_larger_than_capital_clamps_and_records_shortfall() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);

    assert_eq!(c.apply_loss(&s.pool, &1_500), 1_000);
    let st = c.state();
    assert_eq!(st.total_capital, 0);
    assert_eq!(st.total_losses_covered, 1_000);
    assert_eq!(st.total_shortfall, 500);
    assert_eq!(s.token().balance(&s.pool), 1_000);

    // A further loss with no capital covers nothing and is all shortfall.
    assert_eq!(c.apply_loss(&s.pool, &(i128::MAX / 2)), 0);
    assert_eq!(c.state().total_shortfall, 500 + i128::MAX / 2);

    // The wiped-out staker can still exit (for zero).
    c.request_unstake(&a, &1_000);
    s.advance(COOLDOWN);
    assert_eq!(c.execute_unstake(&a), 0);
    assert_eq!(c.state().total_shares, 0);
}

#[test]
fn loss_on_empty_backstop_is_all_shortfall() {
    let s = Setup::new();
    let c = s.client();
    assert_eq!(c.apply_loss(&s.pool, &1), 0);
    assert_eq!(c.state().total_shortfall, 1);
}

#[test]
fn loss_events_are_emitted() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);
    c.request_unstake(&a, &500);
    c.apply_loss(&s.pool, &100);
    s.advance(COOLDOWN);
    c.execute_unstake(&a);

    let names: StdVec<Symbol> = s
        .env
        .events()
        .all()
        .iter()
        .filter(|(id, _, _)| *id == s.id)
        .map(|(_, topics, _)| Symbol::try_from_val(&s.env, &topics.get(0).unwrap()).unwrap())
        .collect();
    assert!(names.contains(&Symbol::new(&s.env, "UnstakeRequested")));
    assert!(names.contains(&Symbol::new(&s.env, "LossSocialized")));
    assert!(names.contains(&Symbol::new(&s.env, "UnstakeExecuted")));
}

#[test]
fn rounding_conserves_funds_to_the_stroop_with_many_small_stakers() {
    let s = Setup::with_bps(3_333);
    let c = s.client();
    let payer = s.staker(1_000_000);

    let mut stakers = StdVec::new();
    let mut deposited = 0i128;
    for i in 0..40i128 {
        let amt = 7 + i * 13; // awkward, co-prime-ish amounts
        let a = s.staker(amt);
        c.deposit(&a, &amt);
        deposited += amt;
        stakers.push(a);
    }
    // Interleave premiums, losses and partial exits.
    let premium = c.receive_premium(&payer, &1_001);
    let mut covered = c.apply_loss(&s.pool, &97);
    for a in stakers.iter().step_by(3) {
        let sh = c.stake_of(a).active_shares;
        c.request_unstake(a, &(sh / 2 + 1));
    }
    covered += c.apply_loss(&s.pool, &131);
    s.advance(COOLDOWN);

    let mut paid = 0i128;
    for a in stakers.iter().step_by(3) {
        paid += c.execute_unstake(a);
    }
    covered += c.apply_loss(&s.pool, &3);
    for a in stakers.iter() {
        let sh = c.stake_of(a).active_shares;
        if sh > 0 {
            c.request_unstake(a, &sh);
        }
    }
    s.advance(COOLDOWN);
    for a in stakers.iter() {
        if c.stake_of(a).pending_shares > 0 {
            paid += c.execute_unstake(a);
        }
    }

    // Every stroop is accounted for: nothing stranded, nothing invented.
    let st = c.state();
    assert_eq!(st.total_shares, 0);
    assert_eq!(st.total_capital, 0);
    assert_eq!(s.token().balance(&s.id), 0);
    assert_eq!(covered, st.total_losses_covered);
    assert_eq!(s.token().balance(&s.pool), covered);
    assert_eq!(paid + covered, deposited + premium);
    let held: i128 = stakers.iter().map(|a| s.token().balance(a)).sum();
    assert_eq!(held, paid);
}

// ─── Premiums ───────────────────────────────────────────────────────────

#[test]
fn receive_premium_pulls_tokens_for_the_cut() {
    let s = Setup::new(); // 50%
    let c = s.client();
    let a = s.staker(1_000);
    let payer = s.staker(1_000);
    c.deposit(&a, &1_000);
    assert_eq!(
        c.try_receive_premium(&payer, &0),
        Err(Ok(BackstopError::InvalidAmount))
    );
    assert_eq!(c.receive_premium(&payer, &401), 200);
    assert_eq!(s.token().balance(&payer), 800);
    assert_eq!(s.token().balance(&s.id), 1_200);
    assert_eq!(c.state().premium_income, 200);
    assert_eq!(c.value_of(&a), 1_200);
}

#[test]
fn arithmetic_overflow_is_an_error_not_a_trap() {
    let s = Setup::with_bps(10_000);
    let c = s.client();
    let payer = Address::generate(&s.env);
    assert_eq!(
        c.try_receive_premium(&payer, &i128::MAX),
        Err(Ok(BackstopError::ArithmeticOverflow))
    );
}

// ─── Storage / TTL (issue #205) ─────────────────────────────────────────

/// Simulate `stellar contract restore` (RestoreFootprintOp): every archived
/// persistent entry for which `restore` returns true comes back with the
/// network's minimum persistent TTL. Native test contracts are re-registered
/// on the rebuilt env.
fn restore_archived(
    env: &Env,
    id: &Address,
    restore: impl Fn(&LedgerKey) -> bool,
) -> (Env, Address) {
    let mut snap = env.to_snapshot();
    let seq = snap.ledger.sequence_number;
    let min_ttl = snap.ledger.min_persistent_entry_ttl;
    for (key, (_, live_until)) in snap.ledger.ledger_entries.iter_mut() {
        if let Some(lu) = live_until {
            if *lu < seq && restore(key) {
                *live_until = Some(seq + min_ttl - 1);
            }
        }
    }
    let new_env = Env::from_snapshot(snap);
    new_env.mock_all_auths();
    let id = port(env, &new_env, id);
    new_env.register_contract(&id, RefractBackstop);
    (new_env, id)
}

/// Re-create `a` (an object owned by `from`) inside `to`.
fn port(from: &Env, to: &Env, a: &Address) -> Address {
    let v = ScVal::try_from_val(from, &a.to_val()).unwrap();
    Address::try_from_val(to, &v).unwrap()
}

/// True when the ledger entry for `key` exists but its TTL has run out.
/// (Touching an archived entry panics in the test host, so check the
/// snapshot instead of calling the contract.)
fn is_archived(env: &Env, key: &ScVal) -> bool {
    let snap = env.to_snapshot();
    let seq = snap.ledger.sequence_number;
    snap.ledger
        .ledger_entries
        .iter()
        .any(|(lk, (_, lu))| is_key(lk, key) && matches!(lu, Some(lu) if *lu < seq))
}

fn scval(env: &Env, key: &DataKey) -> ScVal {
    let v: Val = key.into_val(env);
    ScVal::try_from_val(env, &v).unwrap()
}

fn is_key(lk: &LedgerKey, key: &ScVal) -> bool {
    matches!(lk, LedgerKey::ContractData(d) if d.key == *key)
}

#[test]
fn every_entrypoint_extends_instance_and_touched_stake_ttl() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    let ttl_instance = || {
        s.env
            .as_contract(&s.id, || s.env.storage().instance().get_ttl())
    };
    let ttl_stake = |who: &Address| {
        s.env.as_contract(&s.id, || {
            s.env
                .storage()
                .persistent()
                .get_ttl(&DataKey::Stake(who.clone()))
        })
    };

    assert_eq!(ttl_instance(), INSTANCE_BUMP_AMOUNT);
    c.deposit(&a, &1_000);
    assert_eq!(ttl_stake(&a), PERSISTENT_BUMP_AMOUNT);

    // Within the threshold window nothing is re-extended (no wasted rent)...
    s.advance_ledgers(DAY_IN_LEDGERS / 2);
    c.stake_of(&a);
    assert_eq!(ttl_stake(&a), PERSISTENT_BUMP_AMOUNT - DAY_IN_LEDGERS / 2);
    assert_eq!(ttl_instance(), INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS / 2);

    // ...below it, any read extends back to the full amount.
    s.advance_ledgers(8 * DAY_IN_LEDGERS);
    assert!(ttl_stake(&a) < PERSISTENT_LIFETIME_THRESHOLD);
    c.value_of(&a);
    assert_eq!(ttl_stake(&a), PERSISTENT_BUMP_AMOUNT);
    assert_eq!(ttl_instance(), INSTANCE_BUMP_AMOUNT);

    // The permissionless bump entrypoints do the same without other effects.
    s.advance_ledgers(30 * DAY_IN_LEDGERS);
    assert!(c.extend_stake_ttl(&a));
    assert_eq!(ttl_stake(&a), PERSISTENT_BUMP_AMOUNT);
    assert_eq!(ttl_instance(), INSTANCE_BUMP_AMOUNT);
    assert!(!c.extend_stake_ttl(&Address::generate(&s.env)));
    s.advance_ledgers(2 * DAY_IN_LEDGERS);
    c.extend_ttl();
    assert_eq!(ttl_instance(), INSTANCE_BUMP_AMOUNT);

    // An exited staker's record is deleted, not left paying rent.
    c.request_unstake(&a, &1_000);
    s.advance(COOLDOWN);
    c.execute_unstake(&a);
    let gone = s.env.as_contract(&s.id, || {
        s.env.storage().persistent().has(&DataKey::Stake(a.clone()))
    });
    assert!(!gone);
}

#[test]
fn ttl_constants_are_consistent() {
    assert!(INSTANCE_LIFETIME_THRESHOLD < INSTANCE_BUMP_AMOUNT);
    assert!(PERSISTENT_LIFETIME_THRESHOLD < PERSISTENT_BUMP_AMOUNT);
    // Must fit under the network's max entry TTL (default 6_312_000).
    assert!(PERSISTENT_BUMP_AMOUNT <= 6_312_000);
}

#[test]
fn archived_stake_is_unreadable_until_restored_then_intact() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    c.deposit(&a, &1_000);

    // Everything (instance, stake, token entries) expires.
    s.advance_ledgers(PERSISTENT_BUMP_AMOUNT + 1);
    assert!(is_archived(
        &s.env,
        &scval(&s.env, &DataKey::Stake(a.clone()))
    ));
    assert!(is_archived(&s.env, &ScVal::LedgerKeyContractInstance));

    let (env, id) = restore_archived(&s.env, &s.id, |_| true);
    let a = port(&s.env, &env, &a);
    let c = RefractBackstopClient::new(&env, &id);
    assert_eq!(c.stake_of(&a).active_shares, 1_000);
    assert_eq!(c.value_of(&a), 1_000);
    // Restore only grants the minimum TTL; the first touch extends it fully.
    let ttl = env.as_contract(&id, || {
        env.storage()
            .persistent()
            .get_ttl(&DataKey::Stake(a.clone()))
    });
    assert_eq!(ttl, PERSISTENT_BUMP_AMOUNT);
}

#[test]
fn restore_after_loss_does_not_resurrect_pre_loss_balance() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker(1_000);
    let b = s.staker(1_000);
    c.deposit(&a, &1_000);
    c.deposit(&b, &1_000);
    s.advance_ledgers(PERSISTENT_BUMP_AMOUNT + 1);

    // Restore everything except A's record, then apply a loss while A is
    // still archived.
    let a_key = scval(&s.env, &DataKey::Stake(a.clone()));
    let (env, id) = restore_archived(&s.env, &s.id, |k| !is_key(k, &a_key));
    let c = RefractBackstopClient::new(&env, &id);
    assert!(is_archived(&env, &a_key));
    let pool = c.config().pool;
    let b = port(&s.env, &env, &b);
    c.apply_loss(&pool, &1_000);
    assert_eq!(c.value_of(&b), 500);

    // A comes back holding shares, so it is written down like everyone else.
    let (env2, id) = restore_archived(&env, &id, |_| true);
    let a = port(&s.env, &env2, &a);
    let env = env2;
    let c = RefractBackstopClient::new(&env, &id);
    assert_eq!(c.value_of(&a), 500);
    c.request_unstake(&a, &1_000);
    env.ledger().with_mut(|l| l.timestamp += COOLDOWN);
    assert_eq!(c.execute_unstake(&a), 500);
}

#[test]
fn hot_path_stays_within_transaction_budget() {
    let s = Setup::new();
    let c = s.client();
    // Populate many unrelated stakers first: cost must not depend on them.
    for _ in 0..50 {
        let x = s.staker(100);
        c.deposit(&x, &100);
    }
    let a = s.staker(1_000);
    c.deposit(&a, &500);
    s.advance_ledgers(10 * DAY_IN_LEDGERS); // force real TTL extensions

    for op in 0..4 {
        s.env.budget().reset_default();
        match op {
            0 => {
                c.deposit(&a, &500);
            }
            1 => {
                c.request_unstake(&a, &500);
            }
            2 => {
                c.apply_loss(&s.pool, &1_000);
            }
            _ => {
                s.advance(COOLDOWN);
                c.execute_unstake(&a);
            }
        }
        // Soroban per-transaction limits (protocol 21): 100M instructions,
        // 40 MiB memory. Stay well inside them.
        let cpu = s.env.budget().cpu_instruction_cost();
        let mem = s.env.budget().memory_bytes_cost();
        assert!(cpu < 10_000_000, "op {op}: cpu {cpu}");
        assert!(mem < 4 * 1024 * 1024, "op {op}: mem {mem}");
    }
}
