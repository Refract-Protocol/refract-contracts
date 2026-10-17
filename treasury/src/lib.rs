#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Paused,
    Allowance(Address),
}

/// Read-only simulation result returned by [`RefractTreasury::preview_treasury_spend`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasurySpendPreview {
    /// Asset contract address queried.
    pub asset: Address,
    /// Amount requested to spend.
    pub requested_amount: i128,
    /// Current vault token balance for `asset`.
    pub available_balance: i128,
    /// Configured per-asset spend allowance (or `available_balance` if no cap is configured).
    pub current_allowance: i128,
    /// Net amount that `withdraw` can disburse right now (0 if insufficient balance or allowance).
    pub disbursable_amount: i128,
    /// Vault token balance that would remain after executing the withdrawal.
    pub remaining_balance: i128,
    /// Remaining asset allowance after executing the withdrawal.
    pub remaining_allowance: i128,
    /// Whether both vault balance and allowance cover `requested_amount`.
    pub can_spend: bool,
    /// Whether the treasury is currently paused (previews still return figures when paused).
    pub paused: bool,
}

/// `RefractTreasury` is a token-custody vault with read-only spend simulation.
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
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Admin-gated pause toggle. Previews still return figures while flagging `paused = true`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            panic!("unauthorized");
        }
        env.storage().instance().set(&DataKey::Paused, &paused);
    }

    /// Configure an optional per-asset spend allowance.
    pub fn set_asset_allowance(env: Env, caller: Address, token: Address, allowance: i128) {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            panic!("unauthorized");
        }
        env.storage()
            .persistent()
            .set(&DataKey::Allowance(token), &allowance);
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    /// Shares [`Self::compute_treasury_spend`] with [`Self::preview_treasury_spend`].
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            panic!("unauthorized");
        }
        if Self::is_paused(&env) {
            panic!("paused");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let preview = Self::compute_treasury_spend(&env, &token, amount);
        if !preview.can_spend {
            panic!("exceeds balance or allowance");
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Allowance(token.clone()))
        {
            env.storage().persistent().set(
                &DataKey::Allowance(token.clone()),
                &preview.remaining_allowance,
            );
        }
        let client = token::Client::new(&env, &token);
        client.transfer(
            &env.current_contract_address(),
            &to,
            &preview.disbursable_amount,
        );
    }

    /// Pure, side-effect-free simulation of spending `amount` of `token` from the treasury.
    ///
    /// Performs no storage writes, no `require_auth`, and no token transfers.
    pub fn preview_treasury_spend(env: Env, token: Address, amount: i128) -> TreasurySpendPreview {
        Self::compute_treasury_spend(&env, &token, amount)
    }

    /// Shared internal calculation called by both [`Self::withdraw`] and [`Self::preview_treasury_spend`].
    fn compute_treasury_spend(env: &Env, token: &Address, amount: i128) -> TreasurySpendPreview {
        let available_balance =
            token::Client::new(env, token).balance(&env.current_contract_address());
        let configured_allowance: Option<i128> = env
            .storage()
            .persistent()
            .get(&DataKey::Allowance(token.clone()));
        let current_allowance = configured_allowance.unwrap_or(available_balance);
        let paused = Self::is_paused(env);

        if amount <= 0 {
            return TreasurySpendPreview {
                asset: token.clone(),
                requested_amount: amount,
                available_balance,
                current_allowance,
                disbursable_amount: 0,
                remaining_balance: available_balance,
                remaining_allowance: current_allowance,
                can_spend: false,
                paused,
            };
        }

        let can_spend = amount <= available_balance && amount <= current_allowance;
        let disbursable_amount = if can_spend { amount } else { 0 };
        let remaining_balance = available_balance - disbursable_amount;
        let remaining_allowance = current_allowance - disbursable_amount;

        TreasurySpendPreview {
            asset: token.clone(),
            requested_amount: amount,
            available_balance,
            current_allowance,
            disbursable_amount,
            remaining_balance,
            remaining_allowance,
            can_spend,
            paused,
        }
    }

    fn is_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn preview_treasury_spend_matches_withdraw_and_allowance_exhaustion() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        client.set_asset_allowance(&admin, &token_id, &600);

        let p1 = client.preview_treasury_spend(&token_id, &400);
        assert!(p1.can_spend);
        assert_eq!(p1.disbursable_amount, 400);
        assert_eq!(p1.remaining_balance, 600);
        assert_eq!(p1.remaining_allowance, 200);
        assert!(!p1.paused);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), p1.disbursable_amount);
        assert_eq!(token_client.balance(&contract_id), p1.remaining_balance);

        // Next spend of 300 exceeds the remaining allowance of 200.
        let p2 = client.preview_treasury_spend(&token_id, &300);
        assert!(!p2.can_spend);
        assert_eq!(p2.disbursable_amount, 0);
        assert_eq!(p2.current_allowance, 200);
        assert_eq!(p2.remaining_allowance, 200);

        // Paused flag is reported while still returning figures.
        client.set_paused(&admin, &true);
        let p3 = client.preview_treasury_spend(&token_id, &150);
        assert!(p3.paused);
        assert!(p3.can_spend);
        assert_eq!(p3.disbursable_amount, 150);
    }
}
