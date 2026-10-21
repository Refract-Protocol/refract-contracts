//! # RefractTreasury
//!
//! Admin-gated custody vault for protocol funds (one token, USDC in
//! production). Funds come in through `deposit` and leave only through the
//! two withdrawal paths below, both rate-limited by a per-epoch spend limit.
//!
//! ## Roles (issue #199)
//!
//! | Role      | Who in production                 | Can do |
//! |-----------|-----------------------------------|--------|
//! | `admin`   | the `RefractTimelock` (governance) | approve withdrawal requests, change roles, change limits |
//! | `manager` | the operations multisig            | small direct withdrawals, propose larger ones, cancel its own requests |
//!
//! * **Small withdrawals** — `withdraw(to, amount)` by the manager, allowed
//!   only for `amount <= large_withdrawal_threshold`.
//! * **Large withdrawals** — the manager calls `propose_withdrawal(to, amount)`;
//!   the request records the recipient. The admin then calls
//!   `approve_withdrawal(id)`, which pays exactly the recorded recipient and
//!   amount. Nothing else can redirect the funds.
//! * Both paths count against the same per-epoch spend limit, so no single
//!   call (or burst of calls) can drain more than `epoch_limit` per epoch.
//!
//! ## Governance gating (issue #201)
//!
//! The admin is meant to be the timelock contract. The timelock calls the
//! treasury directly, so `admin.require_auth()` passes only for calls coming
//! out of `timelock.execute`, i.e. a passed governance proposal that waited
//! out the delay. That covers role changes, limit changes and large
//! withdrawals. `admin()` returns the configured timelock address.
//!
//! ## Epochs
//!
//! Epochs are fixed windows of `epoch_length` seconds anchored at the
//! initialization time: `[start + k * len, start + (k + 1) * len)`. The spend
//! counter resets when the ledger time enters a new window. Because the
//! windows are a fixed grid, no call can trigger an early reset.
//!
//! ## Lock-out protection
//!
//! * Admin changes are two-step (`propose_admin` → `accept_admin`), so the
//!   new admin must prove it exists and can authorize before it takes over.
//! * Neither role may be set to the vault itself, and admin and manager
//!   must be different addresses.
//!
//! The balance is always read from the token contract, never from a stored
//! counter, so it cannot drift from the funds actually held.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol,
};

const INSTANCE_TTL_THRESHOLD: u32 = 259_200;
const INSTANCE_TTL_EXTEND: u32 = 518_400;
const REQUEST_TTL_THRESHOLD: u32 = 518_400;
const REQUEST_TTL_EXTEND: u32 = 1_036_800;

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    Manager,
    Token,
    EpochLength,
    EpochLimit,
    EpochStart,
    EpochSpent,
    LargeThreshold,
    NextRequestId,
    Request(u64),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// Amount was zero or negative.
    InvalidAmount = 4,
    /// A configuration value was zero or negative.
    InvalidConfig = 5,
    /// The withdrawal would exceed the remaining spend for this epoch.
    ExceedsEpochLimit = 6,
    /// The vault does not hold enough funds.
    InsufficientFunds = 7,
    /// Above `large_withdrawal_threshold`: use `propose_withdrawal`.
    RequiresApproval = 8,
    RequestNotFound = 9,
    /// The request was already approved or cancelled.
    RequestNotPending = 10,
    /// The role change would lock the vault out (role set to the vault
    /// itself, or admin and manager collapsed into one address).
    InvalidRole = 11,
    NoPendingAdmin = 12,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RequestStatus {
    Pending = 0,
    Approved = 1,
    Cancelled = 2,
}

/// A manager-proposed withdrawal awaiting admin approval.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawalRequest {
    /// The only address the funds can be paid to.
    pub to: Address,
    pub amount: i128,
    pub created_at: u64,
    pub status: RequestStatus,
}

/// Read-only view of the current spend epoch.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochStatus {
    pub epoch_start: u64,
    pub epoch_end: u64,
    pub spent: i128,
    pub remaining: i128,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the vault.
    ///
    /// * `admin` — approver for large withdrawals and all config changes
    ///   (the timelock in production).
    /// * `manager` — operational spender.
    /// * `token` — the custody token (USDC).
    /// * `epoch_length` — seconds per spend epoch (> 0).
    /// * `epoch_limit` — max total withdrawn per epoch (> 0).
    /// * `large_withdrawal_threshold` — max amount the manager can withdraw
    ///   without admin approval (> 0).
    pub fn initialize(
        env: Env,
        admin: Address,
        manager: Address,
        token: Address,
        epoch_length: u64,
        epoch_limit: i128,
        large_withdrawal_threshold: i128,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        if epoch_length == 0 || epoch_limit <= 0 || large_withdrawal_threshold <= 0 {
            return Err(TreasuryError::InvalidConfig);
        }
        Self::check_roles(&env, &admin, &manager)?;

        let s = env.storage().instance();
        s.set(&DataKey::Admin, &admin);
        s.set(&DataKey::Manager, &manager);
        s.set(&DataKey::Token, &token);
        s.set(&DataKey::EpochLength, &epoch_length);
        s.set(&DataKey::EpochLimit, &epoch_limit);
        s.set(&DataKey::LargeThreshold, &large_withdrawal_threshold);
        s.set(&DataKey::EpochStart, &env.ledger().timestamp());
        s.set(&DataKey::EpochSpent, &0i128);
        s.set(&DataKey::NextRequestId, &0u64);
        Self::bump_instance(&env);

        env.events().publish(
            (Symbol::new(&env, "tr_init"),),
            (admin, manager, token, epoch_limit),
        );
        Ok(())
    }

    // ─── Deposits ────────────────────────────────────────────────────────

    /// Move `amount` of the custody token from `from` into the vault.
    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<(), TreasuryError> {
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let token = Self::token(env.clone())?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        env.events()
            .publish((Symbol::new(&env, "deposit"), from), (amount,));
        Ok(())
    }

    // ─── Withdrawals ─────────────────────────────────────────────────────

    /// Manager-only direct withdrawal of up to `large_withdrawal_threshold`.
    pub fn withdraw(env: Env, to: Address, amount: i128) -> Result<(), TreasuryError> {
        Self::manager(env.clone())?.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if amount > Self::large_withdrawal_threshold(env.clone())? {
            return Err(TreasuryError::RequiresApproval);
        }
        Self::pay(&env, &to, amount)?;
        env.events()
            .publish((Symbol::new(&env, "withdraw"), to), (amount,));
        Ok(())
    }

    /// Manager-only: record a withdrawal request for admin approval.
    /// Returns the request id.
    pub fn propose_withdrawal(env: Env, to: Address, amount: i128) -> Result<u64, TreasuryError> {
        Self::manager(env.clone())?.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextRequestId)
            .unwrap_or(0);
        let request = WithdrawalRequest {
            to: to.clone(),
            amount,
            created_at: env.ledger().timestamp(),
            status: RequestStatus::Pending,
        };
        Self::save_request(&env, id, &request);
        env.storage()
            .instance()
            .set(&DataKey::NextRequestId, &(id + 1));
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "wd_proposed"), id), (to, amount));
        Ok(id)
    }

    /// Admin-only: pay out a pending request to its recorded recipient.
    pub fn approve_withdrawal(env: Env, id: u64) -> Result<(), TreasuryError> {
        Self::admin(env.clone())?.require_auth();
        let mut request = Self::load_request(&env, id)?;
        if request.status != RequestStatus::Pending {
            return Err(TreasuryError::RequestNotPending);
        }
        request.status = RequestStatus::Approved;
        Self::save_request(&env, id, &request);
        Self::pay(&env, &request.to, request.amount)?;
        env.events().publish(
            (Symbol::new(&env, "wd_approved"), id),
            (request.to, request.amount),
        );
        Ok(())
    }

    /// Cancel a pending request. Callable by the manager or the admin.
    pub fn cancel_withdrawal(env: Env, caller: Address, id: u64) -> Result<(), TreasuryError> {
        caller.require_auth();
        if caller != Self::manager(env.clone())? && caller != Self::admin(env.clone())? {
            return Err(TreasuryError::Unauthorized);
        }
        let mut request = Self::load_request(&env, id)?;
        if request.status != RequestStatus::Pending {
            return Err(TreasuryError::RequestNotPending);
        }
        request.status = RequestStatus::Cancelled;
        Self::save_request(&env, id, &request);
        env.events()
            .publish((Symbol::new(&env, "wd_cancelled"), id), (caller,));
        Ok(())
    }

    // ─── Roles ───────────────────────────────────────────────────────────

    /// Admin-only: replace the manager.
    pub fn set_manager(env: Env, new_manager: Address) -> Result<(), TreasuryError> {
        let admin = Self::admin(env.clone())?;
        admin.require_auth();
        Self::check_roles(&env, &admin, &new_manager)?;
        env.storage()
            .instance()
            .set(&DataKey::Manager, &new_manager);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "manager_set"),), (new_manager,));
        Ok(())
    }

    /// Admin-only: nominate a new admin. Takes effect on `accept_admin`.
    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), TreasuryError> {
        Self::admin(env.clone())?.require_auth();
        Self::check_roles(&env, &new_admin, &Self::manager(env.clone())?)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "admin_proposed"),), (new_admin,));
        Ok(())
    }

    /// The nominated admin accepts the role (must authorize).
    pub fn accept_admin(env: Env) -> Result<(), TreasuryError> {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(TreasuryError::NoPendingAdmin)?;
        pending.require_auth();
        // The manager may have changed since the nomination.
        Self::check_roles(&env, &pending, &Self::manager(env.clone())?)?;
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "admin_set"),), (pending,));
        Ok(())
    }

    // ─── Limits ──────────────────────────────────────────────────────────

    /// Admin-only: change the per-epoch spend limit. Takes effect
    /// immediately; spend already recorded this epoch still counts.
    pub fn set_epoch_limit(env: Env, epoch_limit: i128) -> Result<(), TreasuryError> {
        Self::admin(env.clone())?.require_auth();
        if epoch_limit <= 0 {
            return Err(TreasuryError::InvalidConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::EpochLimit, &epoch_limit);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "limit_set"),), (epoch_limit,));
        Ok(())
    }

    /// Admin-only: change the manager's direct-withdrawal ceiling.
    pub fn set_large_withdrawal_threshold(env: Env, threshold: i128) -> Result<(), TreasuryError> {
        Self::admin(env.clone())?.require_auth();
        if threshold <= 0 {
            return Err(TreasuryError::InvalidConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::LargeThreshold, &threshold);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "threshold_set"),), (threshold,));
        Ok(())
    }

    // ─── Views ───────────────────────────────────────────────────────────

    /// Custody-token balance held by the vault (read from the token).
    pub fn balance(env: Env) -> Result<i128, TreasuryError> {
        let token = Self::token(env.clone())?;
        Ok(token::Client::new(&env, &token).balance(&env.current_contract_address()))
    }

    /// The admin (the configured timelock / governance address).
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::get(&env, &DataKey::Admin)
    }

    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    pub fn manager(env: Env) -> Result<Address, TreasuryError> {
        Self::get(&env, &DataKey::Manager)
    }

    pub fn token(env: Env) -> Result<Address, TreasuryError> {
        Self::get(&env, &DataKey::Token)
    }

    pub fn epoch_limit(env: Env) -> Result<i128, TreasuryError> {
        Self::get(&env, &DataKey::EpochLimit)
    }

    pub fn epoch_length(env: Env) -> Result<u64, TreasuryError> {
        Self::get(&env, &DataKey::EpochLength)
    }

    pub fn large_withdrawal_threshold(env: Env) -> Result<i128, TreasuryError> {
        Self::get(&env, &DataKey::LargeThreshold)
    }

    /// Current epoch window and how much can still be spent in it.
    pub fn epoch_status(env: Env) -> Result<EpochStatus, TreasuryError> {
        let (start, spent) = Self::current_epoch(&env)?;
        let len = Self::epoch_length(env.clone())?;
        let limit = Self::epoch_limit(env.clone())?;
        Ok(EpochStatus {
            epoch_start: start,
            epoch_end: start.saturating_add(len),
            spent,
            remaining: (limit - spent).max(0),
        })
    }

    pub fn get_request(env: Env, id: u64) -> Option<WithdrawalRequest> {
        env.storage().persistent().get(&DataKey::Request(id))
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

impl RefractTreasury {
    fn get<V: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(
        env: &Env,
        key: &DataKey,
    ) -> Result<V, TreasuryError> {
        env.storage()
            .instance()
            .get(key)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn check_roles(env: &Env, admin: &Address, manager: &Address) -> Result<(), TreasuryError> {
        let me = env.current_contract_address();
        if admin == &me || manager == &me || admin == manager {
            return Err(TreasuryError::InvalidRole);
        }
        Ok(())
    }

    /// The current grid-aligned epoch start and the amount spent in it.
    fn current_epoch(env: &Env) -> Result<(u64, i128), TreasuryError> {
        let stored_start: u64 = Self::get(env, &DataKey::EpochStart)?;
        let len: u64 = Self::get(env, &DataKey::EpochLength)?;
        let now = env.ledger().timestamp();
        let elapsed_epochs = now.saturating_sub(stored_start) / len;
        if elapsed_epochs == 0 {
            let spent: i128 = Self::get(env, &DataKey::EpochSpent)?;
            Ok((stored_start, spent))
        } else {
            let start = stored_start.saturating_add(elapsed_epochs.saturating_mul(len));
            Ok((start, 0))
        }
    }

    /// Enforce the epoch limit and the vault balance, record the spend and
    /// transfer.
    fn pay(env: &Env, to: &Address, amount: i128) -> Result<(), TreasuryError> {
        let (start, spent) = Self::current_epoch(env)?;
        let limit: i128 = Self::get(env, &DataKey::EpochLimit)?;
        let new_spent = spent
            .checked_add(amount)
            .ok_or(TreasuryError::ExceedsEpochLimit)?;
        if new_spent > limit {
            return Err(TreasuryError::ExceedsEpochLimit);
        }
        let token_addr: Address = Self::get(env, &DataKey::Token)?;
        let token = token::Client::new(env, &token_addr);
        let me = env.current_contract_address();
        if token.balance(&me) < amount {
            return Err(TreasuryError::InsufficientFunds);
        }

        env.storage().instance().set(&DataKey::EpochStart, &start);
        env.storage()
            .instance()
            .set(&DataKey::EpochSpent, &new_spent);
        Self::bump_instance(env);

        token.transfer(&me, to, &amount);
        Ok(())
    }

    fn load_request(env: &Env, id: u64) -> Result<WithdrawalRequest, TreasuryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Request(id))
            .ok_or(TreasuryError::RequestNotFound)
    }

    fn save_request(env: &Env, id: u64, request: &WithdrawalRequest) {
        let key = DataKey::Request(id);
        env.storage().persistent().set(&key, request);
        env.storage()
            .persistent()
            .extend_ttl(&key, REQUEST_TTL_THRESHOLD, REQUEST_TTL_EXTEND);
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND);
    }
}

#[cfg(test)]
mod test;
