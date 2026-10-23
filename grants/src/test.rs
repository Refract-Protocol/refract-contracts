#![cfg(test)]
extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, vec, Address, Env, Vec,
};

use crate::{GrantsError, RefractGrants, RefractGrantsClient};

fn create_grants(env: &Env) -> (RefractGrantsClient, Address, Address) {
    let admin = Address::generate(env);
    let governance = Address::generate(env);

    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let sac = token::StellarAssetClient::new(env, &token_id.address());

    let contract_id = env.register(RefractGrants, ());
    let client = RefractGrantsClient::new(env, &contract_id);
    client.initialize(&governance, &token_id.address()).unwrap();

    // Mint funds to the contract so it can disburse.
    sac.mint(&contract_id, &1_000_000i128);

    (client, governance, token_id.address())
}

// ── Lifecycle tests ───────────────────────────────────────────────────────────

#[test]
fn full_lifecycle_create_confirm_claim() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    let id = client
        .create_grant(&governance, &recipient, &300i128, &vec![&env, 100i128, 100i128, 100i128])
        .unwrap();

    assert_eq!(client.claimable(&id), Some(0i128));

    client.confirm_milestone(&governance, &id, &0u32).unwrap();
    assert_eq!(client.claimable(&id), Some(100i128));

    client.claim(&recipient, &id).unwrap();
    assert_eq!(client.claimable(&id), Some(0i128));

    client.confirm_milestone(&governance, &id, &1u32).unwrap();
    client.confirm_milestone(&governance, &id, &2u32).unwrap();
    assert_eq!(client.claimable(&id), Some(200i128));

    client.claim(&recipient, &id).unwrap();
    assert_eq!(client.claimable(&id), Some(0i128));
}

#[test]
fn rejects_milestone_sum_mismatch() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    let err = client
        .create_grant(&governance, &recipient, &300i128, &vec![&env, 100i128, 100i128])
        .unwrap_err();
    assert_eq!(err, GrantsError::MilestoneSumMismatch);
}

#[test]
fn rejects_zero_milestones() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    let empty: Vec<i128> = vec![&env];
    let err = client
        .create_grant(&governance, &recipient, &100i128, &empty)
        .unwrap_err();
    assert_eq!(err, GrantsError::NoMilestones);
}

#[test]
fn rejects_out_of_order_confirmation() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    let id = client
        .create_grant(&governance, &recipient, &300i128, &vec![&env, 100i128, 100i128, 100i128])
        .unwrap();

    let err = client
        .confirm_milestone(&governance, &id, &1u32)
        .unwrap_err();
    assert_eq!(err, GrantsError::MilestoneOutOfOrder);
}

#[test]
fn enforces_governance_gating() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);

    let err = client
        .create_grant(&stranger, &recipient, &100i128, &vec![&env, 100i128])
        .unwrap_err();
    assert_eq!(err, GrantsError::Unauthorized);

    let id = client
        .create_grant(&governance, &recipient, &100i128, &vec![&env, 100i128])
        .unwrap();
    let err = client
        .confirm_milestone(&stranger, &id, &0u32)
        .unwrap_err();
    assert_eq!(err, GrantsError::Unauthorized);
}

#[test]
fn claim_is_recipient_only() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);

    let id = client
        .create_grant(&governance, &recipient, &100i128, &vec![&env, 100i128])
        .unwrap();
    client.confirm_milestone(&governance, &id, &0u32).unwrap();

    let err = client.claim(&stranger, &id).unwrap_err();
    assert_eq!(err, GrantsError::NotRecipient);

    client.claim(&recipient, &id).unwrap();
}

// ── Velocity control: spacing ─────────────────────────────────────────────────

#[test]
fn spacing_rejects_second_claim_within_interval() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    // 3600-second minimum spacing between claims.
    client
        .set_velocity_config(&governance, &3600u64, &0i128, &3600u64, &0i128)
        .unwrap();

    // Grant with two milestones.
    let id = client
        .create_grant(&governance, &recipient, &200i128, &vec![&env, 100i128, 100i128])
        .unwrap();
    client.confirm_milestone(&governance, &id, &0u32).unwrap();
    client.claim(&recipient, &id).unwrap();

    // Confirm second milestone and try to claim immediately.
    client.confirm_milestone(&governance, &id, &1u32).unwrap();
    let err = client.claim(&recipient, &id).unwrap_err();
    assert_eq!(err, GrantsError::SpacingNotElapsed);
}

#[test]
fn spacing_allows_claim_after_interval() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    client
        .set_velocity_config(&governance, &3600u64, &0i128, &3600u64, &0i128)
        .unwrap();

    let id = client
        .create_grant(&governance, &recipient, &200i128, &vec![&env, 100i128, 100i128])
        .unwrap();
    client.confirm_milestone(&governance, &id, &0u32).unwrap();
    client.claim(&recipient, &id).unwrap();

    client.confirm_milestone(&governance, &id, &1u32).unwrap();
    env.ledger().with_mut(|l| l.timestamp += 4_000);
    client.claim(&recipient, &id).unwrap();
}

// ── Velocity control: epoch cap ───────────────────────────────────────────────

#[test]
fn epoch_cap_rejects_claims_exceeding_limit() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    // epoch_secs=10_000, epoch_cap=150 → first 100 ok, second 100 exceeds.
    client
        .set_velocity_config(&governance, &10_000u64, &150i128, &0u64, &0i128)
        .unwrap();

    let id = client
        .create_grant(&governance, &recipient, &200i128, &vec![&env, 100i128, 100i128])
        .unwrap();
    client.confirm_milestone(&governance, &id, &0u32).unwrap();
    client.claim(&recipient, &id).unwrap();

    client.confirm_milestone(&governance, &id, &1u32).unwrap();
    let err = client.claim(&recipient, &id).unwrap_err();
    assert_eq!(err, GrantsError::EpochCapExceeded);
}

#[test]
fn epoch_cap_resets_after_window_expires() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    client
        .set_velocity_config(&governance, &10_000u64, &150i128, &0u64, &0i128)
        .unwrap();

    let id = client
        .create_grant(&governance, &recipient, &400i128, &vec![&env, 100i128, 100i128, 100i128, 100i128])
        .unwrap();

    client.confirm_milestone(&governance, &id, &0u32).unwrap();
    client.claim(&recipient, &id).unwrap(); // 100 in window

    // Advance past the window; cap resets.
    env.ledger().with_mut(|l| l.timestamp += 11_000);

    client.confirm_milestone(&governance, &id, &1u32).unwrap();
    client.claim(&recipient, &id).unwrap(); // fresh window, 100 ok
}

// ── Velocity control: rolling window cap ─────────────────────────────────────

#[test]
fn rolling_window_cap_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _) = create_grants(&env);
    let recipient = Address::generate(&env);

    client
        .set_velocity_config(&governance, &10_000u64, &0i128, &0u64, &150i128)
        .unwrap();

    let id = client
        .create_grant(&governance, &recipient, &200i128, &vec![&env, 100i128, 100i128])
        .unwrap();

    client.confirm_milestone(&governance, &id, &0u32).unwrap();
    client.claim(&recipient, &id).unwrap();

    client.confirm_milestone(&governance, &id, &1u32).unwrap();
    let err = client.claim(&recipient, &id).unwrap_err();
    assert_eq!(err, GrantsError::RollingCapExceeded);
}

// ── Governance gating for velocity config ────────────────────────────────────

#[test]
fn only_governance_can_set_velocity_config() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _, _) = create_grants(&env);
    let stranger = Address::generate(&env);

    let err = client
        .set_velocity_config(&stranger, &3600u64, &1_000i128, &0u64, &0i128)
        .unwrap_err();
    assert_eq!(err, GrantsError::Unauthorized);
}
