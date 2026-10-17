//! Stellar-asset-backed mock token helper with `fund`, `drain`, and `balance` utilities.

use soroban_sdk::{testutils::Address as _, token, Address, Env};

/// Wrapper around a Stellar Asset Contract instance for deterministic test funding and draining.
#[derive(Clone)]
pub struct MockTokenHelper {
    /// Address of the registered Stellar Asset Contract.
    pub address: Address,
    /// Admin address of the Stellar Asset Contract.
    pub admin: Address,
}

impl MockTokenHelper {
    /// Deploy a fresh Stellar Asset Contract on `env` and return its [`MockTokenHelper`].
    pub fn deploy(env: &Env) -> Self {
        let admin = Address::generate(env);
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        Self {
            address: sac.address(),
            admin,
        }
    }

    /// Mint `amount` (`1e7` units) directly to `recipient`.
    pub fn fund(&self, env: &Env, recipient: &Address, amount: i128) {
        token::StellarAssetClient::new(env, &self.address).mint(recipient, &amount);
    }

    /// Drain `amount` (`1e7` units) from `holder` by transferring to a burn sink address.
    pub fn drain(&self, env: &Env, holder: &Address, amount: i128) {
        let sink = Address::generate(env);
        token::Client::new(env, &self.address).transfer(holder, &sink, &amount);
    }

    /// Return the current token balance of `account`.
    pub fn balance(&self, env: &Env, account: &Address) -> i128 {
        token::Client::new(env, &self.address).balance(account)
    }
}
