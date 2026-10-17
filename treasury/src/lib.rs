#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InvalidExpiry = 5,
    InsufficientAvailableBalance = 6,
    ReservationNotFound = 7,
    ReservationNotActive = 8,
    ReservationExpired = 9,
    ReservationNotExpired = 10,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReservationStatus {
    Active = 0,
    Released = 1,
    Consumed = 2,
    Expired = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservationRecord {
    pub reservation_id: u64,
    pub consumer: Address,
    pub purpose: Symbol,
    pub nonce: u64,
    pub asset: Address,
    pub amount: i128,
    pub remaining_amount: i128,
    pub expiry_ledger: u32,
    pub status: ReservationStatus,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    NextReservationId,
    ConsumerAllowed(Address),
    Reservation(u64),
    IdempotencyKey(Address, Symbol, u64),
    TotalReserved(Address),
    ConsumerReserved(Address, Address),
}

/// `RefractTreasury` is a token-custody vault with a cross-contract capital
/// reservation ledger that prevents double-commitment of treasury funds across
/// `RefractBountyEscrow`, `RefractGrants`, and `RefractBackstop`.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds and
    /// manage allowlisted consumer contracts.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::NextReservationId, &1u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Allowlist a consumer contract (`bounty`, `grants`, `backstop`) to place
    /// and draw down capital reservations.
    pub fn register_consumer(
        env: Env,
        caller: Address,
        consumer: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::ConsumerAllowed(consumer.clone()), &true);
        env.events()
            .publish((symbol_short!("cons_reg"), consumer), caller);
        Ok(())
    }

    /// Revoke a consumer contract's permission to create new reservations.
    pub fn revoke_consumer(
        env: Env,
        caller: Address,
        consumer: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::ConsumerAllowed(consumer.clone()), &false);
        env.events()
            .publish((symbol_short!("cons_rev"), consumer), caller);
        Ok(())
    }

    /// Return whether `consumer` is an allowlisted reservation consumer.
    pub fn is_consumer(env: Env, consumer: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::ConsumerAllowed(consumer))
            .unwrap_or(false)
    }

    /// Reserve `amount` of `asset` for `(consumer, purpose, nonce)` until
    /// `expiry_ledger`. Idempotent on `(consumer, purpose, nonce)`: if a
    /// reservation already exists for that tuple, returns its `reservation_id`
    /// without double-reserving.
    pub fn reserve_capital(
        env: Env,
        consumer: Address,
        purpose: Symbol,
        nonce: u64,
        asset: Address,
        amount: i128,
        expiry_ledger: u32,
    ) -> Result<u64, TreasuryError> {
        consumer.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if consumer != admin && !Self::is_consumer(env.clone(), consumer.clone()) {
            return Err(TreasuryError::Unauthorized);
        }

        let idem_key = DataKey::IdempotencyKey(consumer.clone(), purpose.clone(), nonce);
        if let Some(existing_id) = env.storage().persistent().get::<DataKey, u64>(&idem_key) {
            return Ok(existing_id);
        }

        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if expiry_ledger <= env.ledger().sequence() {
            return Err(TreasuryError::InvalidExpiry);
        }

        let available = Self::available_balance(env.clone(), asset.clone());
        if amount > available {
            return Err(TreasuryError::InsufficientAvailableBalance);
        }

        let reservation_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextReservationId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextReservationId, &(reservation_id + 1));

        let record = ReservationRecord {
            reservation_id,
            consumer: consumer.clone(),
            purpose: purpose.clone(),
            nonce,
            asset: asset.clone(),
            amount,
            remaining_amount: amount,
            expiry_ledger,
            status: ReservationStatus::Active,
        };

        let total_res = Self::total_reserved(env.clone(), asset.clone());
        let cons_res = Self::consumer_reserved(env.clone(), consumer.clone(), asset.clone());

        env.storage()
            .persistent()
            .set(&DataKey::Reservation(reservation_id), &record);
        env.storage().persistent().set(&idem_key, &reservation_id);
        env.storage().persistent().set(
            &DataKey::TotalReserved(asset.clone()),
            &(total_res + amount),
        );
        env.storage().persistent().set(
            &DataKey::ConsumerReserved(consumer.clone(), asset.clone()),
            &(cons_res + amount),
        );

        env.events().publish(
            (Symbol::new(&env, "reservation_created"), reservation_id),
            (consumer, purpose, asset, amount, expiry_ledger),
        );

        Ok(reservation_id)
    }

    /// Release an active reservation early, freeing its remaining reserved
    /// balance back into `available_balance`. Callable by the owning consumer
    /// or the treasury admin.
    pub fn release_reservation(
        env: Env,
        caller: Address,
        reservation_id: u64,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        let mut record: ReservationRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
            .ok_or(TreasuryError::ReservationNotFound)?;

        if caller != record.consumer && caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if record.status != ReservationStatus::Active {
            return Err(TreasuryError::ReservationNotActive);
        }

        let released = record.remaining_amount;
        record.remaining_amount = 0;
        record.status = ReservationStatus::Released;

        Self::decrement_reserved(&env, &record.consumer, &record.asset, released);
        env.storage()
            .persistent()
            .set(&DataKey::Reservation(reservation_id), &record);

        env.events().publish(
            (Symbol::new(&env, "reservation_released"), reservation_id),
            (record.consumer, record.asset, released),
        );

        Ok(released)
    }

    /// Draw down `amount` from an active, non-expired reservation and transfer
    /// the tokens to `recipient` atomically.
    pub fn draw_down_reservation(
        env: Env,
        consumer: Address,
        reservation_id: u64,
        recipient: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        consumer.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        let mut record: ReservationRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
            .ok_or(TreasuryError::ReservationNotFound)?;

        if consumer != record.consumer && consumer != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if record.status != ReservationStatus::Active {
            return Err(TreasuryError::ReservationNotActive);
        }
        if env.ledger().sequence() > record.expiry_ledger {
            return Err(TreasuryError::ReservationExpired);
        }
        if amount <= 0 || amount > record.remaining_amount {
            return Err(TreasuryError::InvalidAmount);
        }

        record.remaining_amount -= amount;
        if record.remaining_amount == 0 {
            record.status = ReservationStatus::Consumed;
        }

        Self::decrement_reserved(&env, &record.consumer, &record.asset, amount);
        env.storage()
            .persistent()
            .set(&DataKey::Reservation(reservation_id), &record);

        token::Client::new(&env, &record.asset).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "reservation_drawn_down"), reservation_id),
            (consumer, recipient, amount, record.remaining_amount),
        );

        Ok(amount)
    }

    /// Permissionlessly reclaim an expired reservation after `expiry_ledger`
    /// has passed, returning its remaining reserved balance to
    /// `available_balance` and emitting `reservation_expired`.
    pub fn reclaim_expired_reservation(
        env: Env,
        reservation_id: u64,
    ) -> Result<i128, TreasuryError> {
        let mut record: ReservationRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
            .ok_or(TreasuryError::ReservationNotFound)?;

        if record.status != ReservationStatus::Active {
            return Err(TreasuryError::ReservationNotActive);
        }
        if env.ledger().sequence() <= record.expiry_ledger {
            return Err(TreasuryError::ReservationNotExpired);
        }

        let reclaimed = record.remaining_amount;
        record.remaining_amount = 0;
        record.status = ReservationStatus::Expired;

        Self::decrement_reserved(&env, &record.consumer, &record.asset, reclaimed);
        env.storage()
            .persistent()
            .set(&DataKey::Reservation(reservation_id), &record);

        env.events().publish(
            (Symbol::new(&env, "reservation_expired"), reservation_id),
            (record.consumer, record.asset, reclaimed),
        );

        Ok(reclaimed)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this, and
    /// the withdrawal cannot spend funds currently locked in active
    /// reservations (`amount <= available_balance(token)`).
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
        let available = Self::available_balance(env.clone(), token.clone());
        if amount > available {
            panic!("insufficient available balance");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    /// Fallible variant of `withdraw` that returns a typed `TreasuryError`.
    pub fn withdraw_checked(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let available = Self::available_balance(env.clone(), token.clone());
        if amount > available {
            return Err(TreasuryError::InsufficientAvailableBalance);
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Unencumbered balance of `asset` available for new reservations or direct
    /// withdrawals (`token_balance - total_reserved`).
    pub fn available_balance(env: Env, asset: Address) -> i128 {
        let balance = token::Client::new(&env, &asset).balance(&env.current_contract_address());
        let reserved = Self::total_reserved(env, asset);
        (balance - reserved).max(0)
    }

    /// Total active reserved amount across all consumers for `asset`.
    pub fn total_reserved(env: Env, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalReserved(asset))
            .unwrap_or(0)
    }

    /// Total active reserved amount held by `consumer` for `asset`.
    pub fn consumer_reserved(env: Env, consumer: Address, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::ConsumerReserved(consumer, asset))
            .unwrap_or(0)
    }

    /// Look up a reservation record by `reservation_id`.
    pub fn reservation(env: Env, reservation_id: u64) -> Option<ReservationRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Reservation(reservation_id))
    }

    fn require_admin_addr(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn decrement_reserved(env: &Env, consumer: &Address, asset: &Address, amount: i128) {
        let total_res = Self::total_reserved(env.clone(), asset.clone());
        let cons_res = Self::consumer_reserved(env.clone(), consumer.clone(), asset.clone());
        env.storage().persistent().set(
            &DataKey::TotalReserved(asset.clone()),
            &(total_res - amount).max(0),
        );
        env.storage().persistent().set(
            &DataKey::ConsumerReserved(consumer.clone(), asset.clone()),
            &(cons_res - amount).max(0),
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    fn setup() -> (
        Env,
        Address,
        RefractTreasuryClient<'static>,
        Address,
        token::Client<'static>,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.sequence_number = 100);

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&contract_id, &1_000);

        (env, contract_id, client, token_id, token_client, admin)
    }

    #[test]
    fn withdraw_is_admin_gated() {
        let (env, contract_id, client, token_id, token_client, admin) = setup();
        assert_eq!(client.admin(), admin);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }

    #[test]
    fn competing_consumers_cannot_double_commit_balance() {
        let (env, _contract_id, client, token_id, _token_client, admin) = setup();
        let bounty_consumer = Address::generate(&env);
        let grants_consumer = Address::generate(&env);

        client.register_consumer(&admin, &bounty_consumer);
        client.register_consumer(&admin, &grants_consumer);

        let res_1 = client.reserve_capital(
            &bounty_consumer,
            &symbol_short!("bounty"),
            &1,
            &token_id,
            &700,
            &200,
        );
        assert_eq!(res_1, 1);
        assert_eq!(client.total_reserved(&token_id), 700);
        assert_eq!(client.available_balance(&token_id), 300);

        // Second consumer tries to reserve 400 when only 300 is available.
        let competing = client.try_reserve_capital(
            &grants_consumer,
            &symbol_short!("grant"),
            &1,
            &token_id,
            &400,
            &200,
        );
        assert_eq!(
            competing,
            Err(Ok(TreasuryError::InsufficientAvailableBalance))
        );

        // Direct admin withdrawal exceeding available_balance (300) is also rejected.
        let recipient = Address::generate(&env);
        assert_eq!(
            client.try_withdraw_checked(&admin, &token_id, &350, &recipient),
            Err(Ok(TreasuryError::InsufficientAvailableBalance))
        );

        // Reserving the remaining 300 succeeds.
        let res_2 = client.reserve_capital(
            &grants_consumer,
            &symbol_short!("grant"),
            &2,
            &token_id,
            &300,
            &200,
        );
        assert_eq!(res_2, 2);
        assert_eq!(client.total_reserved(&token_id), 1_000);
        assert_eq!(client.available_balance(&token_id), 0);
    }

    #[test]
    fn reserve_capital_is_idempotent_on_consumer_purpose_nonce() {
        let (env, _contract_id, client, token_id, _token_client, admin) = setup();
        let consumer = Address::generate(&env);
        client.register_consumer(&admin, &consumer);

        let id_a = client.reserve_capital(
            &consumer,
            &symbol_short!("bounty"),
            &42,
            &token_id,
            &500,
            &300,
        );
        let id_b = client.reserve_capital(
            &consumer,
            &symbol_short!("bounty"),
            &42,
            &token_id,
            &500,
            &300,
        );
        assert_eq!(id_a, id_b);
        assert_eq!(client.total_reserved(&token_id), 500);
        assert_eq!(client.consumer_reserved(&consumer, &token_id), 500);
    }

    #[test]
    fn partial_and_full_draw_down_and_release_lifecycle() {
        let (env, contract_id, client, token_id, token_client, admin) = setup();
        let consumer = Address::generate(&env);
        let recipient = Address::generate(&env);
        client.register_consumer(&admin, &consumer);

        let res_id = client.reserve_capital(
            &consumer,
            &symbol_short!("grant"),
            &7,
            &token_id,
            &600,
            &250,
        );

        // Partial drawdown of 250.
        let drawn_1 = client.draw_down_reservation(&consumer, &res_id, &recipient, &250);
        assert_eq!(drawn_1, 250);
        assert_eq!(token_client.balance(&recipient), 250);
        assert_eq!(token_client.balance(&contract_id), 750);
        assert_eq!(client.total_reserved(&token_id), 350);
        assert_eq!(client.available_balance(&token_id), 400);

        // Release remaining 350 back to available balance.
        let released = client.release_reservation(&consumer, &res_id);
        assert_eq!(released, 350);
        assert_eq!(client.total_reserved(&token_id), 0);
        assert_eq!(client.available_balance(&token_id), 750);

        let record = client.reservation(&res_id).unwrap();
        assert_eq!(record.status, ReservationStatus::Released);
        assert_eq!(record.remaining_amount, 0);

        // Drawing down or releasing again fails.
        assert_eq!(
            client.try_draw_down_reservation(&consumer, &res_id, &recipient, &100),
            Err(Ok(TreasuryError::ReservationNotActive))
        );
    }

    #[test]
    fn expired_reservation_cannot_be_drawn_and_can_be_reclaimed_permissionlessly() {
        let (env, _contract_id, client, token_id, _token_client, admin) = setup();
        let consumer = Address::generate(&env);
        let recipient = Address::generate(&env);
        client.register_consumer(&admin, &consumer);

        let res_id = client.reserve_capital(
            &consumer,
            &symbol_short!("backstop"),
            &99,
            &token_id,
            &800,
            &150,
        );

        // Cannot reclaim before expiry_ledger (150).
        assert_eq!(
            client.try_reclaim_expired_reservation(&res_id),
            Err(Ok(TreasuryError::ReservationNotExpired))
        );

        // Advance past expiry_ledger.
        env.ledger().with_mut(|li| li.sequence_number = 151);

        // Drawdown after expiry is rejected.
        assert_eq!(
            client.try_draw_down_reservation(&consumer, &res_id, &recipient, &200),
            Err(Ok(TreasuryError::ReservationExpired))
        );

        // Permissionless reclaim restores available_balance and marks Expired.
        let reclaimed = client.reclaim_expired_reservation(&res_id);
        assert_eq!(reclaimed, 800);
        assert_eq!(client.total_reserved(&token_id), 0);
        assert_eq!(client.available_balance(&token_id), 1_000);
        assert_eq!(
            client.reservation(&res_id).unwrap().status,
            ReservationStatus::Expired
        );

        // Reclaiming a second time is rejected.
        assert_eq!(
            client.try_reclaim_expired_reservation(&res_id),
            Err(Ok(TreasuryError::ReservationNotActive))
        );
    }
}
