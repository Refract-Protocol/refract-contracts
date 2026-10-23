#![cfg(test)]
extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, BytesN, Env,
};

use crate::{BountyError, RefractBountyEscrow, RefractBountyEscrowClient};

fn create_bounty(env: &Env) -> (RefractBountyEscrowClient, Address, Address) {
    let admin = Address::generate(env);
    let governance = Address::generate(env);

    // Deploy a SAC token and mint to the governance address so we can fund.
    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let sac = token::StellarAssetClient::new(env, &token_id.address());
    sac.mint(&governance, &1_000_000i128);

    let contract_id = env.register(RefractBountyEscrow, ());
    let client = RefractBountyEscrowClient::new(env, &contract_id);
    client.initialize(&governance, &token_id.address()).unwrap();

    // Fund the escrow.
    client.fund(&governance, &500_000i128).unwrap();

    (client, governance, token_id.address())
}

fn hash(env: &Env, seed: u8) -> BytesN<32> {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    BytesN::from_array(env, &bytes)
}

// ── Velocity control: spacing ─────────────────────────────────────────────────

#[test]
fn award_succeeds_without_velocity_config() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    let researcher = Address::generate(&env);
    client
        .award(&governance, &researcher, &10_000i128, &hash(&env, 1))
        .unwrap();
    let balance = client.balance().unwrap();
    assert_eq!(balance, 490_000i128);
}

#[test]
fn spacing_rejects_second_award_within_interval() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    // Set 3600-second minimum spacing.
    client
        .set_velocity_config(&governance, &3600u64, &0i128, &3600u64, &0i128)
        .unwrap();

    let researcher = Address::generate(&env);
    client
        .award(&governance, &researcher, &1_000i128, &hash(&env, 1))
        .unwrap();

    // Same researcher, different hash, but spacing not elapsed.
    let err = client
        .award(&governance, &researcher, &1_000i128, &hash(&env, 2))
        .unwrap_err();
    assert_eq!(err, BountyError::SpacingNotElapsed);
}

#[test]
fn spacing_allows_award_after_interval_elapsed() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    client
        .set_velocity_config(&governance, &3600u64, &0i128, &3600u64, &0i128)
        .unwrap();

    let researcher = Address::generate(&env);
    client
        .award(&governance, &researcher, &1_000i128, &hash(&env, 1))
        .unwrap();

    // Advance time past spacing interval.
    env.ledger().with_mut(|l| l.timestamp += 4_000);

    client
        .award(&governance, &researcher, &1_000i128, &hash(&env, 2))
        .unwrap();
}

// ── Velocity control: epoch cap ───────────────────────────────────────────────

#[test]
fn epoch_cap_rejects_payout_exceeding_per_epoch_limit() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    // epoch_secs=10_000, epoch_cap=5_000, no spacing, no rolling cap.
    client
        .set_velocity_config(&governance, &10_000u64, &5_000i128, &0u64, &0i128)
        .unwrap();

    let researcher = Address::generate(&env);
    // First award: 3_000 → within cap.
    client
        .award(&governance, &researcher, &3_000i128, &hash(&env, 1))
        .unwrap();

    // Second award: 3_000 more → would push outflow to 6_000 > 5_000.
    let err = client
        .award(&governance, &researcher, &3_000i128, &hash(&env, 2))
        .unwrap_err();
    assert_eq!(err, BountyError::EpochCapExceeded);
}

#[test]
fn epoch_cap_resets_after_window_expires() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    client
        .set_velocity_config(&governance, &10_000u64, &5_000i128, &0u64, &0i128)
        .unwrap();

    let researcher = Address::generate(&env);
    client
        .award(&governance, &researcher, &4_000i128, &hash(&env, 1))
        .unwrap();

    // Advance time past epoch window.
    env.ledger().with_mut(|l| l.timestamp += 11_000);

    // Window resets, so the cap applies fresh.
    client
        .award(&governance, &researcher, &4_000i128, &hash(&env, 2))
        .unwrap();
}

// ── Velocity control: rolling window cap ─────────────────────────────────────

#[test]
fn rolling_window_cap_rejects_excess_outflow() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    // rolling_window_cap=8_000, no epoch cap.
    client
        .set_velocity_config(&governance, &10_000u64, &0i128, &0u64, &8_000i128)
        .unwrap();

    let researcher = Address::generate(&env);
    client
        .award(&governance, &researcher, &5_000i128, &hash(&env, 1))
        .unwrap();
    // 5_000 + 5_000 = 10_000 > 8_000 → should fail.
    let err = client
        .award(&governance, &researcher, &5_000i128, &hash(&env, 2))
        .unwrap_err();
    assert_eq!(err, BountyError::RollingCapExceeded);
}

// ── Velocity limits don't affect different recipients ─────────────────────────

#[test]
fn velocity_limits_are_per_recipient() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_bounty(&env);

    client
        .set_velocity_config(&governance, &10_000u64, &5_000i128, &3600u64, &0i128)
        .unwrap();

    let r1 = Address::generate(&env);
    let r2 = Address::generate(&env);

    // Hit r1's epoch cap.
    client
        .award(&governance, &r1, &5_000i128, &hash(&env, 1))
        .unwrap();
    // r2 is unaffected.
    client
        .award(&governance, &r2, &5_000i128, &hash(&env, 2))
        .unwrap();
}

// ── Governance gating for velocity config ────────────────────────────────────

#[test]
fn only_governance_can_set_velocity_config() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _, _) = create_bounty(&env);
    let stranger = Address::generate(&env);
    let err = client
        .set_velocity_config(&stranger, &3600u64, &1_000i128, &0u64, &0i128)
        .unwrap_err();
    assert_eq!(err, BountyError::Unauthorized);
}
