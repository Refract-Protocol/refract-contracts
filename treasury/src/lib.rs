//! # RefractTreasury
//!
//! Role-separated protocol custody vault for Soroban (`#![no_std]`).
//!
//! ## Authorization & Spending Model
//! - **Deposits**: Anyone may deposit the configured SEP-41 token via [`RefractTreasury::deposit`].
//! - **Role Separation**:
//!   - `manager` can propose withdrawals via [`RefractTreasury::propose_withdrawal`] or execute direct withdrawals within the active epoch budget via [`RefractTreasury::withdraw`].
//!   - `admin` (governance / timelock / council) authorizes proposed withdrawals via [`RefractTreasury::approve_withdrawal`], executes direct withdrawals via [`RefractTreasury::withdraw`], and configures roles (`set_manager`, `set_admin`) and per-epoch spending limits (`set_epoch_limit`).
//! - **Epoch Rate Limiting**: Every withdrawal (whether direct or via proposal approval) enforces `epoch_spent + amount <= epoch_limit`. Once `now >= epoch_start + epoch_duration`, the epoch window rolls over deterministically (`epoch_start = now`, `epoch_spent = 0`).
//! - **Drift-Free Accounting**: [`RefractTreasury::balance`] queries the underlying SEP-41 token contract directly so vault balance never drifts from actual on-chain token custody.

#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

/// Minimum ledger TTL threshold before extension (~7 days at 5s/ledger).
const TTL_THRESHOLD: u32 = 120_960;
/// Target ledger TTL extension (~30 days at 5s/ledger).
const TTL_BUMP: u32 = 518_400;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientBalance = 5,
    EpochLimitExceeded = 6,
    InvalidEpochConfig = 7,
    ProposalNotFound = 8,
    ProposalAlreadyExecuted = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawalProposal {
    pub id: u64,
    pub proposer: Address,
    pub to: Address,
    pub amount: i128,
    pub executed: bool,
    pub created_at: u64,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Manager,
    Token,
    EpochLimit,
    EpochDuration,
    EpochStart,
    EpochSpent,
    NextProposalId,
    Proposal(u64),
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury vault with separated `admin` and `manager` roles,
    /// the custody `token`, and per-epoch spending limit parameters.
    pub fn initialize(
        env: Env,
        admin: Address,
        manager: Address,
        token: Address,
        epoch_limit: i128,
        epoch_duration: u64,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        if epoch_limit <= 0 || epoch_duration == 0 {
            return Err(TreasuryError::InvalidEpochConfig);
        }
        admin.require_auth();

        let now = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Manager, &manager);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::EpochLimit, &epoch_limit);
        env.storage()
            .instance()
            .set(&DataKey::EpochDuration, &epoch_duration);
        env.storage().instance().set(&DataKey::EpochStart, &now);
        env.storage().instance().set(&DataKey::EpochSpent, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &0u64);
        Self::bump_instance(&env);
        Ok(())
    }

    /// Deposit `amount` (in 1e7 fixed-point) of the configured token into the
    /// treasury vault. Returns the updated vault balance.
    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<i128, TreasuryError> {
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        let client = token::Client::new(&env, &token);
        let vault = env.current_contract_address();
        client.transfer(&from, &vault, &amount);

        Self::bump_instance(&env);
        env.events()
            .publish((symbol_short!("tr_dep"), from), amount);

        Ok(client.balance(&vault))
    }

    /// Propose a withdrawal of `amount` to `to`. Callable by `manager` or `admin`.
    ///
    /// The recorded `to` recipient is immutable on the proposal so approval can
    /// never redirect funds to a third party.
    pub fn propose_withdrawal(
        env: Env,
        caller: Address,
        to: Address,
        amount: i128,
    ) -> Result<u64, TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        let manager = Self::read_manager(&env)?;
        if caller != manager && caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProposalId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &(id + 1));

        let proposal = WithdrawalProposal {
            id,
            proposer: caller.clone(),
            to: to.clone(),
            amount,
            executed: false,
            created_at: env.ledger().timestamp(),
        };
        let key = DataKey::Proposal(id);
        env.storage().persistent().set(&key, &proposal);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
        Self::bump_instance(&env);

        env.events()
            .publish((symbol_short!("tr_prop"), id), (caller, to, amount));

        Ok(id)
    }

    /// Approve and execute a pending withdrawal proposal. Callable strictly by
    /// `admin`, enforcing the per-epoch spending limit and transferring funds
    /// exclusively to the proposal's recorded `to` address.
    pub fn approve_withdrawal(
        env: Env,
        caller: Address,
        proposal_id: u64,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }

        let key = DataKey::Proposal(proposal_id);
        let mut proposal: WithdrawalProposal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(TreasuryError::ProposalNotFound)?;
        if proposal.executed {
            return Err(TreasuryError::ProposalAlreadyExecuted);
        }

        let new_balance = Self::execute_outflow(&env, &caller, &proposal.to, proposal.amount)?;

        proposal.executed = true;
        env.storage().persistent().set(&key, &proposal);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);

        Ok(new_balance)
    }

    /// Withdraw `amount` to `to`. Gated by the authorized spender roles (`manager`
    /// or `admin`) and subject to the per-epoch spending limit.
    pub fn withdraw(
        env: Env,
        caller: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        let manager = Self::read_manager(&env)?;
        if caller != admin && caller != manager {
            return Err(TreasuryError::Unauthorized);
        }
        Self::execute_outflow(&env, &caller, &to, amount)
    }

    /// Reassign the `manager` spender role. Admin-only.
    pub fn set_manager(
        env: Env,
        caller: Address,
        new_manager: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Manager, &new_manager);
        Self::bump_instance(&env);
        env.events().publish(
            (symbol_short!("tr_role"), symbol_short!("manager")),
            new_manager,
        );
        Ok(())
    }

    /// Reassign the `admin` role. Admin-only; requires both the current `caller`
    /// and `new_admin` to authorize the handover so the vault cannot be locked out
    /// by setting a non-existent address.
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        new_admin.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        Self::bump_instance(&env);
        env.events().publish(
            (symbol_short!("tr_role"), symbol_short!("admin")),
            new_admin,
        );
        Ok(())
    }

    /// Update the per-epoch spending limit and epoch duration. Admin-only.
    pub fn set_epoch_limit(
        env: Env,
        caller: Address,
        new_limit: i128,
        new_duration: u64,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if new_limit <= 0 || new_duration == 0 {
            return Err(TreasuryError::InvalidEpochConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::EpochLimit, &new_limit);
        env.storage()
            .instance()
            .set(&DataKey::EpochDuration, &new_duration);
        Self::bump_instance(&env);
        env.events().publish(
            (symbol_short!("tr_limit"), caller),
            (new_limit, new_duration),
        );
        Ok(())
    }

    /// Return the actual SEP-41 token balance held by the treasury vault.
    pub fn balance(env: Env) -> Result<i128, TreasuryError> {
        let token = Self::read_token(&env)?;
        Ok(token::Client::new(&env, &token).balance(&env.current_contract_address()))
    }

    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    pub fn manager(env: Env) -> Result<Address, TreasuryError> {
        Self::read_manager(&env)
    }

    pub fn epoch_limit(env: Env) -> Result<i128, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::EpochLimit)
            .ok_or(TreasuryError::NotInitialized)
    }

    pub fn epoch_spent(env: Env) -> Result<i128, TreasuryError> {
        let _ = Self::read_admin(&env)?;
        let now = env.ledger().timestamp();
        let epoch_start: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EpochStart)
            .unwrap_or(0);
        let epoch_duration: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EpochDuration)
            .unwrap_or(1);
        if now >= epoch_start.saturating_add(epoch_duration) {
            Ok(0)
        } else {
            Ok(env
                .storage()
                .instance()
                .get(&DataKey::EpochSpent)
                .unwrap_or(0))
        }
    }

    pub fn get_proposal(env: Env, proposal_id: u64) -> Result<WithdrawalProposal, TreasuryError> {
        let key = DataKey::Proposal(proposal_id);
        let prop: WithdrawalProposal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(TreasuryError::ProposalNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
        Ok(prop)
    }

    fn execute_outflow(
        env: &Env,
        caller: &Address,
        to: &Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let token = Self::read_token(env)?;
        let client = token::Client::new(env, &token);
        let vault = env.current_contract_address();
        let current_balance = client.balance(&vault);
        if amount > current_balance {
            return Err(TreasuryError::InsufficientBalance);
        }

        let now = env.ledger().timestamp();
        let mut epoch_start: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EpochStart)
            .unwrap_or(0);
        let epoch_duration: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EpochDuration)
            .unwrap_or(1);
        let epoch_limit: i128 = env
            .storage()
            .instance()
            .get(&DataKey::EpochLimit)
            .unwrap_or(0);
        let mut epoch_spent: i128 = env
            .storage()
            .instance()
            .get(&DataKey::EpochSpent)
            .unwrap_or(0);

        // Roll over epoch window if current epoch has elapsed.
        if now >= epoch_start.saturating_add(epoch_duration) {
            let elapsed = now - epoch_start;
            let epochs_passed = elapsed / epoch_duration;
            epoch_start = epoch_start.saturating_add(epochs_passed.saturating_mul(epoch_duration));
            epoch_spent = 0;
        }

        let new_spent = epoch_spent
            .checked_add(amount)
            .ok_or(TreasuryError::EpochLimitExceeded)?;
        if new_spent > epoch_limit {
            return Err(TreasuryError::EpochLimitExceeded);
        }

        env.storage()
            .instance()
            .set(&DataKey::EpochStart, &epoch_start);
        env.storage()
            .instance()
            .set(&DataKey::EpochSpent, &new_spent);
        Self::bump_instance(env);

        client.transfer(&vault, to, &amount);

        env.events().publish(
            (symbol_short!("tr_wdrw"), to.clone()),
            (caller.clone(), amount),
        );

        Ok(current_balance - amount)
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_manager(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Manager)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn bump_instance(env: &Env) {
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_BUMP);
    }
}

#[cfg(test)]
mod test;
