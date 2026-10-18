#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PayoutStatus {
    Pending = 0,
    Claimed = 1,
    Swept = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutRecord {
    pub payout_id: u64,
    pub recipient: Address,
    pub asset: Address,
    pub amount: i128,
    pub claim_deadline: u64,
    pub status: PayoutStatus,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    InvalidDeadline = 4,
    InsufficientAvailableBalance = 5,
    PayoutNotFound = 6,
    NotRecipient = 7,
    AlreadyClaimed = 8,
    AlreadySwept = 9,
    DeadlineNotReached = 10,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    NextPayoutId,
    ReservedBalance(Address),
    Payout(u64),
}

/// `RefractTreasury` is a token-custody vault supporting pull-based per-recipient
/// escrow payouts with deadline sweep and reservation accounting.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw or create payouts.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextPayoutId, &0u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Return reserved balance for `token` across pending pull-based payouts.
    pub fn reserved_balance(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReservedBalance(token))
            .unwrap_or(0)
    }

    /// Return available (unreserved) balance for `token`.
    pub fn available_balance(env: Env, token: Address) -> i128 {
        let total = token::Client::new(&env, &token).balance(&env.current_contract_address());
        let reserved = Self::reserved_balance(env, token);
        (total - reserved).max(0)
    }

    /// Create an escrowed pull-based payout record and reserve funds from available balance.
    pub fn create_payout(
        env: Env,
        caller: Address,
        recipient: Address,
        asset: Address,
        amount: i128,
        claim_deadline: u64,
    ) -> Result<u64, TreasuryError> {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if claim_deadline <= env.ledger().timestamp() {
            return Err(TreasuryError::InvalidDeadline);
        }
        if Self::available_balance(env.clone(), asset.clone()) < amount {
            return Err(TreasuryError::InsufficientAvailableBalance);
        }

        let reserved = Self::reserved_balance(env.clone(), asset.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(asset.clone()),
            &(reserved + amount),
        );

        let payout_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextPayoutId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextPayoutId, &(payout_id + 1));

        let record = PayoutRecord {
            payout_id,
            recipient: recipient.clone(),
            asset: asset.clone(),
            amount,
            claim_deadline,
            status: PayoutStatus::Pending,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

        env.events().publish(
            (symbol_short!("pay_creat"), payout_id),
            (recipient, asset, amount, claim_deadline),
        );

        Ok(payout_id)
    }

    /// Pull-based claim callable only by the recorded recipient.
    pub fn claim(env: Env, caller: Address, payout_id: u64) -> Result<i128, TreasuryError> {
        caller.require_auth();
        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
            .ok_or(TreasuryError::PayoutNotFound)?;

        if caller != record.recipient {
            return Err(TreasuryError::NotRecipient);
        }
        match record.status {
            PayoutStatus::Claimed => return Err(TreasuryError::AlreadyClaimed),
            PayoutStatus::Swept => return Err(TreasuryError::AlreadySwept),
            PayoutStatus::Pending => {}
        }

        record.status = PayoutStatus::Claimed;
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

        let reserved = Self::reserved_balance(env.clone(), record.asset.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(record.asset.clone()),
            &(reserved - record.amount),
        );

        token::Client::new(&env, &record.asset).transfer(
            &env.current_contract_address(),
            &record.recipient,
            &record.amount,
        );

        env.events().publish(
            (symbol_short!("pay_claim"), payout_id),
            (record.recipient, record.asset, record.amount),
        );

        Ok(record.amount)
    }

    /// Permissionless, idempotent sweep of an expired unclaimed payout after `claim_deadline`.
    pub fn sweep_expired(env: Env, payout_id: u64) -> Result<i128, TreasuryError> {
        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
            .ok_or(TreasuryError::PayoutNotFound)?;

        match record.status {
            PayoutStatus::Swept => return Ok(0),
            PayoutStatus::Claimed => return Err(TreasuryError::AlreadyClaimed),
            PayoutStatus::Pending => {}
        }
        if env.ledger().timestamp() <= record.claim_deadline {
            return Err(TreasuryError::DeadlineNotReached);
        }

        record.status = PayoutStatus::Swept;
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

        let reserved = Self::reserved_balance(env.clone(), record.asset.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(record.asset.clone()),
            &(reserved - record.amount),
        );

        env.events().publish(
            (symbol_short!("pay_swept"), payout_id),
            (record.recipient, record.asset, record.amount),
        );

        Ok(record.amount)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this, and
    /// it cannot withdraw funds reserved for pending pull-based payouts.
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
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    #[test]
    fn withdraw_is_admin_gated_and_respects_payout_reservations() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.timestamp = 100);
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let pid = client.create_payout(&admin, &recipient, &token_id, &700, &500);
        assert_eq!(client.available_balance(&token_id), 300);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &300, &to);
        assert_eq!(token_client.balance(&to), 300);
        assert_eq!(client.available_balance(&token_id), 0);

        assert_eq!(client.claim(&recipient, &pid), 700);
        assert_eq!(token_client.balance(&recipient), 700);
    }
}
