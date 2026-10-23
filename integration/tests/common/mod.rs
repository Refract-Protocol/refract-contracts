use refract_bounty::{RefractBountyEscrow, RefractBountyEscrowClient};
use soroban_sdk::{testutils::Address as _, token, Address, Env};

/// A bounty escrow wired to a governance address and a stub token, with a
/// funded pool. The stub token is the Stellar asset contract, not a mock client.
pub struct Fixture {
    pub env: Env,
    pub bounty: RefractBountyEscrowClient<'static>,
    pub governance: Address,
    pub token: Address,
    pub funder: Address,
}

pub fn fixture(pool_funding: i128) -> Fixture {
    let env = Env::default();
    env.mock_all_auths();

    let bounty_id = env.register_contract(None, RefractBountyEscrow);
    let bounty = RefractBountyEscrowClient::new(&env, &bounty_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract(token_admin);
    bounty.initialize(&governance, &token);

    let funder = Address::generate(&env);
    token::StellarAssetClient::new(&env, &token).mint(&funder, &pool_funding);
    bounty.fund(&funder, &pool_funding);

    Fixture {
        env,
        bounty,
        governance,
        token,
        funder,
    }
}
