#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};

fn measure<T>(env: &Env, ep: &str, f: impl FnOnce() -> T) -> T {
    env.budget().reset_default();
    let r = f();
    std::println!(
        "BUDGET refract_backstop {ep} {} {}",
        env.budget().cpu_instruction_cost(),
        env.budget().memory_bytes_cost()
    );
    r
}

#[test]
fn deposit_withdraw_cover_round_trip() {
    let env = Env::default();
    env.mock_all_auths();
    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token = TokenClient::new(&env, &sac.address());
    let id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &id);
    client.initialize(&pool, &admin, &sac.address(), &2_000);

    let staker = Address::generate(&env);
    StellarAssetClient::new(&env, &sac.address()).mint(&staker, &1_000);
    assert_eq!(client.deposit(&staker, &1_000), 1_000);
    let outsider = Address::generate(&env);
    assert_eq!(
        client.try_cover_shortfall(&outsider, &10),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(client.cover_shortfall(&pool, &400), 400);
    assert_eq!(token.balance(&pool), 400);
    // Share price dropped: 1000 shares now redeem 600.
    assert_eq!(client.withdraw(&staker, &1_000), 600);
}

/// Resource benchmark (issue #221): prints one `BUDGET` line per
/// state-changing entrypoint for `cargo xtask budget-check`.
#[test]
fn budget_entrypoints() {
    let env = Env::default();
    env.mock_all_auths();
    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &id);
    let staker = Address::generate(&env);
    StellarAssetClient::new(&env, &sac.address()).mint(&staker, &10_000);

    measure(&env, "initialize", || {
        client.initialize(&pool, &admin, &sac.address(), &2_000)
    });
    measure(&env, "deposit", || client.deposit(&staker, &5_000));
    measure(&env, "receive_premium", || client.receive_premium(&1_000));
    measure(&env, "cover_shortfall", || {
        client.cover_shortfall(&pool, &1_000)
    });
    measure(&env, "withdraw", || client.withdraw(&staker, &1_000));
}
