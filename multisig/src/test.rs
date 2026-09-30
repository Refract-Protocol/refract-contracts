#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    Address, Env, Symbol, Val, Vec,
};

// ── Minimal stub target ───────────────────────────────────────────────────────

mod stub {
    use soroban_sdk::{contract, contractimpl, Env, Symbol};

    #[contract]
    pub struct Stub;

    #[contractimpl]
    impl Stub {
        pub fn set_value(env: Env, value: i128) -> i128 {
            env.storage().instance().set(&Symbol::new(&env, "v"), &value);
            value
        }

        pub fn get_value(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "v"))
                .unwrap_or(0)
        }
    }
}

use stub::StubClient;

// ── Setup helpers ─────────────────────────────────────────────────────────────

fn make_owners(env: &Env, n: usize) -> Vec<Address> {
    let mut v: Vec<Address> = Vec::new(env);
    for _ in 0..n {
        v.push_back(Address::generate(env));
    }
    v
}

fn stub_args(env: &Env, value: i128) -> Vec<Val> {
    let mut args: Vec<Val> = Vec::new(env);
    args.push_back(value.into_val(env));
    args
}

fn owner_at(owners: &Vec<Address>, i: u32) -> Address {
    owners.get(i).unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn double_initialize_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let owners = make_owners(&env, 2);
    ms.initialize(&owners, &1u32);
    let owners2 = make_owners(&env, 2);
    let res = ms.try_initialize(&owners2, &1u32);
    assert_eq!(res, Err(Ok(MultisigError::AlreadyInitialized)));
}

#[test]
fn invalid_threshold_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let owners = make_owners(&env, 2);
    assert_eq!(ms.try_initialize(&owners, &5u32), Err(Ok(MultisigError::InvalidThreshold)));
    assert_eq!(ms.try_initialize(&owners, &0u32), Err(Ok(MultisigError::InvalidThreshold)));
}

#[test]
fn below_threshold_execute_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 3);
    ms.initialize(&owners, &2u32); // 2-of-3

    let owner0 = owner_at(&owners, 0);
    let id = ms.propose(&owner0, &target_id, &Symbol::new(&env, "set_value"), &stub_args(&env, 10));

    // Only 1 approval (proposer auto-approves) but need 2.
    let res = ms.try_execute(&id);
    assert_eq!(res, Err(Ok(MultisigError::Unauthorized)));
}

#[test]
fn threshold_met_execute_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 3);
    ms.initialize(&owners, &2u32); // 2-of-3

    let owner0 = owner_at(&owners, 0);
    let owner1 = owner_at(&owners, 1);

    let id = ms.propose(&owner0, &target_id, &Symbol::new(&env, "set_value"), &stub_args(&env, 99));
    ms.approve(&owner1, &id);
    ms.execute(&id);

    let stub = StubClient::new(&env, &target_id);
    assert_eq!(stub.get_value(), 99);
}

#[test]
fn double_approval_does_not_inflate_count() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 3);
    ms.initialize(&owners, &2u32); // 2-of-3

    let owner0 = owner_at(&owners, 0);

    let id = ms.propose(&owner0, &target_id, &Symbol::new(&env, "set_value"), &stub_args(&env, 5));
    // owner0 approves again — should be no-op.
    ms.approve(&owner0, &id);

    // Still only 1 approval.
    let proposal = ms.get_proposal(&id).unwrap();
    assert_eq!(proposal.approvals.len(), 1);

    // Execute should still be rejected (need 2).
    let res = ms.try_execute(&id);
    assert_eq!(res, Err(Ok(MultisigError::Unauthorized)));
}

#[test]
fn executed_proposal_cannot_be_re_executed() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 2);
    ms.initialize(&owners, &1u32); // 1-of-2

    let owner0 = owner_at(&owners, 0);
    let id = ms.propose(&owner0, &target_id, &Symbol::new(&env, "set_value"), &stub_args(&env, 3));
    ms.execute(&id);

    let res = ms.try_execute(&id);
    assert_eq!(res, Err(Ok(MultisigError::ProposalNotPending)));
}

#[test]
fn non_owner_cannot_propose() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 2);
    ms.initialize(&owners, &1u32);

    let stranger = Address::generate(&env);
    let res = ms.try_propose(
        &stranger,
        &target_id,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
    );
    assert_eq!(res, Err(Ok(MultisigError::NotOwner)));
}

#[test]
fn non_owner_cannot_approve() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 3);
    ms.initialize(&owners, &2u32); // 2-of-3

    let owner0 = owner_at(&owners, 0);
    let stranger = Address::generate(&env);

    let id = ms.propose(&owner0, &target_id, &Symbol::new(&env, "set_value"), &stub_args(&env, 1));

    let res = ms.try_approve(&stranger, &id);
    assert_eq!(res, Err(Ok(MultisigError::NotOwner)));
}

#[test]
fn proposer_approval_is_auto_counted() {
    let env = Env::default();
    env.mock_all_auths();
    let ms_id = env.register_contract(None, RefractMultisig);
    let ms = RefractMultisigClient::new(&env, &ms_id);
    let target_id = env.register_contract(None, stub::Stub);
    let owners = make_owners(&env, 2);
    ms.initialize(&owners, &1u32); // 1-of-2

    let owner0 = owner_at(&owners, 0);
    let id = ms.propose(&owner0, &target_id, &Symbol::new(&env, "set_value"), &stub_args(&env, 77));

    // Should execute immediately without extra approve() calls.
    ms.execute(&id);

    let stub = StubClient::new(&env, &target_id);
    assert_eq!(stub.get_value(), 77);
}
