#![no_std]
// Native builds with soroban-sdk's `testutils` (pulled in by tests) contain
// SDK-generated client code that unwraps, which trips clippy.toml's
// panic-free rule. The rule is still enforced on the shipped contract by
// `cargo clippy --target wasm32-unknown-unknown` and by
// scripts/check_soroban_footguns.py in CI.
#![cfg_attr(not(target_family = "wasm"), allow(clippy::disallowed_methods))]

//! # RefractTreasury
//!
//! Token-custody vault with source-tagged inflow accounting.
//!
//! Every inflow arrives through [`RefractTreasury::deposit`] and is tagged
//! with a [`FundingSource`]. The vault keeps, per asset:
//!
//! * `total_by_source(asset, source)` – cumulative inflows from one source.
//!   Spends never decrease it, so inflow reporting stays truthful.
//! * `total_deposits(asset)` – cumulative inflows from all sources.
//! * `total_withdrawn(asset)` – cumulative outflows through `withdraw`.
//!
//! The recorded net position is `total_deposits - total_withdrawn`.
//! [`RefractTreasury::unaccounted_balance`] compares it with the real token
//! balance. Tokens sent straight to the contract address (bypassing
//! `deposit`) are never credited to any source; they show up there instead.
//!
//! Totals live in persistent storage, one entry per `(asset, source)` pair,
//! so a busy source cannot bloat the instance entry.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

/// Persistent entries are extended to roughly 30 days whenever they are
/// written and fall below roughly 15 days of remaining TTL.
const PERSISTENT_TTL_THRESHOLD: u32 = 259_200;
const PERSISTENT_TTL_EXTEND_TO: u32 = 518_400;

/// Where an inflow came from.
///
/// A closed set encoded on the wire as its `u32` discriminant. An argument
/// carrying any other value fails to decode, so the call is rejected before
/// the contract runs.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum FundingSource {
    /// Fees routed from the protocol (pool premiums, fee switch).
    ProtocolFees = 0,
    /// Money earmarked for the grants programme.
    GrantFunding = 1,
    /// Money earmarked for bounty escrow top-ups.
    BountyTopUp = 2,
    /// Donations from outside the protocol.
    ExternalDonation = 3,
}

impl FundingSource {
    /// Every source, in discriminant order.
    pub const ALL: [FundingSource; 4] = [
        FundingSource::ProtocolFees,
        FundingSource::GrantFunding,
        FundingSource::BountyTopUp,
        FundingSource::ExternalDonation,
    ];

    /// Decodes a raw discriminant, rejecting unknown values.
    pub fn from_u32(value: u32) -> Result<Self, TreasuryError> {
        Self::ALL
            .get(value as usize)
            .copied()
            .ok_or(TreasuryError::UnknownFundingSource)
    }
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Instance: admin authorised to withdraw, pause and set caps.
    Admin,
    /// Instance: whether deposits are currently rejected.
    Paused,
    /// Persistent: cumulative deposits of `asset` from `source`.
    SourceTotal(Address, FundingSource),
    /// Persistent: optional cap on `SourceTotal` (absent or 0 = no cap).
    SourceCap(Address, FundingSource),
    /// Persistent: cumulative deposits of `asset` from all sources.
    TotalDeposits(Address),
    /// Persistent: cumulative withdrawals of `asset`.
    TotalWithdrawn(Address),
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
    /// Deposits are paused.
    Paused = 5,
    /// The deposit would push the source total above its cap.
    SourceCapExceeded = 6,
    /// A raw source discriminant did not match any [`FundingSource`].
    UnknownFundingSource = 7,
    /// Checked arithmetic overflowed.
    Overflow = 8,
    /// A cap value was negative.
    InvalidCap = 9,
}

/// Payload of the `("deposit", depositor)` event.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositEvent {
    pub asset: Address,
    pub source: FundingSource,
    pub amount: i128,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::load_admin(&env)
    }

    /// Deposit `amount` of `asset` from `depositor`, tagged with `source`.
    ///
    /// Transfers the tokens in, then adds `amount` to
    /// `total_by_source(asset, source)` and `total_deposits(asset)`. Host
    /// atomicity means a failed transfer leaves no totals behind. Emits
    /// `("deposit", depositor)` with a [`DepositEvent`].
    pub fn deposit(
        env: Env,
        depositor: Address,
        asset: Address,
        amount: i128,
        source: FundingSource,
    ) -> Result<(), TreasuryError> {
        depositor.require_auth();
        Self::load_admin(&env)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if Self::is_paused(env.clone()) {
            return Err(TreasuryError::Paused);
        }

        let source_key = DataKey::SourceTotal(asset.clone(), source);
        let new_source_total = Self::read_i128(&env, &source_key)
            .checked_add(amount)
            .ok_or(TreasuryError::Overflow)?;
        let cap = Self::read_i128(&env, &DataKey::SourceCap(asset.clone(), source));
        if cap > 0 && new_source_total > cap {
            return Err(TreasuryError::SourceCapExceeded);
        }
        let total_key = DataKey::TotalDeposits(asset.clone());
        let new_total = Self::read_i128(&env, &total_key)
            .checked_add(amount)
            .ok_or(TreasuryError::Overflow)?;

        token::Client::new(&env, &asset).transfer(
            &depositor,
            &env.current_contract_address(),
            &amount,
        );

        Self::write_i128(&env, &source_key, new_source_total);
        Self::write_i128(&env, &total_key, new_total);

        env.events().publish(
            (symbol_short!("deposit"), depositor),
            DepositEvent {
                asset,
                source,
                amount,
            },
        );
        Ok(())
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    ///
    /// Adds to `total_withdrawn(token)`; per-source inflow totals are left
    /// untouched. Withdrawals stay available while deposits are paused so
    /// funds can still be moved during an incident.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let key = DataKey::TotalWithdrawn(token.clone());
        let new_total = Self::read_i128(&env, &key)
            .checked_add(amount)
            .ok_or(TreasuryError::Overflow)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        Self::write_i128(&env, &key, new_total);
        Ok(())
    }

    /// Pause or resume deposits. Admin-gated.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Paused, &paused);
        env.events().publish((symbol_short!("paused"),), paused);
        Ok(())
    }

    /// Whether deposits are currently rejected.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Cap cumulative deposits of `asset` from `source` at `cap`.
    /// `0` removes the cap. Admin-gated.
    ///
    /// A cap below the current total is allowed; it blocks further deposits
    /// from that source without touching what is already recorded.
    pub fn set_source_cap(
        env: Env,
        caller: Address,
        asset: Address,
        source: FundingSource,
        cap: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        if cap < 0 {
            return Err(TreasuryError::InvalidCap);
        }
        let key = DataKey::SourceCap(asset.clone(), source);
        let old = Self::read_i128(&env, &key);
        if cap == 0 {
            env.storage().persistent().remove(&key);
        } else {
            Self::write_i128(&env, &key, cap);
        }
        env.events()
            .publish((symbol_short!("src_cap"), asset, source), (old, cap));
        Ok(())
    }

    /// Configured cap for `(asset, source)`; `0` means no cap.
    pub fn source_cap(env: Env, asset: Address, source: FundingSource) -> i128 {
        Self::read_i128(&env, &DataKey::SourceCap(asset, source))
    }

    /// Cumulative deposits of `asset` tagged with `source`.
    pub fn total_by_source(env: Env, asset: Address, source: FundingSource) -> i128 {
        Self::read_i128(&env, &DataKey::SourceTotal(asset, source))
    }

    /// Cumulative deposits of `asset` across every source.
    pub fn total_deposits(env: Env, asset: Address) -> i128 {
        Self::read_i128(&env, &DataKey::TotalDeposits(asset))
    }

    /// Cumulative withdrawals of `asset`.
    pub fn total_withdrawn(env: Env, asset: Address) -> i128 {
        Self::read_i128(&env, &DataKey::TotalWithdrawn(asset))
    }

    /// Token balance minus the recorded net position
    /// (`total_deposits - total_withdrawn`).
    ///
    /// Positive: tokens arrived out-of-band (a direct transfer) and are not
    /// attributed to any source. Negative: tokens left without a recorded
    /// withdrawal (e.g. an issuer clawback). Zero: fully reconciled.
    pub fn unaccounted_balance(env: Env, asset: Address) -> Result<i128, TreasuryError> {
        let balance = token::Client::new(&env, &asset).balance(&env.current_contract_address());
        let net = Self::read_i128(&env, &DataKey::TotalDeposits(asset.clone()))
            .checked_sub(Self::read_i128(&env, &DataKey::TotalWithdrawn(asset)))
            .ok_or(TreasuryError::Overflow)?;
        balance.checked_sub(net).ok_or(TreasuryError::Overflow)
    }

    fn load_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        if *caller != Self::load_admin(env)? {
            return Err(TreasuryError::Unauthorized);
        }
        Ok(())
    }

    fn read_i128(env: &Env, key: &DataKey) -> i128 {
        env.storage().persistent().get(key).unwrap_or(0)
    }

    fn write_i128(env: &Env, key: &DataKey, value: i128) {
        let storage = env.storage().persistent();
        storage.set(key, &value);
        storage.extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO);
    }
}

#[cfg(test)]
mod test;
