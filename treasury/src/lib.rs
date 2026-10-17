#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Initialized,
    Admin,
    TotalWithdrawn(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    EntryArchived = 5,
    InconsistentRestoredState = 6,
}

/// `RefractTreasury` is a deliberately minimal token-custody vault.
///
/// Funds are deposited via plain token transfers to the contract address
/// (no `deposit` entrypoint is required). Withdrawals are admin-gated.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Return the configured admin, distinguishing `EntryArchived` from `NotInitialized`.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_live_admin(&env)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_live_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);

        let key = DataKey::TotalWithdrawn(token);
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + amount));
        Ok(())
    }

    /// Restore an archived cumulative withdrawal counter for `token`.
    pub fn restore_withdrawn_counter(
        env: Env,
        caller: Address,
        token: Address,
        total_withdrawn: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_live_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if total_withdrawn < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::TotalWithdrawn(token), &total_withdrawn);
        Ok(())
    }

    /// Post-restore consistency check validating live admin configuration,
    /// cumulative withdrawn counter, and actual token balance against expected inflows.
    pub fn verify_post_restore(
        env: Env,
        token: Address,
        expected_total_inflows: i128,
    ) -> Result<i128, TreasuryError> {
        let _ = Self::read_live_admin(&env)?;
        if expected_total_inflows < 0 {
            return Err(TreasuryError::InconsistentRestoredState);
        }
        let withdrawn: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalWithdrawn(token.clone()))
            .ok_or(TreasuryError::EntryArchived)?;
        if withdrawn < 0 || withdrawn > expected_total_inflows {
            return Err(TreasuryError::InconsistentRestoredState);
        }

        let actual_balance =
            token::Client::new(&env, &token).balance(&env.current_contract_address());
        if actual_balance + withdrawn != expected_total_inflows {
            return Err(TreasuryError::InconsistentRestoredState);
        }
        Ok(actual_balance)
    }

    fn read_live_admin(env: &Env) -> Result<Address, TreasuryError> {
        if let Some(admin) = env.storage().instance().get(&DataKey::Admin) {
            return Ok(admin);
        }
        let init: bool = env
            .storage()
            .instance()
            .get(&DataKey::Initialized)
            .unwrap_or(false);
        if init {
            Err(TreasuryError::EntryArchived)
        } else {
            Err(TreasuryError::NotInitialized)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated_and_restore_consistency_verified() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin);
        let token_id = token_contract.address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);

        // Simulate persistent withdrawal counter archival
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .remove(&DataKey::TotalWithdrawn(token_id.clone()));
        });
        assert_eq!(
            client.try_verify_post_restore(&token_id, &1_000),
            Err(Ok(TreasuryError::EntryArchived))
        );

        // Restore with wrong counter fails consistency check
        client.restore_withdrawn_counter(&admin, &token_id, &200);
        assert_eq!(
            client.try_verify_post_restore(&token_id, &1_000),
            Err(Ok(TreasuryError::InconsistentRestoredState))
        );

        // Restore exact counter passes consistency check
        client.restore_withdrawn_counter(&admin, &token_id, &400);
        assert_eq!(client.verify_post_restore(&token_id, &1_000), 600);
    }
}
