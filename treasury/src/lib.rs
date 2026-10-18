#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RoundReservationState {
    Reserved = 0,
    Settled = 1,
    Refunded = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoundBudgetReservation {
    pub round_id: u64,
    pub token: Address,
    pub budget: i128,
    pub state: RoundReservationState,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    InsufficientAvailableBalance = 4,
    RoundAlreadyReserved = 5,
    RoundNotFound = 6,
    RoundNotReserved = 7,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    ReservedBalance(Address),
    RoundReservation(u64),
}

/// `RefractTreasury` is a token-custody vault supporting upfront budget
/// reservations, settlement, and zero-vote/cancellation refunds for retroactive
/// public-goods funding rounds.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw or reserve round budgets.
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

    /// Return total reserved balance for `token` across active retroactive rounds.
    pub fn reserved_balance(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReservedBalance(token))
            .unwrap_or(0)
    }

    /// Return uncommitted balance available for new rounds or withdrawals.
    pub fn available_balance(env: Env, token: Address) -> i128 {
        let total = token::Client::new(&env, &token).balance(&env.current_contract_address());
        let reserved = Self::reserved_balance(env, token);
        (total - reserved).max(0)
    }

    /// Reserve `budget` of `token` for retroactive funding `round_id` so multiple
    /// rounds cannot over-commit the same treasury funds.
    pub fn reserve_round_budget(
        env: Env,
        caller: Address,
        round_id: u64,
        token: Address,
        budget: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        if caller != Self::admin(env.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if budget <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::RoundReservation(round_id))
        {
            return Err(TreasuryError::RoundAlreadyReserved);
        }
        if Self::available_balance(env.clone(), token.clone()) < budget {
            return Err(TreasuryError::InsufficientAvailableBalance);
        }

        let reserved = Self::reserved_balance(env.clone(), token.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(token.clone()),
            &(reserved + budget),
        );
        env.storage().persistent().set(
            &DataKey::RoundReservation(round_id),
            &RoundBudgetReservation {
                round_id,
                token: token.clone(),
                budget,
                state: RoundReservationState::Reserved,
            },
        );

        env.events()
            .publish((symbol_short!("rnd_rsv"), round_id), (token, budget));
        Ok(())
    }

    /// Refund reserved budget back to available treasury balance when a round is
    /// cancelled during the challenge window or concludes with zero votes.
    pub fn refund_round_budget(
        env: Env,
        caller: Address,
        round_id: u64,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        if caller != Self::admin(env.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        let mut rsv: RoundBudgetReservation = env
            .storage()
            .persistent()
            .get(&DataKey::RoundReservation(round_id))
            .ok_or(TreasuryError::RoundNotFound)?;
        if rsv.state != RoundReservationState::Reserved {
            return Err(TreasuryError::RoundNotReserved);
        }

        rsv.state = RoundReservationState::Refunded;
        env.storage()
            .persistent()
            .set(&DataKey::RoundReservation(round_id), &rsv);

        let reserved = Self::reserved_balance(env.clone(), rsv.token.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(rsv.token.clone()),
            &(reserved - rsv.budget),
        );

        env.events().publish(
            (symbol_short!("rnd_rfnd"), round_id),
            (rsv.token, rsv.budget),
        );
        Ok(rsv.budget)
    }

    /// Settle reserved round budget by transferring `budget` to `grants_contract`.
    pub fn settle_round_budget(
        env: Env,
        caller: Address,
        round_id: u64,
        grants_contract: Address,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        if caller != Self::admin(env.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        let mut rsv: RoundBudgetReservation = env
            .storage()
            .persistent()
            .get(&DataKey::RoundReservation(round_id))
            .ok_or(TreasuryError::RoundNotFound)?;
        if rsv.state != RoundReservationState::Reserved {
            return Err(TreasuryError::RoundNotReserved);
        }

        rsv.state = RoundReservationState::Settled;
        env.storage()
            .persistent()
            .set(&DataKey::RoundReservation(round_id), &rsv);

        let reserved = Self::reserved_balance(env.clone(), rsv.token.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(rsv.token.clone()),
            &(reserved - rsv.budget),
        );

        token::Client::new(&env, &rsv.token).transfer(
            &env.current_contract_address(),
            &grants_contract,
            &rsv.budget,
        );

        env.events().publish(
            (symbol_short!("rnd_stld"), round_id),
            (rsv.token, rsv.budget, grants_contract),
        );
        Ok(rsv.budget)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this, and
    /// it cannot withdraw funds reserved for active retroactive rounds.
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
        if Self::available_balance(env.clone(), token.clone()) < amount {
            panic!("insufficient available balance");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn round_budget_reservation_prevents_overcommitment_and_supports_refund_and_settle() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let grants = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        assert_eq!(client.available_balance(&token_id), 1_000);

        client.reserve_round_budget(&admin, &1, &token_id, &600);
        assert_eq!(client.available_balance(&token_id), 400);

        // Second round attempting to reserve 500 fails due to insufficient available balance.
        assert_eq!(
            client.try_reserve_round_budget(&admin, &2, &token_id, &500),
            Err(Ok(TreasuryError::InsufficientAvailableBalance))
        );

        // Refunding round 1 restores available balance.
        assert_eq!(client.refund_round_budget(&admin, &1), 600);
        assert_eq!(client.available_balance(&token_id), 1_000);

        // Reserve and settle round 2.
        client.reserve_round_budget(&admin, &2, &token_id, &500);
        assert_eq!(client.settle_round_budget(&admin, &2, &grants), 500);
        assert_eq!(token_client.balance(&grants), 500);
        assert_eq!(client.available_balance(&token_id), 500);
    }
}
