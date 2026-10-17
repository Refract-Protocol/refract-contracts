//! # RefractTreasury
//!
//! Protocol fee and reserve vault with a stakeholder challenge period for
//! high-value withdrawals, weighted vetoken/backstop challenges, anti-splitting
//! window cumulative checks, and timelock-delayed execution.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    InvalidCap = 5,
    CapExceeded = 6,
    WithdrawalNotFound = 7,
    InvalidWithdrawalState = 8,
    ChallengeWindowClosed = 9,
    ChallengeWindowStillOpen = 10,
    AlreadyChallenged = 11,
    ZeroChallengeWeight = 12,
    ContractPaused = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    Paused,
    Balance,
    OutflowWindow,
    ChallengeConfig,
    StakeholderWeight(Address),
    NextWithdrawalId,
    Withdrawal(u64),
    HasChallenged(u64, Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub token: Address,
    pub max_single_withdrawal: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChallengeConfig {
    pub threshold: i128,
    pub window_ledgers: u32,
    pub timelock_delay_ledgers: u32,
    pub quorum_weight: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutflowWindow {
    pub window_start_ledger: u32,
    pub cumulative_fast_outflow: i128,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum WithdrawalStatus {
    Queued = 0,
    Challenged = 1,
    Executed = 2,
    Cancelled = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedWithdrawal {
    pub id: u64,
    pub recipient: Address,
    pub amount: i128,
    pub reason: Symbol,
    pub challenge_window_ends: u32,
    pub executable_at: u32,
    pub challenge_weight: i128,
    pub status: WithdrawalStatus,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
        max_single_withdrawal: i128,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        governance.require_auth();
        if max_single_withdrawal <= 0 {
            return Err(TreasuryError::InvalidCap);
        }
        let cfg = Config {
            governance,
            token,
            max_single_withdrawal,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::NextWithdrawalId, &0u64);
        env.storage().instance().set(
            &DataKey::OutflowWindow,
            &OutflowWindow {
                window_start_ledger: env.ledger().sequence(),
                cumulative_fast_outflow: 0,
            },
        );
        env.storage().instance().set(
            &DataKey::ChallengeConfig,
            &ChallengeConfig {
                threshold: max_single_withdrawal / 2,
                window_ledgers: 20,
                timelock_delay_ledgers: 10,
                quorum_weight: 1_000,
            },
        );
        Ok(())
    }

    pub fn set_challenge_config(
        env: Env,
        threshold: i128,
        window_ledgers: u32,
        timelock_delay_ledgers: u32,
        quorum_weight: i128,
    ) -> Result<ChallengeConfig, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if threshold <= 0 || window_ledgers == 0 || quorum_weight <= 0 {
            return Err(TreasuryError::InvalidCap);
        }
        let ccfg = ChallengeConfig {
            threshold,
            window_ledgers,
            timelock_delay_ledgers,
            quorum_weight,
        };
        env.storage()
            .instance()
            .set(&DataKey::ChallengeConfig, &ccfg);
        Ok(ccfg)
    }

    pub fn set_paused(env: Env, paused: bool) -> Result<(), TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        Ok(())
    }

    /// Registers or snapshots a stakeholder's combined vetoken + backstop voting power.
    pub fn set_stakeholder_weight(
        env: Env,
        stakeholder: Address,
        weight: i128,
    ) -> Result<(), TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if weight < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::StakeholderWeight(stakeholder), &weight);
        Ok(())
    }

    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<i128, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        token::Client::new(&env, &cfg.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let next = Self::read_balance(&env)? + amount;
        env.storage().instance().set(&DataKey::Balance, &next);
        Ok(next)
    }

    /// Submits a governance-approved withdrawal. Below-threshold operational withdrawals
    /// execute immediately unless splitting within the same window would exceed the threshold;
    /// high-value withdrawals enter the challenge queue with `challenge_window_ends`.
    pub fn withdraw(
        env: Env,
        recipient: Address,
        amount: i128,
        reason: Symbol,
    ) -> Result<Option<u64>, TreasuryError> {
        Self::require_not_paused(&env)?;
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if amount > cfg.max_single_withdrawal {
            return Err(TreasuryError::CapExceeded);
        }
        let bal = Self::read_balance(&env)?;
        if amount > bal {
            return Err(TreasuryError::InsufficientBalance);
        }

        let ccfg: ChallengeConfig = env
            .storage()
            .instance()
            .get(&DataKey::ChallengeConfig)
            .ok_or(TreasuryError::NotInitialized)?;
        let now = env.ledger().sequence();

        let mut win: OutflowWindow = env
            .storage()
            .instance()
            .get(&DataKey::OutflowWindow)
            .unwrap_or(OutflowWindow {
                window_start_ledger: now,
                cumulative_fast_outflow: 0,
            });
        if now >= win.window_start_ledger.saturating_add(ccfg.window_ledgers) {
            win.window_start_ledger = now;
            win.cumulative_fast_outflow = 0;
        }

        if amount < ccfg.threshold && win.cumulative_fast_outflow + amount < ccfg.threshold {
            win.cumulative_fast_outflow += amount;
            env.storage().instance().set(&DataKey::OutflowWindow, &win);
            env.storage()
                .instance()
                .set(&DataKey::Balance, &(bal - amount));
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &recipient,
                &amount,
            );
            env.events().publish(
                (symbol_short!("treasury"), symbol_short!("withdraw")),
                (recipient, amount, reason),
            );
            return Ok(None);
        }

        // High-value or split-threshold withdrawal -> escrow from liquid balance and queue
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(bal - amount));

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextWithdrawalId)
            .unwrap_or(0);
        let challenge_window_ends = now.saturating_add(ccfg.window_ledgers);
        let executable_at = challenge_window_ends.saturating_add(ccfg.timelock_delay_ledgers);
        let item = QueuedWithdrawal {
            id,
            recipient: recipient.clone(),
            amount,
            reason: reason.clone(),
            challenge_window_ends,
            executable_at,
            challenge_weight: 0,
            status: WithdrawalStatus::Queued,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(id), &item);
        env.storage()
            .instance()
            .set(&DataKey::NextWithdrawalId, &(id + 1));

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("w_queued")),
            (id, recipient, amount, challenge_window_ends, executable_at),
        );
        Ok(Some(id))
    }

    /// Raises a weighted stakeholder challenge during the active challenge window.
    pub fn challenge(
        env: Env,
        challenger: Address,
        withdrawal_id: u64,
    ) -> Result<QueuedWithdrawal, TreasuryError> {
        let _cfg = Self::read_config(&env)?;
        challenger.require_auth();

        let weight: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakeholderWeight(challenger.clone()))
            .unwrap_or(0);
        if weight <= 0 {
            return Err(TreasuryError::ZeroChallengeWeight);
        }

        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::HasChallenged(withdrawal_id, challenger.clone()))
            .unwrap_or(false)
        {
            return Err(TreasuryError::AlreadyChallenged);
        }

        let mut item: QueuedWithdrawal = env
            .storage()
            .persistent()
            .get(&DataKey::Withdrawal(withdrawal_id))
            .ok_or(TreasuryError::WithdrawalNotFound)?;

        if item.status != WithdrawalStatus::Queued && item.status != WithdrawalStatus::Challenged {
            return Err(TreasuryError::InvalidWithdrawalState);
        }
        let now = env.ledger().sequence();
        if now > item.challenge_window_ends {
            return Err(TreasuryError::ChallengeWindowClosed);
        }

        env.storage().persistent().set(
            &DataKey::HasChallenged(withdrawal_id, challenger.clone()),
            &true,
        );
        item.challenge_weight += weight;

        let ccfg: ChallengeConfig = env
            .storage()
            .instance()
            .get(&DataKey::ChallengeConfig)
            .ok_or(TreasuryError::NotInitialized)?;
        if item.challenge_weight >= ccfg.quorum_weight {
            item.status = WithdrawalStatus::Challenged;
        }
        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(withdrawal_id), &item);

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("w_chal")),
            (withdrawal_id, challenger, weight, item.challenge_weight),
        );
        Ok(item)
    }

    /// Executes an unchallenged queued withdrawal once the challenge window and timelock delay have elapsed.
    pub fn execute_withdrawal(
        env: Env,
        withdrawal_id: u64,
    ) -> Result<QueuedWithdrawal, TreasuryError> {
        Self::require_not_paused(&env)?;
        let cfg = Self::read_config(&env)?;
        let mut item: QueuedWithdrawal = env
            .storage()
            .persistent()
            .get(&DataKey::Withdrawal(withdrawal_id))
            .ok_or(TreasuryError::WithdrawalNotFound)?;

        if item.status != WithdrawalStatus::Queued {
            return Err(TreasuryError::InvalidWithdrawalState);
        }
        let now = env.ledger().sequence();
        if now <= item.challenge_window_ends || now < item.executable_at {
            return Err(TreasuryError::ChallengeWindowStillOpen);
        }

        item.status = WithdrawalStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(withdrawal_id), &item);

        token::Client::new(&env, &cfg.token).transfer(
            &env.current_contract_address(),
            &item.recipient,
            &item.amount,
        );
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("w_exec")),
            (withdrawal_id, item.recipient.clone(), item.amount),
        );
        Ok(item)
    }

    /// Cancels a queued or challenged withdrawal and returns its reserved balance to the treasury.
    pub fn cancel_withdrawal(
        env: Env,
        withdrawal_id: u64,
    ) -> Result<QueuedWithdrawal, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut item: QueuedWithdrawal = env
            .storage()
            .persistent()
            .get(&DataKey::Withdrawal(withdrawal_id))
            .ok_or(TreasuryError::WithdrawalNotFound)?;

        if item.status != WithdrawalStatus::Queued && item.status != WithdrawalStatus::Challenged {
            return Err(TreasuryError::InvalidWithdrawalState);
        }
        item.status = WithdrawalStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Withdrawal(withdrawal_id), &item);

        let bal = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(bal + item.amount));
        Ok(item)
    }

    pub fn balance(env: Env) -> i128 {
        Self::read_balance(&env).unwrap_or(0)
    }

    pub fn get_withdrawal(env: Env, withdrawal_id: u64) -> Option<QueuedWithdrawal> {
        env.storage()
            .persistent()
            .get(&DataKey::Withdrawal(withdrawal_id))
    }

    fn require_not_paused(env: &Env) -> Result<(), TreasuryError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(TreasuryError::ContractPaused);
        }
        Ok(())
    }

    fn read_config(env: &Env) -> Result<Config, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, TreasuryError> {
        let _ = Self::read_config(env)?;
        Ok(env.storage().instance().get(&DataKey::Balance).unwrap_or(0))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    #[test]
    fn high_value_withdrawals_queue_for_challenge_and_enforce_quorum_and_delay() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let recipient = Address::generate(&env);
        let staker1 = Address::generate(&env);
        let staker2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&gov, &20_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &cid);

        client.initialize(&gov, &token_id, &5_000);
        client.set_challenge_config(&2_000, &10, &5, &1_000);
        client.deposit(&gov, &15_000);
        client.set_stakeholder_weight(&staker1, &600);
        client.set_stakeholder_weight(&staker2, &500);

        // Below-threshold withdrawal (1_200 < 2_000) executes on fast path
        assert_eq!(
            client.withdraw(&recipient, &1_200, &symbol_short!("ops")),
            None
        );
        assert_eq!(tok.balance(&recipient), 1_200);

        // Second sub-threshold withdrawal (1_000) in same window crosses 2_000 cumulative -> queued!
        let w0 = client
            .withdraw(&recipient, &1_000, &symbol_short!("split"))
            .unwrap();

        // High-value withdrawal (3_000 >= 2_000) -> queued as w1
        let w1 = client
            .withdraw(&recipient, &3_000, &symbol_short!("large"))
            .unwrap();

        // Challenge w1 with staker1 (600) and staker2 (500) -> crosses 1_000 quorum -> Challenged
        client.challenge(&staker1, &w1);
        assert_eq!(
            client.try_challenge(&staker1, &w1),
            Err(Ok(TreasuryError::AlreadyChallenged))
        );
        let challenged = client.challenge(&staker2, &w1);
        assert_eq!(challenged.status, WithdrawalStatus::Challenged);

        // Advance past window (10) + timelock delay (5) = ledger 16
        env.ledger().with_mut(|l| l.sequence_number = 16);

        // Challenged w1 cannot execute; unchallenged w0 executes once only
        assert_eq!(
            client.try_execute_withdrawal(&w1),
            Err(Ok(TreasuryError::InvalidWithdrawalState))
        );
        client.execute_withdrawal(&w0);
        assert_eq!(tok.balance(&recipient), 2_200);
        assert_eq!(
            client.try_execute_withdrawal(&w0),
            Err(Ok(TreasuryError::InvalidWithdrawalState))
        );

        // Cancel challenged w1 and verify balance is restored
        client.cancel_withdrawal(&w1);
        assert_eq!(client.balance(), 15_000 - 2_200);
    }
}
