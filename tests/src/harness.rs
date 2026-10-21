use refract_backstop::{RefractBackstop, RefractBackstopClient};
use refract_bounty::{RefractBountyEscrow, RefractBountyEscrowClient};
use refract_treasury::{RefractTreasury, RefractTreasuryClient};
use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, AuthorizedFunction, AuthorizedInvocation},
    token, Address, Env,
};

/// Shared test harness providing pre-configured protocol contracts, token, and auth assertion utilities.
pub struct SharedAuthHarness<'a> {
    pub env: Env,
    pub admin: Address,
    pub pool: Address,
    pub governance: Address,
    pub token_id: Address,
    pub sac: token::StellarAssetClient<'a>,
    pub token_client: token::Client<'a>,
    pub backstop_id: Address,
    pub backstop: RefractBackstopClient<'a>,
    pub treasury_id: Address,
    pub treasury: RefractTreasuryClient<'a>,
    pub bounty_id: Address,
    pub bounty: RefractBountyEscrowClient<'a>,
}

impl<'a> Default for SharedAuthHarness<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> SharedAuthHarness<'a> {
    pub fn new() -> Self {
        let env = Env::default();

        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let governance = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);

        let backstop_id = env.register_contract(None, RefractBackstop);
        let backstop = RefractBackstopClient::new(&env, &backstop_id);
        backstop.initialize(&pool, &admin, &token_id, &2_000);

        let treasury_id = env.register_contract(None, RefractTreasury);
        let treasury = RefractTreasuryClient::new(&env, &treasury_id);
        treasury.initialize(&admin);

        let bounty_id = env.register_contract(None, RefractBountyEscrow);
        let bounty = RefractBountyEscrowClient::new(&env, &bounty_id);
        bounty.initialize(&governance, &token_id);

        SharedAuthHarness {
            env,
            admin,
            pool,
            governance,
            token_id,
            sac,
            token_client,
            backstop_id,
            backstop,
            treasury_id,
            treasury,
            bounty_id,
            bounty,
        }
    }

    /// Mint tokens to `to` using the SAC client with mocked SAC auth.
    pub fn mint_token(&self, to: &Address, amount: &i128) {
        self.sac.mock_all_auths().mint(to, amount);
    }
}

// ── Mock Intermediate Contracts for Sub-Invocation Chains ────────────────────

/// Contract representing a Treasury/Router that forwards calls into an intermediate or leaf contract.
#[contract]
pub struct MockTreasuryRouter;

#[contractimpl]
impl MockTreasuryRouter {
    /// Calls backstop.cover_shortfall on behalf of caller.
    pub fn route_shortfall(env: Env, caller: Address, backstop: Address, amount: i128) -> i128 {
        caller.require_auth();
        let client = RefractBackstopClient::new(&env, &backstop);
        client.cover_shortfall(&caller, &amount)
    }

    /// Three-contract chain: TreasuryRouter -> GrantsDistributor -> Backstop.cover_shortfall.
    pub fn chain_to_grants_and_backstop(
        env: Env,
        caller: Address,
        grants: Address,
        backstop: Address,
        amount: i128,
    ) -> i128 {
        caller.require_auth();
        let grants_client = MockGrantsDistributorClient::new(&env, &grants);
        grants_client.disburse_to_backstop(&caller, &backstop, &amount)
    }
}

/// Contract representing an intermediate Grants distributor/forwarder.
#[contract]
pub struct MockGrantsDistributor;

#[contractimpl]
impl MockGrantsDistributor {
    /// Forwards call to backstop.cover_shortfall.
    pub fn disburse_to_backstop(
        env: Env,
        caller: Address,
        backstop: Address,
        amount: i128,
    ) -> i128 {
        caller.require_auth();
        let client = RefractBackstopClient::new(&env, &backstop);
        client.cover_shortfall(&caller, &amount)
    }

    /// Calls treasury.withdraw on behalf of caller.
    pub fn withdraw_from_treasury(
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
}

// ── Auth-Tree Verification Utilities ─────────────────────────────────────────

/// Helper to assert that `auths` contains a single root authorization for `expected_address`.
pub fn get_single_root_auth<'a>(
    auths: &'a [(Address, AuthorizedInvocation)],
    expected_address: &Address,
) -> &'a AuthorizedInvocation {
    assert_eq!(
        auths.len(),
        1,
        "Expected exactly 1 authorized address in auth tree, found {}",
        auths.len()
    );
    assert_eq!(&auths[0].0, expected_address, "Authorized address mismatch");
    &auths[0].1
}

/// Helper to verify a contract function invocation node in the auth tree.
pub fn assert_contract_function_invocation(
    invocation: &AuthorizedInvocation,
    expected_contract: &Address,
    expected_fn: &str,
) {
    match &invocation.function {
        AuthorizedFunction::Contract((contract, fn_symbol, _args)) => {
            assert_eq!(
                contract, expected_contract,
                "Contract address mismatch in auth invocation"
            );
            let fn_name_str = fn_symbol.to_string();
            assert_eq!(
                fn_name_str, expected_fn,
                "Function name mismatch in auth invocation: expected {}, found {}",
                expected_fn, fn_name_str
            );
        }
        _ => panic!("Expected AuthorizedFunction::Contract, found other variant"),
    }
}
