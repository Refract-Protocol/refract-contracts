//! Shared fixture for the end-to-end suites.
//!
//! [`Protocol::new`] registers every contract in one `Env` and wires them the
//! way a production deployment would:
//!
//! * `RefractGovernor` owns the `RefractTimelock` (it is the timelock admin).
//! * The timelock is the admin/governance address of `RefractTreasury`,
//!   `RefractGrants`, `RefractBountyEscrow`, `RefractBackstop` and (after
//!   bootstrap) `RefractPool`.
//! * `RefractPool` routes `PREMIUM_SHARE_BPS` of every premium to
//!   `RefractBackstop`.
//! * USDC and the governance token are instances of [`MockToken`], a minimal
//!   real token contract (not a mocked client).
//!
//! Contract ids are never hardcoded: every address comes from
//! `register_contract`, so the suites do not depend on registration order.
//! Time only moves through [`Protocol::advance`].
#![allow(dead_code)]

use refract_backstop::{RefractBackstop, RefractBackstopClient};
use refract_bounty::{RefractBountyEscrow, RefractBountyEscrowClient};
use refract_governance::{GovernorConfig, ProposalStatus, RefractGovernor, RefractGovernorClient};
use refract_grants::{RefractGrants, RefractGrantsClient};
use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use refract_pool::{CoverageType, PolicyParams, RefractPool, RefractPoolClient};
use refract_timelock::{RefractTimelock, RefractTimelockClient};
use refract_treasury::{RefractTreasury, RefractTreasuryClient};
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    Address, Env, Symbol, TryFromVal, Val, Vec,
};

pub mod mock_token {
    use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

    #[contracttype]
    enum Key {
        Balance(Address),
        Supply,
    }

    /// Minimal token: SEP-41 `balance`/`transfer`, plus `mint` (test only)
    /// and `total_supply` (needed by the governor's quorum).
    #[contract]
    pub struct MockToken;

    #[contractimpl]
    impl MockToken {
        pub fn mint(env: Env, to: Address, amount: i128) {
            assert!(amount > 0);
            let bal = Self::balance(env.clone(), to.clone());
            env.storage()
                .persistent()
                .set(&Key::Balance(to), &(bal + amount));
            let supply = Self::total_supply(env.clone());
            env.storage()
                .instance()
                .set(&Key::Supply, &(supply + amount));
        }

        pub fn balance(env: Env, id: Address) -> i128 {
            env.storage()
                .persistent()
                .get(&Key::Balance(id))
                .unwrap_or(0)
        }

        pub fn total_supply(env: Env) -> i128 {
            env.storage().instance().get(&Key::Supply).unwrap_or(0)
        }

        pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
            from.require_auth();
            assert!(amount >= 0, "negative amount");
            let from_bal = Self::balance(env.clone(), from.clone());
            assert!(from_bal >= amount, "insufficient balance");
            env.storage()
                .persistent()
                .set(&Key::Balance(from), &(from_bal - amount));
            let to_bal = Self::balance(env.clone(), to.clone());
            env.storage()
                .persistent()
                .set(&Key::Balance(to), &(to_bal + amount));
        }
    }
}

pub use mock_token::MockTokenClient;

/// Timelock delay used by every scenario.
pub const DELAY: u64 = 2 * 86_400;
/// Governance voting period.
pub const VOTING_PERIOD: u64 = 3 * 86_400;
pub const DAY: u64 = 86_400;
/// USDC uses 1e7 fixed-point, like the pool.
pub const USDC: i128 = 10_000_000;
pub const TREASURY_EPOCH_LIMIT: i128 = 50_000 * USDC;
pub const TREASURY_LARGE_THRESHOLD: i128 = 1_000 * USDC;
pub const PREMIUM_SHARE_BPS: u32 = 2_000;
pub const BACKSTOP_BPS: i128 = 2_000;
pub const POOL_CAPITAL: i128 = 100_000 * USDC;

pub struct Protocol {
    pub env: Env,
    /// Bootstrap deployer key: configures contracts, then hands control to
    /// the timelock. Also the governor's guardian (may cancel proposals).
    pub deployer: Address,
    pub manager: Address,
    /// Holds a voting majority.
    pub alice: Address,
    pub bob: Address,
    pub stranger: Address,

    pub usdc: MockTokenClient<'static>,
    pub gov_token: MockTokenClient<'static>,
    pub governor: RefractGovernorClient<'static>,
    pub timelock: RefractTimelockClient<'static>,
    pub treasury: RefractTreasuryClient<'static>,
    pub grants: RefractGrantsClient<'static>,
    pub bounty: RefractBountyEscrowClient<'static>,
    pub backstop: RefractBackstopClient<'static>,
    pub pool: RefractPoolClient<'static>,
    pub registry: RefractPolicyRegistryClient<'static>,
}

impl Protocol {
    pub fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| {
            l.timestamp = 1_700_000_000;
            l.sequence_number = 100;
        });

        let deployer = Address::generate(&env);
        let manager = Address::generate(&env);
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let stranger = Address::generate(&env);

        let usdc = MockTokenClient::new(&env, &env.register_contract(None, mock_token::MockToken));
        let gov_token =
            MockTokenClient::new(&env, &env.register_contract(None, mock_token::MockToken));
        gov_token.mint(&alice, &700_000);
        gov_token.mint(&bob, &300_000);

        // Governance + timelock.
        let governor =
            RefractGovernorClient::new(&env, &env.register_contract(None, RefractGovernor));
        let timelock =
            RefractTimelockClient::new(&env, &env.register_contract(None, RefractTimelock));
        governor.initialize(
            &deployer,
            &GovernorConfig {
                token: gov_token.address.clone(),
                voting_period: VOTING_PERIOD,
                quorum_bps: 400,
                proposal_threshold: 10_000,
            },
        );
        timelock.initialize(&governor.address, &DELAY);
        governor.set_timelock(&deployer, &timelock.address);
        let tl = timelock.address.clone();

        // Treasury, grants, bounty — all governed by the timelock.
        let treasury =
            RefractTreasuryClient::new(&env, &env.register_contract(None, RefractTreasury));
        treasury.initialize(
            &tl,
            &manager,
            &usdc.address,
            &DAY,
            &TREASURY_EPOCH_LIMIT,
            &TREASURY_LARGE_THRESHOLD,
        );
        let grants = RefractGrantsClient::new(&env, &env.register_contract(None, RefractGrants));
        grants.initialize(&tl, &usdc.address);
        let bounty =
            RefractBountyEscrowClient::new(&env, &env.register_contract(None, RefractBountyEscrow));
        bounty.initialize(&tl, &usdc.address);

        // Pool, registry and backstop.
        let registry = RefractPolicyRegistryClient::new(
            &env,
            &env.register_contract(None, RefractPolicyRegistry),
        );
        let pool = RefractPoolClient::new(&env, &env.register_contract(None, RefractPool));
        let backstop =
            RefractBackstopClient::new(&env, &env.register_contract(None, RefractBackstop));
        registry.initialize(&deployer, &pool.address);
        pool.initialize(&deployer, &usdc.address, &registry.address);
        backstop.initialize(&pool.address, &tl, &usdc.address, &BACKSTOP_BPS);
        pool.set_backstop(&deployer, &backstop.address);
        pool.set_premium_share_bps(&deployer, &PREMIUM_SHARE_BPS);
        // Bootstrap done: hand the pool's admin role to the timelock.
        pool.set_admin(&deployer, &tl);

        Protocol {
            env,
            deployer,
            manager,
            alice,
            bob,
            stranger,
            usdc,
            gov_token,
            governor,
            timelock,
            treasury,
            grants,
            bounty,
            backstop,
            pool,
            registry,
        }
    }

    pub fn now(&self) -> u64 {
        self.env.ledger().timestamp()
    }

    /// Move the ledger clock forward. Only the timestamp moves: the ledger
    /// sequence stays put so storage TTLs (a separate concern, #205) don't
    /// expire mid-scenario.
    pub fn advance(&self, secs: u64) {
        self.env.ledger().with_mut(|l| l.timestamp += secs);
    }

    pub fn fund_treasury(&self, amount: i128) {
        let funder = Address::generate(&self.env);
        self.usdc.mint(&funder, &amount);
        self.treasury.deposit(&funder, &amount);
    }

    /// Propose `function(args)` on `target`, vote it through with alice and
    /// finalize. Returns the proposal id.
    pub fn pass(&self, target: &Address, function: &str, args: Vec<Val>) -> u64 {
        let id = self.governor.propose(
            &self.alice,
            target,
            &Symbol::new(&self.env, function),
            &args,
            &Symbol::new(&self.env, "e2e"),
        );
        self.governor.cast_vote(&self.alice, &id, &true);
        self.advance(VOTING_PERIOD + 1);
        assert_eq!(self.governor.finalize(&id), ProposalStatus::Succeeded);
        id
    }

    /// `pass` + queue into the timelock. Returns `(proposal_id, op_id)`.
    pub fn pass_and_queue(&self, target: &Address, function: &str, args: Vec<Val>) -> (u64, u64) {
        let id = self.pass(target, function, args);
        let op = self
            .governor
            .queue(&id)
            .expect("timelock is wired, so queue returns an op id");
        (id, op)
    }

    /// Full happy path: pass, queue, wait out the delay, execute.
    pub fn govern(&self, target: &Address, function: &str, args: Vec<Val>) -> u64 {
        let (id, _op) = self.pass_and_queue(target, function, args);
        self.advance(DELAY);
        self.governor.execute(&id);
        id
    }

    /// Run `f` with every signature check enforced (no mocked auth), then
    /// restore mocking for the rest of the scenario.
    pub fn without_mocked_auth<R>(&self, f: impl FnOnce() -> R) -> R {
        self.env.set_auths(&[]);
        let r = f();
        self.env.mock_all_auths();
        r
    }

    pub fn policy_params(&self) -> PolicyParams {
        PolicyParams {
            coverage_amount: 1_000 * USDC,
            coverage_type: CoverageType::StablecoinDepeg,
            duration_days: 30,
            trigger_threshold: 500,
        }
    }

    pub fn seed_pool_capital(&self) {
        let lp = Address::generate(&self.env);
        self.usdc.mint(&lp, &POOL_CAPITAL);
        self.pool.provide_capital(&lp, &POOL_CAPITAL);
    }

    /// True if `contract` emitted an event in the last invocation whose first
    /// topic is the symbol `name`.
    pub fn emitted(&self, contract: &Address, name: &str) -> bool {
        let want = Symbol::new(&self.env, name);
        self.env.events().all().iter().any(|(c, topics, _)| {
            &c == contract
                && topics
                    .get(0)
                    .and_then(|t| Symbol::try_from_val(&self.env, &t).ok())
                    .map_or(false, |s| s == want)
        })
    }
}

pub fn args(env: &Env, vals: &[Val]) -> Vec<Val> {
    Vec::from_slice(env, vals)
}
