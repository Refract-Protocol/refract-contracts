#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Vec,
};

// ─── Fixture ──────────────────────────────────────────────────────────────────

const ONE_USDC: i128 = 10_000_000; // 1e7 fixed-point

struct Fixture<'a> {
    env: Env,
    grants: RefractGrantsClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    governance: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let governance = Address::generate(&env);

    // Deploy a SAC-backed token as the grant disbursement token.
    let token_owner = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(token_owner);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let contract_id = env.register_contract(None, RefractGrants);
    let grants = RefractGrantsClient::new(&env, &contract_id);

    grants.initialize(&governance, &sac.address());

    Fixture {
        env,
        grants,
        token,
        token_admin,
        governance,
    }
}

/// Mint `amount` tokens to `recipient` and fund the grants escrow.
fn fund_escrow(f: &Fixture, funder: &Address, amount: i128) {
    f.token_admin.mint(funder, &amount);
    f.grants.fund(funder, &amount);
}

// ─── Initialization tests ─────────────────────────────────────────────────────

#[test]
fn initialize_sets_governance_and_token() {
    let f = setup();
    assert_eq!(f.grants.governance(), f.governance);
}

#[test]
#[should_panic]
fn double_initialize_panics() {
    let f = setup();
    let g2 = Address::generate(&f.env);
    let t2 = Address::generate(&f.env);
    // Second initialize should trap with AlreadyInitialized.
    f.grants.initialize(&g2, &t2);
}

// ─── Funding tests ────────────────────────────────────────────────────────────

#[test]
fn fund_transfers_tokens_into_escrow() {
    let f = setup();
    let funder = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(10 * ONE_USDC));

    let escrow = f.grants.address.clone();
    assert_eq!(f.token.balance(&escrow), 0);

    f.grants.fund(&funder, &(10 * ONE_USDC));

    assert_eq!(f.token.balance(&escrow), 10 * ONE_USDC);
    assert_eq!(f.token.balance(&funder), 0);
}

#[test]
#[should_panic]
fn fund_zero_panics() {
    let f = setup();
    let funder = Address::generate(&f.env);
    f.grants.fund(&funder, &0);
}

// ─── Grant creation tests ─────────────────────────────────────────────────────

#[test]
fn create_grant_returns_sequential_ids() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let milestones_a: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC]);
    let milestones_b: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC, ONE_USDC]);
    fund_escrow(&f, &Address::generate(&f.env), 10 * ONE_USDC);

    let id_a = f
        .grants
        .create_grant(&f.governance, &recipient, &ONE_USDC, &milestones_a);
    let id_b = f
        .grants
        .create_grant(&f.governance, &recipient, &(2 * ONE_USDC), &milestones_b);

    assert_eq!(id_a, 0u64);
    assert_eq!(id_b, 1u64);
}

#[test]
#[should_panic]
fn create_grant_unauthorized_panics() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC]);
    f.grants
        .create_grant(&stranger, &recipient, &ONE_USDC, &milestones);
}

#[test]
#[should_panic]
fn create_grant_zero_total_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [0]);
    f.grants
        .create_grant(&f.governance, &recipient, &0, &milestones);
}

#[test]
#[should_panic]
fn create_grant_empty_milestones_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let milestones: Vec<i128> = Vec::new(&f.env);
    f.grants
        .create_grant(&f.governance, &recipient, &ONE_USDC, &milestones);
}

#[test]
#[should_panic]
fn create_grant_sum_mismatch_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    // 2 milestones totaling 1.5 USDC but declared total is 2 USDC.
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC / 2, ONE_USDC]);
    f.grants
        .create_grant(&f.governance, &recipient, &(2 * ONE_USDC), &milestones);
}

// ─── Confirm milestone tests ──────────────────────────────────────────────────

#[test]
fn confirm_milestone_makes_amount_claimable() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), 3 * ONE_USDC);

    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC, ONE_USDC, ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &(3 * ONE_USDC), &milestones);

    assert_eq!(f.grants.claimable(&id), 0);
    f.grants.confirm_milestone(&f.governance, &id, &0u32);
    assert_eq!(f.grants.claimable(&id), ONE_USDC);
}

#[test]
#[should_panic]
fn confirm_out_of_order_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), 2 * ONE_USDC);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC, ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &(2 * ONE_USDC), &milestones);
    // Cannot confirm index 1 before index 0.
    f.grants.confirm_milestone(&f.governance, &id, &1u32);
}

#[test]
#[should_panic]
fn confirm_already_confirmed_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), ONE_USDC);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &ONE_USDC, &milestones);
    f.grants.confirm_milestone(&f.governance, &id, &0u32);
    // Second confirmation on same index should fail.
    f.grants.confirm_milestone(&f.governance, &id, &0u32);
}

// ─── Claim + SEP-41 transfer tests ───────────────────────────────────────────

#[test]
fn claim_transfers_tokens_to_recipient() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), 3 * ONE_USDC);

    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC, ONE_USDC, ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &(3 * ONE_USDC), &milestones);

    // Confirm first milestone, then claim.
    f.grants.confirm_milestone(&f.governance, &id, &0u32);

    let escrow_before = f.token.balance(&f.grants.address);
    let recipient_before = f.token.balance(&recipient);

    let released = f.grants.claim(&recipient, &id);

    assert_eq!(released, ONE_USDC);
    // SEP-41 debit from escrow.
    assert_eq!(
        f.token.balance(&f.grants.address),
        escrow_before - ONE_USDC
    );
    // SEP-41 credit to recipient.
    assert_eq!(f.token.balance(&recipient), recipient_before + ONE_USDC);
    // claimable resets to zero.
    assert_eq!(f.grants.claimable(&id), 0);
}

#[test]
fn full_lifecycle_create_confirm_claim() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), 3 * ONE_USDC);

    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC, ONE_USDC, ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &(3 * ONE_USDC), &milestones);

    // Nothing claimable before any confirmation.
    assert_eq!(f.grants.claimable(&id), 0);

    // Confirm milestone 0, claim, then confirm 1 & 2 and claim again.
    f.grants.confirm_milestone(&f.governance, &id, &0u32);
    assert_eq!(f.grants.claimable(&id), ONE_USDC);

    let r1 = f.grants.claim(&recipient, &id);
    assert_eq!(r1, ONE_USDC);
    assert_eq!(f.grants.claimable(&id), 0);

    f.grants.confirm_milestone(&f.governance, &id, &1u32);
    f.grants.confirm_milestone(&f.governance, &id, &2u32);
    assert_eq!(f.grants.claimable(&id), 2 * ONE_USDC);

    let r2 = f.grants.claim(&recipient, &id);
    assert_eq!(r2, 2 * ONE_USDC);
    assert_eq!(f.grants.claimable(&id), 0);

    // Total tokens received equals total grant.
    assert_eq!(f.token.balance(&recipient), 3 * ONE_USDC);
    // Escrow is fully drained.
    assert_eq!(f.token.balance(&f.grants.address), 0);
}

#[test]
fn claim_releases_only_confirmed_amounts() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), 5 * ONE_USDC);

    let milestones: Vec<i128> = Vec::from_array(&f.env, [2 * ONE_USDC, 3 * ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &(5 * ONE_USDC), &milestones);

    f.grants.confirm_milestone(&f.governance, &id, &0u32);
    // Only the first milestone is confirmed, so only 2 USDC is released.
    let r = f.grants.claim(&recipient, &id);
    assert_eq!(r, 2 * ONE_USDC);
    assert_eq!(f.token.balance(&recipient), 2 * ONE_USDC);
    // Unconfirmed milestone's funds stay in escrow.
    assert_eq!(f.token.balance(&f.grants.address), 3 * ONE_USDC);
}

#[test]
#[should_panic]
fn claim_nothing_to_claim_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &ONE_USDC, &milestones);
    // No milestone confirmed yet — nothing to claim.
    f.grants.claim(&recipient, &id);
}

#[test]
#[should_panic]
fn claim_wrong_recipient_panics() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);
    fund_escrow(&f, &Address::generate(&f.env), ONE_USDC);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &ONE_USDC, &milestones);
    f.grants.confirm_milestone(&f.governance, &id, &0u32);
    // Wrong address — should fail with NotRecipient.
    f.grants.claim(&stranger, &id);
}

// ─── get_grant read path ──────────────────────────────────────────────────────

#[test]
fn get_grant_returns_none_for_unknown_id() {
    let f = setup();
    assert!(f.grants.get_grant(&999u64).is_none());
}

#[test]
fn get_grant_reflects_confirmed_state() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let milestones: Vec<i128> = Vec::from_array(&f.env, [ONE_USDC]);
    let id = f
        .grants
        .create_grant(&f.governance, &recipient, &ONE_USDC, &milestones);
    fund_escrow(&f, &Address::generate(&f.env), ONE_USDC);
    f.grants.confirm_milestone(&f.governance, &id, &0u32);
    let grant = f.grants.get_grant(&id).unwrap();
    assert_eq!(grant.claimable, ONE_USDC);
    assert!(grant.milestones.get(0).unwrap().confirmed);
}
