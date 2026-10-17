#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

pub const REASON_CIRCUIT_BREAKER: u32 = 999;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BountyAction {
    Fund,
    Award,
    ReclaimExpired,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused: bool,
    pub reason_code: u32,
    pub circuit_breaker_threshold: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyEscrowItem {
    pub id: u64,
    pub funder: Address,
    pub amount: i128,
    pub expiry: u64,
    pub reclaimed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Pause,
    NextBountyId,
    Escrow(u64),
    Award(BytesN<32>),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    Paused = 7,
    BountyNotFound = 8,
    NotExpired = 9,
    AlreadyReclaimed = 10,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::NextBountyId, &1u64);
        env.storage().instance().set(
            &DataKey::Pause,
            &PauseState {
                paused: false,
                reason_code: 0,
                circuit_breaker_threshold: 0,
            },
        );
        Ok(())
    }

    pub fn pause(env: Env, caller: Address, reason_code: u32) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.paused = true;
        ps.reason_code = reason_code;
        env.storage().instance().set(&DataKey::Pause, &ps);
        env.events().publish(
            (
                symbol_short!("refract"),
                symbol_short!("bounty"),
                symbol_short!("pause"),
            ),
            (caller, reason_code),
        );
        Ok(())
    }

    pub fn unpause(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.paused = false;
        ps.reason_code = 0;
        env.storage().instance().set(&DataKey::Pause, &ps);
        env.events().publish(
            (
                symbol_short!("refract"),
                symbol_short!("bounty"),
                symbol_short!("unpause"),
            ),
            caller,
        );
        Ok(())
    }

    pub fn set_circuit_breaker_threshold(
        env: Env,
        caller: Address,
        threshold: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        if threshold < 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut ps = Self::read_pause(&env);
        ps.circuit_breaker_threshold = threshold;
        env.storage().instance().set(&DataKey::Pause, &ps);
        Ok(())
    }

    pub fn pause_state(env: Env) -> PauseState {
        Self::read_pause(&env)
    }

    pub fn fund(env: Env, from: Address, amount: i128, expiry: u64) -> Result<u64, BountyError> {
        from.require_auth();
        Self::guard_not_paused(&env, BountyAction::Fund)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextBountyId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextBountyId, &(id + 1));
        env.storage().persistent().set(
            &DataKey::Escrow(id),
            &BountyEscrowItem {
                id,
                funder: from,
                amount,
                expiry,
                reclaimed: false,
            },
        );
        Ok(id)
    }

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::guard_not_paused(&env, BountyAction::Award)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        if Self::check_circuit_breaker(&env, &caller, amount) {
            return Ok(());
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Exempt exit path: `reclaim_expired` remains allowed even when the contract is paused.
    pub fn reclaim_expired(env: Env, bounty_id: u64) -> Result<i128, BountyError> {
        Self::guard_not_paused(&env, BountyAction::ReclaimExpired)?;
        let mut item: BountyEscrowItem = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(bounty_id))
            .ok_or(BountyError::BountyNotFound)?;
        if item.reclaimed {
            return Err(BountyError::AlreadyReclaimed);
        }
        if env.ledger().timestamp() < item.expiry {
            return Err(BountyError::NotExpired);
        }
        let balance = Self::read_balance(&env)?;
        if item.amount > balance {
            return Err(BountyError::InsufficientBalance);
        }
        item.reclaimed = true;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(bounty_id), &item);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - item.amount));

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &item.funder,
            &item.amount,
        );
        Ok(item.amount)
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    fn guard_not_paused(env: &Env, action: BountyAction) -> Result<(), BountyError> {
        let ps = Self::read_pause(env);
        if !ps.paused {
            return Ok(());
        }
        match action {
            BountyAction::ReclaimExpired => Ok(()),
            _ => Err(BountyError::Paused),
        }
    }

    fn check_circuit_breaker(env: &Env, actor: &Address, outflow: i128) -> bool {
        let mut ps = Self::read_pause(env);
        if ps.circuit_breaker_threshold > 0 && outflow > ps.circuit_breaker_threshold {
            ps.paused = true;
            ps.reason_code = REASON_CIRCUIT_BREAKER;
            env.storage().instance().set(&DataKey::Pause, &ps);
            env.events().publish(
                (
                    symbol_short!("refract"),
                    symbol_short!("bounty"),
                    symbol_short!("pause"),
                ),
                (actor.clone(), REASON_CIRCUIT_BREAKER),
            );
            return true;
        }
        false
    }

    fn read_pause(env: &Env) -> PauseState {
        env.storage()
            .instance()
            .get(&DataKey::Pause)
            .unwrap_or(PauseState {
                paused: false,
                reason_code: 0,
                circuit_breaker_threshold: 0,
            })
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};

    #[test]
    fn bounty_pause_allows_reclaim_expired_and_trips_circuit_breaker() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
            li.min_temp_entry_ttl = 100_000;
            li.min_persistent_entry_ttl = 100_000;
            li.max_entry_ttl = 1_000_000;
        });
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &1_000_0000000);

        client.initialize(&gov, &token_id);
        let bounty_id = client.fund(&funder, &400_0000000, &1_700_001_000);

        client.pause(&gov, &7);
        assert_eq!(
            client.try_fund(&funder, &100_0000000, &1_700_002_000),
            Err(Ok(BountyError::Paused))
        );
        assert_eq!(
            client.try_award(
                &gov,
                &researcher,
                &100_0000000,
                &BytesN::from_array(&env, &[0x11; 32])
            ),
            Err(Ok(BountyError::Paused))
        );

        // Advance past expiry -> reclaim_expired is allowed even while paused!
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_001_000;
        });
        assert_eq!(client.reclaim_expired(&bounty_id), 400_0000000);

        // Unpause, fund again, and trip the circuit breaker on award above threshold.
        client.unpause(&gov);
        client.fund(&funder, &500_0000000, &1_700_009_000);
        client.set_circuit_breaker_threshold(&gov, &250_0000000);
        client.award(
            &gov,
            &researcher,
            &300_0000000,
            &BytesN::from_array(&env, &[0x22; 32]),
        );
        assert!(client.pause_state().paused);
        assert_eq!(client.pause_state().reason_code, REASON_CIRCUIT_BREAKER);
    }
}
