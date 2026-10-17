#![no_std]

pub mod normalization;
pub use normalization::{
    normalize_inflow, normalize_outflow, to_protocol_units, to_token_units, NormalizationError,
    Rounding, PROTOCOL_DECIMALS, PROTOCOL_SCALE,
};

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
    String as SorobanString, Symbol,
};

/// Status of a queued or executed withdrawal.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum WithdrawalStatus {
    Pending = 0,
    Challenged = 1,
    Approved = 2,
    Executed = 3,
    Cancelled = 4,
}

/// A withdrawal proposal/record tracked in treasury storage.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawalRecord {
    pub id: u64,
    pub token: Address,
    pub amount: i128,
    pub to: Address,
    pub proposed_at: u64,
    pub status: WithdrawalStatus,
    pub challenger: Option<Address>,
    pub challenge_reason: Option<SorobanString>,
}

/// Configuration for high-value withdrawal challenge period.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChallengeConfig {
    pub challenge_threshold_amount: i128,
    pub challenge_window_seconds: u64,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone, Debug)]
pub enum DataKey {
    Admin,
    OperatingBalance(Address), // token -> amount
    ReserveBalance(Address),   // token -> amount
    ReserveTarget(Address),    // token -> target ratio / amount
    LastRelease(Address),      // token -> timestamp
    TimelockContract,          // timelock address authorized to release reserve
    ChallengeThresholdAmount,  // i128
    ChallengeWindowSeconds,    // u64
    NextWithdrawalId,          // u64
    Withdrawal(u64),           // u64 -> WithdrawalRecord
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
    NormalizationError = 9,
    StateArchived = 10,
    InconsistentState = 11,
    InvalidChallengeWindow = 12,
    WithdrawalNotFound = 13,
    InvalidWithdrawalStatus = 14,
    WithdrawalChallenged = 15,
    ChallengePeriodActive = 16,
    ChallengePeriodExpired = 17,
}

/// `RefractTreasury` is a token-custody vault with a segregated emergency reserve pool
/// and canonical asset decimal normalization support.
///
/// Standard funds reside in operating balance and can be withdrawn by admin.
/// Emergency reserves are strictly segregated and can only be released by the
/// governance timelock contract. Canonical payout methods normalize protocol 1e7
/// units to native token units applying floor rounding.
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
        env.events()
            .publish((symbol_short!("treasury"), symbol_short!("init")), (admin,));
        Ok(())
    }

    /// Explicit post-restore consistency check and state integrity verification.
    /// Asserts that required instance storage entries are live (failing closed with
    /// `StateArchived` instead of zero/default reads), verifies that reserve balance
    /// and operating balance are non-negative, and reconciles that the actual contract token
    /// balance is at least equal to reserve_balance + operating_balance.
    pub fn on_restore(env: Env, token: Address) -> Result<(i128, i128), TreasuryError> {
        let (operating, reserve) = Self::assert_live_and_consistent(&env, &token)?;
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("restore")),
            (token, operating, reserve),
        );
        Ok((operating, reserve))
    }

    /// Read-only integrity verification check for a given token.
    pub fn verify_integrity(env: Env, token: Address) -> Result<bool, TreasuryError> {
        Self::assert_live_and_consistent(&env, &token).map(|_| true)
    }

    fn assert_live_and_consistent(
        env: &Env,
        token: &Address,
    ) -> Result<(i128, i128), TreasuryError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::StateArchived);
        }
        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBalance(token.clone()))
            .unwrap_or(0);
        let operating: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OperatingBalance(token.clone()))
            .unwrap_or(0);

        if reserve < 0 || operating < 0 {
            return Err(TreasuryError::InconsistentState);
        }

        let total_recorded = reserve
            .checked_add(operating)
            .ok_or(TreasuryError::InconsistentState)?;
        let real_balance = token::Client::new(env, token).balance(&env.current_contract_address());
        if real_balance < total_recorded {
            return Err(TreasuryError::InconsistentState);
        }

        Ok((operating, reserve))
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
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("timelock")),
            (admin, timelock),
        );
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
            .set(&DataKey::ReserveBalance(token.clone()), &new_reserve);

        env.events().publish(
            (symbol_short!("reserve"), symbol_short!("deposit")),
            (from, token, amount),
        );

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
        env.storage().instance().set(
            &DataKey::LastRelease(token.clone()),
            &env.ledger().timestamp(),
        );

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

        let total_balance =
            token::Client::new(&env, &token).balance(&env.current_contract_address());
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
            .set(&DataKey::OperatingBalance(token.clone()), &new_operating);

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("withdraw")),
            (caller, token, amount, to),
        );

        Ok(())
    }

    /// Convert token units to protocol units (1e7, rounding Ceil for inflow).
    pub fn to_protocol_units(
        _env: Env,
        amount: i128,
        token_decimals: u32,
    ) -> Result<i128, NormalizationError> {
        normalization::to_protocol_units(amount, token_decimals)
    }

    /// Convert protocol units (1e7) to token units (rounding Floor for outflow/payout).
    pub fn to_token_units(
        _env: Env,
        protocol_amount: i128,
        token_decimals: u32,
    ) -> Result<i128, NormalizationError> {
        normalization::to_token_units(protocol_amount, token_decimals)
    }

    /// Withdraw using canonical protocol units (1e7), normalizing to native token units
    /// with floor rounding (payout/outflow protection against dust overpayment).
    /// Returns the actual native token amount transferred.
    pub fn withdraw_canonical(
        env: Env,
        caller: Address,
        token: Address,
        protocol_amount: i128,
        to: Address,
    ) -> Result<i128, TreasuryError> {
        let decimals = token::Client::new(&env, &token).decimals();
        let token_amount = normalization::to_token_units(protocol_amount, decimals)
            .map_err(|_| TreasuryError::NormalizationError)?;
        Self::withdraw(env, caller, token, token_amount, to)?;
        Ok(token_amount)
    }

    /// Release emergency reserve using canonical protocol units (1e7), normalizing to native token units
    /// with floor rounding (payout/outflow protection against dust overpayment).
    /// Returns the actual native token amount transferred.
    pub fn release_reserve_canonical(
        env: Env,
        caller: Address,
        token: Address,
        protocol_amount: i128,
        to: Address,
        purpose_tag: Symbol,
    ) -> Result<i128, TreasuryError> {
        let decimals = token::Client::new(&env, &token).decimals();
        let token_amount = normalization::to_token_units(protocol_amount, decimals)
            .map_err(|_| TreasuryError::NormalizationError)?;
        Self::release_reserve(env, caller, token, token_amount, to, purpose_tag)?;
        Ok(token_amount)
    }
    /// Configure high-value withdrawal challenge threshold and window duration.
    /// Only callable by admin/governance.
    pub fn configure_challenge_period(
        env: Env,
        caller: Address,
        challenge_threshold_amount: i128,
        challenge_window_seconds: u64,
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
        if challenge_threshold_amount <= 0 || challenge_window_seconds == 0 {
            return Err(TreasuryError::InvalidChallengeWindow);
        }

        env.storage().instance().set(
            &DataKey::ChallengeThresholdAmount,
            &challenge_threshold_amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::ChallengeWindowSeconds, &challenge_window_seconds);

        env.events().publish(
            (Symbol::new(&env, "ChallengeConfigured"), admin),
            (challenge_threshold_amount, challenge_window_seconds),
        );

        Ok(())
    }

    /// Retrieve the current challenge period configuration:
    /// (challenge_threshold_amount, challenge_window_seconds).
    pub fn get_challenge_config(env: Env) -> (i128, u64) {
        let threshold = env
            .storage()
            .instance()
            .get(&DataKey::ChallengeThresholdAmount)
            .unwrap_or(0);
        let window = env
            .storage()
            .instance()
            .get(&DataKey::ChallengeWindowSeconds)
            .unwrap_or(0);
        (threshold, window)
    }

    /// Propose a withdrawal from operating balance. Only callable by admin.
    /// If amount < challenge_threshold_amount (or threshold not configured), fast-path can still
    /// be proposed and immediately executed via execute_withdrawal, or executed directly via withdraw.
    /// Returns the unique withdrawal_id.
    pub fn propose_withdrawal(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<u64, TreasuryError> {
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

        let total_balance =
            token::Client::new(&env, &token).balance(&env.current_contract_address());
        let reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBalance(token.clone()))
            .unwrap_or(0);
        let operating = total_balance - reserve;
        if amount > operating {
            return Err(TreasuryError::InsufficientBalance);
        }

        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextWithdrawalId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextWithdrawalId, &(next_id + 1));

        let now = env.ledger().timestamp();
        let record = WithdrawalRecord {
            id: next_id,
            token: token.clone(),
            amount,
            to: to.clone(),
            proposed_at: now,
            status: WithdrawalStatus::Pending,
            challenger: None,
            challenge_reason: None,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(next_id), &record);

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("prop_wdr")),
            (next_id, token, amount, to),
        );

        Ok(next_id)
    }

    /// Retrieve a withdrawal record by ID.
    pub fn get_withdrawal(env: Env, withdrawal_id: u64) -> Result<WithdrawalRecord, TreasuryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Withdrawal(withdrawal_id))
            .ok_or(TreasuryError::WithdrawalNotFound)
    }

    /// Raise a challenge against a proposed withdrawal.
    /// Can be called by any stakeholder within the challenge window.
    /// Pauses execution and transitions the withdrawal to `Challenged`.
    pub fn raise_challenge(
        env: Env,
        challenger: Address,
        withdrawal_id: u64,
        reason: SorobanString,
    ) -> Result<(), TreasuryError> {
        challenger.require_auth();

        let mut withdrawal = Self::get_withdrawal(env.clone(), withdrawal_id)?;

        if withdrawal.status != WithdrawalStatus::Pending {
            return Err(TreasuryError::InvalidWithdrawalStatus);
        }

        let (threshold, window) = Self::get_challenge_config(env.clone());
        if threshold > 0 && withdrawal.amount >= threshold {
            let now = env.ledger().timestamp();
            let deadline = withdrawal
                .proposed_at
                .checked_add(window)
                .ok_or(TreasuryError::InconsistentState)?;
            if now >= deadline {
                return Err(TreasuryError::ChallengePeriodExpired);
            }
        }

        withdrawal.status = WithdrawalStatus::Challenged;
        withdrawal.challenger = Some(challenger.clone());
        withdrawal.challenge_reason = Some(reason.clone());

        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(withdrawal_id), &withdrawal);

        env.events().publish(
            (Symbol::new(&env, "ChallengeRaised"), withdrawal_id),
            (challenger, withdrawal.amount, reason),
        );

        Ok(())
    }

    /// Resolve an active challenge on a withdrawal.
    /// Strictly governance/admin only.
    /// If `approved` is true, status becomes `Approved` (clearing the challenge block).
    /// If `approved` is false, status becomes `Cancelled`.
    pub fn resolve_challenge(
        env: Env,
        governance: Address,
        withdrawal_id: u64,
        approved: bool,
    ) -> Result<(), TreasuryError> {
        governance.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if governance != admin {
            return Err(TreasuryError::Unauthorized);
        }

        let mut withdrawal = Self::get_withdrawal(env.clone(), withdrawal_id)?;

        if withdrawal.status != WithdrawalStatus::Challenged {
            return Err(TreasuryError::InvalidWithdrawalStatus);
        }

        if approved {
            withdrawal.status = WithdrawalStatus::Approved;
        } else {
            withdrawal.status = WithdrawalStatus::Cancelled;
        }

        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(withdrawal_id), &withdrawal);

        env.events().publish(
            (Symbol::new(&env, "ChallengeResolved"), withdrawal_id),
            (governance, approved),
        );

        Ok(())
    }

    /// Execute a withdrawal after validations.
    /// Checks:
    /// - If `amount >= challenge_threshold_amount`, withdrawal is permitted ONLY after
    ///   `challenge_window_seconds` has elapsed from `proposed_at`, unless already explicitly
    ///   `Approved` by governance resolution.
    /// - Fails with `WithdrawalChallenged` if currently in `Challenged` status.
    /// - Fails with `ChallengePeriodActive` if within challenge window and not approved.
    pub fn execute_withdrawal(
        env: Env,
        caller: Address,
        withdrawal_id: u64,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();

        let mut withdrawal = Self::get_withdrawal(env.clone(), withdrawal_id)?;

        match withdrawal.status {
            WithdrawalStatus::Pending => {
                let (threshold, window) = Self::get_challenge_config(env.clone());
                if threshold > 0 && withdrawal.amount >= threshold {
                    let now = env.ledger().timestamp();
                    let deadline = withdrawal
                        .proposed_at
                        .checked_add(window)
                        .ok_or(TreasuryError::InconsistentState)?;
                    if now < deadline {
                        return Err(TreasuryError::ChallengePeriodActive);
                    }
                }
            }
            WithdrawalStatus::Approved => {
                // Approved by governance resolution after a challenge; ready to execute
            }
            WithdrawalStatus::Challenged => {
                return Err(TreasuryError::WithdrawalChallenged);
            }
            WithdrawalStatus::Executed | WithdrawalStatus::Cancelled => {
                return Err(TreasuryError::InvalidWithdrawalStatus);
            }
        }

        // Perform the token transfer and update operating balance
        let token = withdrawal.token.clone();
        let amount = withdrawal.amount;
        let to = withdrawal.to.clone();

        let total_balance =
            token::Client::new(&env, &token).balance(&env.current_contract_address());
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
            .set(&DataKey::OperatingBalance(token.clone()), &new_operating);

        withdrawal.status = WithdrawalStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(withdrawal_id), &withdrawal);

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("withdraw")),
            (caller, token, amount, to),
        );

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
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
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
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
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
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let depositor = Address::generate(&env);
        sac.mint(&depositor, &1_000);
        client.deposit_reserve(&depositor, &token_id, &1_000);

        let stranger = Address::generate(&env);
        let to = Address::generate(&env);

        // Stranger fails
        assert_eq!(
            client.try_release_reserve(&stranger, &token_id, &100, &to, &symbol_short!("payout")),
            Err(Ok(TreasuryError::UnauthorizedTimelock))
        );

        // Admin also fails because only timelock can release reserve
        assert_eq!(
            client.try_release_reserve(&admin, &token_id, &100, &to, &symbol_short!("payout")),
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
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let depositor = Address::generate(&env);
        sac.mint(&depositor, &1_000);
        client.deposit_reserve(&depositor, &token_id, &1_000);

        client.set_reserve_target(&admin, &token_id, &800);

        env.ledger().with_mut(|l| l.timestamp = 1_700_000_000);

        let to = Address::generate(&env);
        client.release_reserve(&timelock, &token_id, &400, &to, &symbol_short!("disaster"));

        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);

        let (reserve_bal, target, last_rel) = client.get_reserve_info(&token_id);
        assert_eq!(reserve_bal, 600);
        assert_eq!(target, 800);
        assert_eq!(last_rel, 1_700_000_000);

        // Cannot release more than remaining reserve
        assert_eq!(
            client.try_release_reserve(&timelock, &token_id, &700, &to, &symbol_short!("excess")),
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

    #[test]
    fn test_canonical_conversion_entrypoints() {
        let env = Env::default();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        // 6 decimals: 1_000_000 -> 10_000_000
        assert_eq!(client.to_protocol_units(&1_000_000, &6), 10_000_000);
        assert_eq!(client.to_token_units(&10_000_000, &6), 1_000_000);

        // 7 decimals: 10_000_000 -> 10_000_000
        assert_eq!(client.to_protocol_units(&10_000_000, &7), 10_000_000);
        assert_eq!(client.to_token_units(&10_000_000, &7), 10_000_000);

        // 18 decimals: 10^18 -> 10_000_000
        let weth = 1_000_000_000_000_000_000i128;
        assert_eq!(client.to_protocol_units(&weth, &18), 10_000_000);
        assert_eq!(client.to_token_units(&10_000_000, &18), weth);
    }

    #[test]
    fn test_canonical_withdraw_and_release() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let timelock = Address::generate(&env);
        client.set_timelock(&admin, &timelock);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);

        // Stellar asset contract has 7 decimals (1:1 with protocol scale)
        assert_eq!(token_client.decimals(), 7);

        // Mint operating funds: 50_000_000 (5 tokens at 1e7)
        sac.mint(&contract_id, &50_000_000);

        // Deposit reserve: 20_000_000 (2 tokens at 1e7)
        let depositor = Address::generate(&env);
        sac.mint(&depositor, &20_000_000);
        client.deposit_reserve(&depositor, &token_id, &20_000_000);

        let to = Address::generate(&env);

        // Withdraw 10_000_000 protocol units canonically
        let withdrawn = client.withdraw_canonical(&admin, &token_id, &10_000_000, &to);
        assert_eq!(withdrawn, 10_000_000);
        assert_eq!(token_client.balance(&to), 10_000_000);

        // Release 5_000_000 protocol units from reserve canonically
        let released = client.release_reserve_canonical(
            &timelock,
            &token_id,
            &5_000_000,
            &to,
            &symbol_short!("grant"),
        );
        assert_eq!(released, 5_000_000);
        assert_eq!(token_client.balance(&to), 15_000_000);

        let (reserve_bal, _, _) = client.get_reserve_info(&token_id);
        assert_eq!(reserve_bal, 15_000_000);
    }

    #[test]
    fn test_treasury_on_restore_and_integrity_checks() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit reserve and operating funds
        let depositor = Address::generate(&env);
        sac.mint(&depositor, &50_000_000);
        client.deposit_reserve(&depositor, &token_id, &50_000_000);

        sac.mint(&contract_id, &50_000_000);

        let (operating, reserve) = client.on_restore(&token_id);
        assert_eq!(reserve, 50_000_000);
        assert_eq!(operating, 0); // initial operating set on withdraw or defaults to 0
        assert!(client.verify_integrity(&token_id));

        // Simulate state inconsistency where tracked operating balance exceeds physical tokens
        env.as_contract(&contract_id, || {
            env.storage().instance().set(
                &DataKey::OperatingBalance(token_id.clone()),
                &200_000_000i128,
            );
        });

        assert_eq!(
            client.try_on_restore(&token_id),
            Err(Ok(TreasuryError::InconsistentState))
        );
        assert_eq!(
            client.try_verify_integrity(&token_id),
            Err(Ok(TreasuryError::InconsistentState))
        );
    }

    #[test]
    fn test_treasury_uninitialized_or_archived_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let token = Address::generate(&env);
        assert_eq!(
            client.try_on_restore(&token),
            Err(Ok(TreasuryError::StateArchived))
        );
        assert_eq!(
            client.try_verify_integrity(&token),
            Err(Ok(TreasuryError::StateArchived))
        );
        assert_eq!(client.try_admin(), Err(Ok(TreasuryError::NotInitialized)));
    }

    #[test]
    fn test_challenge_period_configuration_and_permissions() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let stranger = Address::generate(&env);
        client.initialize(&admin);

        // Unauthorized configure
        let unauth = client.try_configure_challenge_period(&stranger, &1_000_000, &86400);
        assert_eq!(unauth, Err(Ok(TreasuryError::Unauthorized)));

        // Invalid parameters
        let invalid_thresh = client.try_configure_challenge_period(&admin, &0, &86400);
        assert_eq!(
            invalid_thresh,
            Err(Ok(TreasuryError::InvalidChallengeWindow))
        );

        let invalid_window = client.try_configure_challenge_period(&admin, &1_000_000, &0);
        assert_eq!(
            invalid_window,
            Err(Ok(TreasuryError::InvalidChallengeWindow))
        );

        // Valid configuration
        client.configure_challenge_period(&admin, &50_000, &3600);
        let (thresh, win) = client.get_challenge_config();
        assert_eq!(thresh, 50_000);
        assert_eq!(win, 3600);
    }

    #[test]
    fn test_high_value_withdrawal_challenge_and_resolution_lifecycle() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let challenger = Address::generate(&env);
        let recipient = Address::generate(&env);
        let stranger = Address::generate(&env);

        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);

        // Configure challenge period: threshold = 10_000, window = 100 seconds
        client.configure_challenge_period(&admin, &10_000, &100);

        // Mint operating funds to treasury contract
        sac.mint(&contract_id, &100_000);

        // 1. Below threshold (fast path proposal): 5_000 < 10_000
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let low_val_id = client.propose_withdrawal(&admin, &token_id, &5_000, &recipient);
        assert_eq!(low_val_id, 1);

        // Immediately executable even at timestamp 1000 without waiting for window
        client.execute_withdrawal(&admin, &low_val_id);
        assert_eq!(token_client.balance(&recipient), 5_000);

        let low_rec = client.get_withdrawal(&low_val_id);
        assert_eq!(low_rec.status, WithdrawalStatus::Executed);

        // Re-executing already executed fails
        assert_eq!(
            client.try_execute_withdrawal(&admin, &low_val_id),
            Err(Ok(TreasuryError::InvalidWithdrawalStatus))
        );

        // 2. High value withdrawal: 20_000 >= 10_000
        env.ledger().with_mut(|l| l.timestamp = 2000);
        let high_val_id = client.propose_withdrawal(&admin, &token_id, &20_000, &recipient);
        assert_eq!(high_val_id, 2);

        // Attempting to execute before challenge window (timestamp 2050 < 2000 + 100) fails
        env.ledger().with_mut(|l| l.timestamp = 2050);
        assert_eq!(
            client.try_execute_withdrawal(&admin, &high_val_id),
            Err(Ok(TreasuryError::ChallengePeriodActive))
        );

        // Stakeholder raises challenge during the window
        let reason = SorobanString::from_str(&env, "Suspicious destination address");
        client.raise_challenge(&challenger, &high_val_id, &reason);

        let challenged_rec = client.get_withdrawal(&high_val_id);
        assert_eq!(challenged_rec.status, WithdrawalStatus::Challenged);
        assert_eq!(challenged_rec.challenger, Some(challenger.clone()));
        assert_eq!(challenged_rec.challenge_reason, Some(reason));

        // Attempt to execute even after window has passed (e.g. timestamp 2500) fails because it is Challenged
        env.ledger().with_mut(|l| l.timestamp = 2500);
        assert_eq!(
            client.try_execute_withdrawal(&admin, &high_val_id),
            Err(Ok(TreasuryError::WithdrawalChallenged))
        );

        // Unauthorized caller cannot resolve challenge
        assert_eq!(
            client.try_resolve_challenge(&stranger, &high_val_id, &true),
            Err(Ok(TreasuryError::Unauthorized))
        );

        // Governance resolves challenge: approved = true
        client.resolve_challenge(&admin, &high_val_id, &true);
        let resolved_rec = client.get_withdrawal(&high_val_id);
        assert_eq!(resolved_rec.status, WithdrawalStatus::Approved);

        // Now execution succeeds
        client.execute_withdrawal(&admin, &high_val_id);
        assert_eq!(token_client.balance(&recipient), 25_000);

        let executed_rec = client.get_withdrawal(&high_val_id);
        assert_eq!(executed_rec.status, WithdrawalStatus::Executed);

        // 3. High value withdrawal cancelled by governance
        let high_val_id3 = client.propose_withdrawal(&admin, &token_id, &30_000, &recipient);
        assert_eq!(high_val_id3, 3);

        let cancel_reason = SorobanString::from_str(&env, "Fraudulent transaction reported");
        client.raise_challenge(&challenger, &high_val_id3, &cancel_reason);

        // Governance cancels withdrawal: approved = false
        client.resolve_challenge(&admin, &high_val_id3, &false);
        let cancelled_rec = client.get_withdrawal(&high_val_id3);
        assert_eq!(cancelled_rec.status, WithdrawalStatus::Cancelled);

        // Execution of cancelled withdrawal fails
        assert_eq!(
            client.try_execute_withdrawal(&admin, &high_val_id3),
            Err(Ok(TreasuryError::InvalidWithdrawalStatus))
        );

        // 4. Challenge after window has expired is rejected
        env.ledger().with_mut(|l| l.timestamp = 3000);
        let high_val_id4 = client.propose_withdrawal(&admin, &token_id, &15_000, &recipient);
        assert_eq!(high_val_id4, 4);

        // Advance past window (timestamp 3150 >= 3000 + 100)
        env.ledger().with_mut(|l| l.timestamp = 3150);

        // Raising challenge now fails with ChallengePeriodExpired
        let late_reason = SorobanString::from_str(&env, "Too late challenge");
        assert_eq!(
            client.try_raise_challenge(&challenger, &high_val_id4, &late_reason),
            Err(Ok(TreasuryError::ChallengePeriodExpired))
        );

        // Execution succeeds because window passed with 0 challenges
        client.execute_withdrawal(&admin, &high_val_id4);
        assert_eq!(token_client.balance(&recipient), 40_000);
    }
}
