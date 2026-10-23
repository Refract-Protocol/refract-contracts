//! Shared Test Harness for Refract Contracts Authorization Testing
//!
//! Provides common fixture setup, multi-contract topologies, and cross-contract
//! invocation forwarding contracts designed to test authorization trees and sub-invocation
//! delegation boundaries under Soroban SDK.

#![cfg_attr(not(feature = "testutils"), no_std)]

use soroban_sdk::{
    contract, contractimpl, token,
    testutils::Address as _,
    Address, Env, IntoVal, Symbol, Vec,
};
use refract_backstop::{RefractBackstop, RefractBackstopClient};
use refract_bounty::{RefractBountyEscrow, RefractBountyEscrowClient};
use refract_grants::{RefractGrants, RefractGrantsClient};
use refract_treasury::{RefractTreasury, RefractTreasuryClient};

/// Test contract simulating a Treasury or Orchestrator that attempts cross-contract
/// invocations into Grants and Backstop.
#[contract]
pub struct CrossContractTreasury;

#[contractimpl]
impl CrossContractTreasury {
    /// Calls Grants to create a grant, then calls Backstop to cover a shortfall.
    /// Used to test 3-layer authorization chains:
    /// Caller -> CrossContractTreasury -> RefractGrants -> RefractBackstop.
    pub fn chain_call(
        env: Env,
        caller: Address,
        grants: Address,
        backstop: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        shortfall_amount: i128,
    ) {
        // Require the top-level caller's authentication
        caller.require_auth();

        // 1. Cross-contract call into Grants
        let grants_client = RefractGrantsClient::new(&env, &grants);
        let grant_id = grants_client.create_grant(&caller, &recipient, &total_amount, &milestones);

        // 2. Cross-contract call into Backstop through caller
        let backstop_client = RefractBackstopClient::new(&env, &backstop);
        let _ = backstop_client.cover_shortfall(&caller, &shortfall_amount);

        // Confirm the grant was created
        assert_eq!(grant_id, 0);
    }

    /// Attempts to borrow the caller's authority to withdraw from RefractTreasury
    /// without explicit sub-invocation authorization.
    pub fn unauthorized_borrow_treasury(
        env: Env,
        caller: Address,
        treasury: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) {
        caller.require_auth();

        let treasury_client = RefractTreasuryClient::new(&env, &treasury);
        treasury_client.withdraw(&caller, &token, &amount, &to);
    }

    /// Attempts to call Backstop using the contract's OWN address as the caller,
    /// proving contracts cannot arbitrarily claim pool/admin privileges.
    pub fn unprivileged_contract_draw(
        env: Env,
        backstop: Address,
        amount: i128,
    ) -> i128 {
        let backstop_client = RefractBackstopClient::new(&env, &backstop);
        backstop_client.cover_shortfall(&env.current_contract_address(), &amount)
    }
}

/// Test contract to evaluate reentrancy and caller context escalation.
#[contract]
pub struct ReentrantContextContract;

#[contractimpl]
impl ReentrantContextContract {
    /// Attempts reentrant call back into treasury while executing.
    pub fn reentrant_hook(
        env: Env,
        caller: Address,
        treasury: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) {
        caller.require_auth();
        let treasury_client = RefractTreasuryClient::new(&env, &treasury);
        // Tries to execute withdraw within callback context
        treasury_client.withdraw(&caller, &token, &amount, &to);
    }
}

/// Contract demonstrating guardian pause asymmetry:
/// Guardian can pause, but cannot unpause or access admin functions.
#[contract]
pub struct GuardianPauseContract;

#[contractimpl]
impl GuardianPauseContract {
    pub fn initialize(env: Env, admin: Address, guardian: Address) {
        env.storage().instance().set(&Symbol::new(&env, "admin"), &admin);
        env.storage().instance().set(&Symbol::new(&env, "guardian"), &guardian);
        env.storage().instance().set(&Symbol::new(&env, "paused"), &false);
    }

    /// Fast-path pause: Callable by EITHER admin OR guardian
    pub fn pause(env: Env, caller: Address) {
        caller.require_auth();
        let admin: Address = env.storage().instance().get(&Symbol::new(&env, "admin")).unwrap();
        let guardian: Address = env.storage().instance().get(&Symbol::new(&env, "guardian")).unwrap();

        if caller != admin && caller != guardian {
            panic!("unauthorized");
        }
        env.storage().instance().set(&Symbol::new(&env, "paused"), &true);
    }

    /// Unpause: Callable strictly by admin ONLY
    pub fn unpause(env: Env, caller: Address) {
        caller.require_auth();
        let admin: Address = env.storage().instance().get(&Symbol::new(&env, "admin")).unwrap();

        if caller != admin {
            panic!("unauthorized_admin_only");
        }
        env.storage().instance().set(&Symbol::new(&env, "paused"), &false);
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage().instance().get(&Symbol::new(&env, "paused")).unwrap_or(false)
    }
}

/// Shared test harness bundle providing deployed instances of all 4 contracts
/// and common token and participant accounts.
pub struct SharedAuthHarness<'a> {
    pub env: &'a Env,
    pub admin: Address,
    pub governance: Address,
    pub pool: Address,
    pub user: Address,
    pub stranger: Address,
    pub token_admin: Address,
    pub token_id: Address,
    pub token_client: token::Client<'a>,
    pub sac: token::StellarAssetClient<'a>,

    pub treasury_id: Address,
    pub treasury: RefractTreasuryClient<'a>,

    pub bounty_id: Address,
    pub bounty: RefractBountyEscrowClient<'a>,

    pub backstop_id: Address,
    pub backstop: RefractBackstopClient<'a>,

    pub grants_id: Address,
    pub grants: RefractGrantsClient<'a>,

    pub cross_treasury_id: Address,
    pub cross_treasury: CrossContractTreasuryClient<'a>,
}

impl<'a> SharedAuthHarness<'a> {
    pub fn new(env: &'a Env) -> Self {
        let admin = Address::generate(env);
        let governance = Address::generate(env);
        let pool = Address::generate(env);
        let user = Address::generate(env);
        let stranger = Address::generate(env);
        let token_admin = Address::generate(env);

        let token_sac = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token_id = token_sac.address();
        let token_client = token::Client::new(env, &token_id);
        let sac = token::StellarAssetClient::new(env, &token_id);

        // Deploy RefractTreasury
        let treasury_id = env.register_contract(None, RefractTreasury);
        let treasury = RefractTreasuryClient::new(env, &treasury_id);
        treasury.initialize(&admin);

        // Deploy RefractBountyEscrow
        let bounty_id = env.register_contract(None, RefractBountyEscrow);
        let bounty = RefractBountyEscrowClient::new(env, &bounty_id);
        bounty.initialize(&governance, &token_id);

        // Deploy RefractBackstop
        let backstop_id = env.register_contract(None, RefractBackstop);
        let backstop = RefractBackstopClient::new(env, &backstop_id);
        backstop.initialize(&pool, &admin, &token_id, &2_000);

        // Deploy RefractGrants
        let grants_id = env.register_contract(None, RefractGrants);
        let grants = RefractGrantsClient::new(env, &grants_id);
        grants.initialize(&governance);

        // Deploy CrossContractTreasury test helper
        let cross_treasury_id = env.register_contract(None, CrossContractTreasury);
        let cross_treasury = CrossContractTreasuryClient::new(env, &cross_treasury_id);

        // Fund contracts with initial liquidity using mock_auths
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &token_admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &token_id,
                fn_name: "mint",
                args: (&treasury_id, 100_000i128).into_val(env),
                sub_invokes: &[],
            },
        }]);
        sac.mint(&treasury_id, &100_000);

        // Mint to user and deposit to backstop so total_capital is 100_000
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &token_admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &token_id,
                fn_name: "mint",
                args: (&user, 100_000i128).into_val(env),
                sub_invokes: &[],
            },
        }]);
        sac.mint(&user, &100_000);

        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &user,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &backstop_id,
                fn_name: "deposit",
                args: (&user, 100_000i128).into_val(env),
                sub_invokes: &[soroban_sdk::testutils::MockAuthInvoke {
                    contract: &token_id,
                    fn_name: "transfer",
                    args: (&user, &backstop_id, 100_000i128).into_val(env),
                    sub_invokes: &[],
                }],
            },
        }]);
        backstop.deposit(&user, &100_000);

        Self {
            env,
            admin,
            governance,
            pool,
            user,
            stranger,
            token_admin,
            token_id,
            token_client,
            sac,
            treasury_id,
            treasury,
            bounty_id,
            bounty,
            backstop_id,
            backstop,
            grants_id,
            grants,
            cross_treasury_id,
            cross_treasury,
        }
    }
}
