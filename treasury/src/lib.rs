#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    OperatingBalance(Address), // token -> amount
    ReserveBalance(Address),   // token -> amount
    ReserveTarget(Address),    // token -> target ratio / amount
    LastRelease(Address),      // token -> timestamp
    TimelockContract,          // timelock address authorized to release reserve
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientReserve = 5,
    ReserveLocked = 6,
    UnauthorizedTimelock = 7,
    InsufficientBalance = 8,
}

/// `RefractTreasury` is a token-custody vault with a segregated emergency reserve pool.
///
/// Standard funds reside in operating balance and can be withdrawn by admin.
/// Emergency reserves are strictly segregated and can only be released by the
/// governance timelock contract.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to manage the vault.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    /// Configure the timelock contract authorized to release emergency reserves.
    /// Only callable by admin.
    pub fn set_timelock(env: Env, caller: Address, timelock: Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::TimelockContract, &timelock);
        Ok(())
    }

    /// Return the configured timelock contract.
    pub fn timelock(env: Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::TimelockContract)
            .ok_or(TreasuryError::NotInitialized)
    }

    /// Deposit funds into the segregated emergency reserve pool.
    pub fn deposit_reserve(
        env: Env,
        from: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&from, &env.current_contract_address(), &amount);

        let cur_reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBalance(token.clone()))
            .unwrap_or(0);
        let new_reserve = cur_reserve
            .checked_add(amount)
            .ok_or(TreasuryError::InvalidAmount)?;
        env.storage()
            .instance()
            .set(&DataKey::ReserveBalance(token), &new_reserve);

        Ok(())
    }

    /// Set reserve target amount or ratio for a token. Callable by admin.
    pub fn set_reserve_target(
        env: Env,
        caller: Address,
        token: Address,
        target: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if target < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::ReserveTarget(token), &target);
        Ok(())
    }

    /// Release funds from the emergency reserve. Gated strictly to the timelock contract.
    pub fn release_reserve(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
        purpose_tag: Symbol,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let timelock: Address = env
            .storage()
            .instance()
            .get(&DataKey::TimelockContract)
            .ok_or(TreasuryError::NotInitialized)?;
        if caller != timelock {
            return Err(TreasuryError::UnauthorizedTimelock);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBalance(token.clone()))
            .unwrap_or(0);
        if amount > reserve {
            return Err(TreasuryError::InsufficientReserve);
        }

        let new_reserve = reserve - amount;
        env.storage()
            .instance()
            .set(&DataKey::ReserveBalance(token.clone()), &new_reserve);
        env.storage()
            .instance()
            .set(&DataKey::LastRelease(token.clone()), &env.ledger().timestamp());

        env.events().publish(
            (symbol_short!("rel_res"), purpose_tag),
            (to.clone(), amount),
        );

        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Return reserve information for `token`: (reserve_balance, reserve_target, last_release_timestamp).
    pub fn get_reserve_info(env: Env, token: Address) -> (i128, i128, u64) {
        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBalance(token.clone()))
            .unwrap_or(0);
        let target: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveTarget(token.clone()))
            .unwrap_or(0);
        let last_release: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastRelease(token))
            .unwrap_or(0);
        (reserve, target, last_release)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    /// Operates strictly on non-reserve (operating) balance.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let total_balance = token::Client::new(&env, &token).balance(&env.current_contract_address());
        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBalance(token.clone()))
            .unwrap_or(0);
        let operating = total_balance - reserve;
        if amount > operating {
            return Err(TreasuryError::InsufficientBalance);
        }

        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);

        let new_operating = operating - amount;
        env.storage()
            .instance()
            .set(&DataKey::OperatingBalance(token), &new_operating);

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

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
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
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
    fn rejects_double_initialization() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let second_admin = Address::generate(&env);
        assert_eq!(
            client.try_initialize(&second_admin),
            Err(Ok(TreasuryError::AlreadyInitialized))
        );
    }

    #[test]
    fn rejects_operations_when_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        assert_eq!(client.try_admin(), Err(Ok(TreasuryError::NotInitialized)));

        let caller = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&caller, &token, &100, &to),
            Err(Ok(TreasuryError::NotInitialized))
        );
    }

    #[test]
    fn rejects_unauthorized_withdraw() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let stranger = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&stranger, &token, &100, &to),
            Err(Ok(TreasuryError::Unauthorized))
        );
    }

    #[test]
    fn rejects_invalid_withdraw_amounts() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token = Address::generate(&env);
        let to = Address::generate(&env);

        assert_eq!(
            client.try_withdraw(&admin, &token, &0, &to),
            Err(Ok(TreasuryError::InvalidAmount))
        );
        assert_eq!(
            client.try_withdraw(&admin, &token, &-500, &to),
            Err(Ok(TreasuryError::InvalidAmount))
        );
    }

    #[test]
    fn test_deposit_reserve_tracks_separate_balance() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let depositor = Address::generate(&env);
        sac.mint(&depositor, &500);

        // Deposit reserve
        client.deposit_reserve(&depositor, &token_id, &500);

        // Treasury holds 500 tokens in contract balance
        assert_eq!(token_client.balance(&contract_id), 500);

        // Reserve info shows 500 in reserve
        let (reserve_bal, reserve_target, last_release) = client.get_reserve_info(&token_id);
        assert_eq!(reserve_bal, 500);
        assert_eq!(reserve_target, 0);
        assert_eq!(last_release, 0);

        // Operating balance is 0, so withdraw fails
        let to = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&admin, &token_id, &1, &to),
            Err(Ok(TreasuryError::InsufficientBalance))
        );
    }

    #[test]
    fn test_withdraw_cannot_touch_reserve() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit 1,000 into emergency reserve
        let depositor = Address::generate(&env);
        sac.mint(&depositor, &1_000);
        client.deposit_reserve(&depositor, &token_id, &1_000);

        // Mint 300 to contract as operating funds
        sac.mint(&contract_id, &300);

        assert_eq!(token_client.balance(&contract_id), 1_300);

        let to = Address::generate(&env);

        // Admin can withdraw operating funds (up to 300)
        client.withdraw(&admin, &token_id, &300, &to);
        assert_eq!(token_client.balance(&to), 300);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        // Admin cannot withdraw even 1 token from reserve
        assert_eq!(
            client.try_withdraw(&admin, &token_id, &1, &to),
            Err(Ok(TreasuryError::InsufficientBalance))
        );
    }

    #[test]
    fn test_release_reserve_requires_timelock() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let timelock = Address::generate(&env);
        client.set_timelock(&admin, &timelock);
        assert_eq!(client.timelock(), timelock);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let depositor = Address::generate(&env);
        sac.mint(&depositor, &1_000);
        client.deposit_reserve(&depositor, &token_id, &1_000);

        let stranger = Address::generate(&env);
        let to = Address::generate(&env);

        // Stranger fails
        assert_eq!(
            client.try_release_reserve(
                &stranger,
                &token_id,
                &100,
                &to,
                &symbol_short!("payout")
            ),
            Err(Ok(TreasuryError::UnauthorizedTimelock))
        );

        // Admin also fails because only timelock can release reserve
        assert_eq!(
            client.try_release_reserve(
                &admin,
                &token_id,
                &100,
                &to,
                &symbol_short!("payout")
            ),
            Err(Ok(TreasuryError::UnauthorizedTimelock))
        );
    }

    #[test]
    fn test_release_reserve_success() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let timelock = Address::generate(&env);
        client.set_timelock(&admin, &timelock);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let depositor = Address::generate(&env);
        sac.mint(&depositor, &1_000);
        client.deposit_reserve(&depositor, &token_id, &1_000);

        client.set_reserve_target(&admin, &token_id, &800);

        env.ledger().with_mut(|l| l.timestamp = 1_700_000_000);

        let to = Address::generate(&env);
        client.release_reserve(
            &timelock,
            &token_id,
            &400,
            &to,
            &symbol_short!("disaster"),
        );

        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);

        let (reserve_bal, target, last_rel) = client.get_reserve_info(&token_id);
        assert_eq!(reserve_bal, 600);
        assert_eq!(target, 800);
        assert_eq!(last_rel, 1_700_000_000);

        // Cannot release more than remaining reserve
        assert_eq!(
            client.try_release_reserve(
                &timelock,
                &token_id,
                &700,
                &to,
                &symbol_short!("excess")
            ),
            Err(Ok(TreasuryError::InsufficientReserve))
        );
    }

    #[test]
    fn test_set_timelock_admin_only() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let stranger = Address::generate(&env);
        let timelock = Address::generate(&env);
        assert_eq!(
            client.try_set_timelock(&stranger, &timelock),
            Err(Ok(TreasuryError::Unauthorized))
        );
    }
}
