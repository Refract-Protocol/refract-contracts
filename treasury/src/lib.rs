#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

/// Tagged record for slashed grantee performance bonds credited to the treasury.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashedBondCredit {
    pub grant_id: u64,
    pub token: Address,
    pub amount: i128,
    pub reason: Symbol,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    GrantsContract,
    /// Unsettled bond custody held in `RefractGrants` (tracked as a memo only;
    /// never added to treasury NAV so bonds are never counted twice).
    BondCustodyMemo(Address),
    /// Cumulative slashed bond funds received into treasury for `token`.
    SlashedBondTotal(Address),
    /// Per-grant slashed bond credit audit record.
    SlashedBondRecord(u64),
}

/// `RefractTreasury` is a token-custody vault that also receives and tags
/// slashed grantee performance bonds without double-counting active bonds in NAV.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Configure the authorized `RefractGrants` contract address.
    pub fn set_grants_contract(env: Env, caller: Address, grants: Address) {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            panic!("unauthorized");
        }
        env.storage()
            .instance()
            .set(&DataKey::GrantsContract, &grants);
    }

    /// Record an informational memo of active grantee bonds held in `RefractGrants`.
    /// Active bonds are held in `RefractGrants` custody and MUST NOT be counted
    /// toward treasury NAV until slashed and transferred.
    pub fn record_bond_custody_memo(env: Env, caller: Address, token: Address, amount: i128) {
        caller.require_auth();
        Self::require_admin_or_grants(&env, &caller);
        if amount < 0 {
            panic!("negative memo amount");
        }
        env.storage()
            .instance()
            .set(&DataKey::BondCustodyMemo(token), &amount);
    }

    /// Receive and tag slashed performance bond funds from `RefractGrants` or admin.
    pub fn receive_slashed_bond(
        env: Env,
        caller: Address,
        grant_id: u64,
        token: Address,
        amount: i128,
        reason: Symbol,
    ) {
        caller.require_auth();
        Self::require_admin_or_grants(&env, &caller);
        if amount <= 0 {
            panic!("amount must be positive");
        }

        let client = token::Client::new(&env, &token);
        client.transfer(&caller, &env.current_contract_address(), &amount);

        let prev_total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashedBondTotal(token.clone()))
            .unwrap_or(0);
        env.storage().instance().set(
            &DataKey::SlashedBondTotal(token.clone()),
            &(prev_total + amount),
        );

        let record = SlashedBondCredit {
            grant_id,
            token: token.clone(),
            amount,
            reason: reason.clone(),
        };
        env.storage()
            .instance()
            .set(&DataKey::SlashedBondRecord(grant_id), &record);

        env.events().publish(
            (symbol_short!("bond_slsh"), grant_id),
            (token, amount, reason),
        );
    }

    /// Return the cumulative slashed bond amount credited for `token`.
    pub fn slashed_bond_total(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SlashedBondTotal(token))
            .unwrap_or(0)
    }

    /// Return the tagged slashed-bond record for `grant_id`, if any.
    pub fn slashed_bond_record(env: Env, grant_id: u64) -> Option<SlashedBondCredit> {
        env.storage()
            .instance()
            .get(&DataKey::SlashedBondRecord(grant_id))
    }

    /// Return the treasury's Net Asset Value (NAV) for `token`, defined strictly
    /// as the settled token balance held by `RefractTreasury` (excluding any
    /// unslashed bond custody memo in `RefractGrants` so bonds are never counted twice).
    pub fn nav_balance(env: Env, token: Address) -> i128 {
        let client = token::Client::new(&env, &token);
        client.balance(&env.current_contract_address())
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    fn require_admin_or_grants(env: &Env, caller: &Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        let grants: Option<Address> = env.storage().instance().get(&DataKey::GrantsContract);
        if *caller != admin && grants.as_ref() != Some(caller) {
            panic!("unauthorized");
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }

    #[test]
    fn slashed_bond_routing_and_nav_no_double_counting() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let grants = Address::generate(&env);
        client.initialize(&admin);
        client.set_grants_contract(&admin, &grants);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &5_000);
        sac.mint(&grants, &1_000);

        // Recording bond custody memo in Grants does not inflate Treasury NAV.
        client.record_bond_custody_memo(&grants, &token_id, &1_000);
        assert_eq!(client.nav_balance(&token_id), 5_000);

        // Slashing 600 transfers funds into Treasury and tags the source.
        client.receive_slashed_bond(&grants, &7, &token_id, &600, &symbol_short!("clawback"));
        assert_eq!(client.nav_balance(&token_id), 5_600);
        assert_eq!(client.slashed_bond_total(&token_id), 600);
        assert_eq!(
            client.slashed_bond_record(&7),
            Some(SlashedBondCredit {
                grant_id: 7,
                token: token_id,
                amount: 600,
                reason: symbol_short!("clawback"),
            })
        );
    }
}
