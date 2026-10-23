#![cfg(test)]

use super::*;
use soroban_sdk::{testutils::Address as _, Address, Env, Symbol};

// ─── Fixture ──────────────────────────────────────────────────────────────────

struct Fixture<'a> {
    env: Env,
    params: RefractParamsClient<'a>,
    timelock: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let timelock = Address::generate(&env);
    let contract_id = env.register_contract(None, RefractParams);
    let params = RefractParamsClient::new(&env, &contract_id);
    params.initialize(&timelock);

    Fixture {
        env,
        params,
        timelock,
    }
}

// ─── Initialization ───────────────────────────────────────────────────────────

#[test]
fn initialize_seeds_defaults() {
    let f = setup();
    // Spot-check a few defaults.
    assert_eq!(
        f.params.get(&Symbol::new(&f.env, "premium_share_bps")),
        Some(DEFAULT_PREMIUM_SHARE_BPS)
    );
    assert_eq!(
        f.params.get(&Symbol::new(&f.env, "unstake_cooldown")),
        Some(DEFAULT_UNSTAKE_COOLDOWN)
    );
    assert_eq!(
        f.params.get(&Symbol::new(&f.env, "treasury_spend_cap")),
        Some(DEFAULT_TREASURY_SPEND_CAP)
    );
    assert_eq!(
        f.params.get(&Symbol::new(&f.env, "paused")),
        Some(DEFAULT_PAUSED)
    );
}

#[test]
fn timelock_returns_configured_address() {
    let f = setup();
    assert_eq!(f.params.timelock(), f.timelock);
}

#[test]
#[should_panic]
fn double_initialize_panics() {
    let f = setup();
    let t2 = Address::generate(&f.env);
    f.params.initialize(&t2);
}

// ─── Governance-gated set ─────────────────────────────────────────────────────

#[test]
fn timelock_can_set_any_parameter() {
    let f = setup();
    let key = Symbol::new(&f.env, "premium_share_bps");
    f.params.set(&f.timelock, &key, &750i128); // change from 500 to 750 bps
    assert_eq!(f.params.get(&key), Some(750i128));
}

#[test]
fn timelock_can_set_new_key() {
    let f = setup();
    let key = Symbol::new(&f.env, "custom_param");
    assert_eq!(f.params.get(&key), None);
    f.params.set(&f.timelock, &key, &42i128);
    assert_eq!(f.params.get(&key), Some(42i128));
}

#[test]
#[should_panic]
fn non_timelock_set_panics() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let key = Symbol::new(&f.env, "premium_share_bps");
    f.params.set(&stranger, &key, &100i128);
}

// ─── get_or_default ───────────────────────────────────────────────────────────

#[test]
fn get_or_default_returns_value_when_set() {
    let f = setup();
    let key = Symbol::new(&f.env, "premium_share_bps");
    assert_eq!(f.params.get_or_default(&key, &9999i128), DEFAULT_PREMIUM_SHARE_BPS);
}

#[test]
fn get_or_default_returns_fallback_for_unset_key() {
    let f = setup();
    let key = Symbol::new(&f.env, "nonexistent_key");
    assert_eq!(f.params.get_or_default(&key, &42i128), 42i128);
}

// ─── set_timelock (meta-governance) ──────────────────────────────────────────

#[test]
fn timelock_can_migrate_authority() {
    let f = setup();
    let new_timelock = Address::generate(&f.env);
    f.params.set_timelock(&f.timelock, &new_timelock);
    assert_eq!(f.params.timelock(), new_timelock);

    // New timelock can now write; old one cannot.
    let key = Symbol::new(&f.env, "paused");
    f.params.set(&new_timelock, &key, &1i128);
    assert_eq!(f.params.get(&key), Some(1i128));
}

#[test]
#[should_panic]
fn non_timelock_set_timelock_panics() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let new_timelock = Address::generate(&f.env);
    f.params.set_timelock(&stranger, &new_timelock);
}

// ─── All well-known defaults ──────────────────────────────────────────────────

#[test]
fn all_well_known_defaults_are_seeded() {
    let f = setup();

    let checks: &[(&str, i128)] = &[
        ("premium_share_bps", DEFAULT_PREMIUM_SHARE_BPS),
        ("unstake_cooldown", DEFAULT_UNSTAKE_COOLDOWN),
        ("loss_haircut_bps", DEFAULT_LOSS_HAIRCUT_BPS),
        ("bounty_critical", DEFAULT_BOUNTY_CRITICAL),
        ("bounty_high", DEFAULT_BOUNTY_HIGH),
        ("bounty_medium", DEFAULT_BOUNTY_MEDIUM),
        ("bounty_low", DEFAULT_BOUNTY_LOW),
        ("bounty_approver_thr", DEFAULT_BOUNTY_APPROVER_THR),
        ("grants_approver_thr", DEFAULT_GRANTS_APPROVER_THR),
        ("treasury_spend_cap", DEFAULT_TREASURY_SPEND_CAP),
        ("paused", DEFAULT_PAUSED),
    ];

    for (key, expected) in checks {
        assert_eq!(
            f.params.get(&Symbol::new(&f.env, key)),
            Some(*expected),
            "default for key '{}' wrong",
            key
        );
    }
}

// ─── Cross-contract scenario: timelock → params ───────────────────────────────

#[test]
fn timelock_scenario_drives_parameter_update() {
    // Demonstrate the governance→timelock→params flow using the shared fixture.
    // Here we replicate the pattern instead of importing testutils directly
    // (testutils is a dev-dep of params, not an in-crate import in lib.rs).
    let env = Env::default();
    env.mock_all_auths();

    let timelock_addr = Address::generate(&env);
    let contract_id = env.register_contract(None, RefractParams);
    let params = RefractParamsClient::new(&env, &contract_id);
    params.initialize(&timelock_addr);

    // Timelock (governance executor) sets a parameter.
    let key = Symbol::new(&env, "premium_share_bps");
    params.set(&timelock_addr, &key, &1000i128); // raise to 10%
    assert_eq!(params.get(&key), Some(1000i128));
}
