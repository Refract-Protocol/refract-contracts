//! # RefractVeToken
//!
//! Vote-escrowed governance weight with protocol fee sharing.
//!
//! ## Voting power
//!
//! An account's governance weight ([`VotingPowerInterface::get_voting_power`])
//! has two parts:
//!
//! * **Lock power** — `amount * remaining_lock_time / MAX_LOCK_TIME`, the
//!   standard linear-decay veToken curve ([`RefractVeToken::voting_power`]).
//! * **Backstop power** — weight for capital at risk in `RefractBackstop`:
//!
//!   ```text
//!   backstop_power = min(active_value * weight_bps / 10_000, max_power)
//!   ```
//!
//!   `active_value` is the current value of the account's *active* backstop
//!   shares (after any drawdown losses). Shares in the unstake cooldown count
//!   for **zero**: a staker who has asked to leave no longer bears the
//!   protocol's long-term risk. `max_power = 0` means no cap. Rounded down.
//!   Configured by the admin with [`RefractVeToken::set_backstop_weight`];
//!   with no backstop configured, backstop power is zero. See SPEC.md
//!   § "Backstop voting power".
//!
//! Backstop power is **pulled** at read time: every call reads
//! `RefractBackstop::get_stake` through the typed
//! [`refract_interfaces::BackstopClient`], so a `deposit`,
//! `request_unstake`, `cancel_unstake` or `withdraw` is visible to the next
//! `get_voting_power` call in the same transaction. The backstop never calls
//! this contract, so there is no re-entrant path and nothing to keep in sync.
//! Each read is one cross-contract call with no iteration over stakers.
//!
//! Fee sharing ([`RefractVeToken::distribute_fees`]) uses lock power only.
//!
//! ## Authorization
//!
//! | Entrypoint | Required principal |
//! |------------|--------------------|
//! | `set_backstop_weight`, `propose_admin` | admin (meant to be the timelock) |
//! | `accept_admin` | pending admin |
//! | `lock`, `extend_lock`, `withdraw`, `claim_fees` | the account |
//! | `distribute_fees` | the treasury |

#![no_std]

use refract_interfaces::{access, apply_bps, Bps, RefractError, VotingPowerInterface, BPS_DENOM};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol,
};

/// Maximum lock duration in seconds (4 years), used as the veToken decay denominator.
const MAX_LOCK_TIME: u64 = 4 * 365 * 24 * 60 * 60;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockInfo {
    pub amount: i128,
    pub unlock_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeCheckpoint {
    /// Cumulative protocol fees routed to lockers per unit of voting power (scaled).
    pub acc_fee_per_power: i128,
    /// Total voting power at the time of the last distribution epoch.
    pub total_power: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeAccount {
    /// Last cumulative fee-per-power value seen by this locker.
    pub reward_per_power_paid: i128,
    /// Fees accrued but not yet claimed.
    pub pending: i128,
}

/// How backstop stake turns into voting power. See the crate docs.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopWeight {
    /// The `RefractBackstop` contract whose stakes count.
    pub backstop: Address,
    /// Voting power per unit of active backstop value, in basis points
    /// (`10_000` = one vote per token, 1e7 fixed point like lock power).
    pub weight_bps: Bps,
    /// Upper bound on one account's backstop power. `0` means no cap.
    pub max_power: i128,
}

/// Storage keys.
#[contracttype]
pub enum DataKey {
    Token,
    Treasury,
    Lock(Address),
    FeeAccount(Address),
    FeeCheckpoint,
    /// [`BackstopWeight`] (instance). Absent until configured.
    BackstopWeight,
    /// Lock power recorded at the account's last lock change (persistent).
    RecordedPower(Address),
}

/// Highest weight the admin may set: 10 votes per backstop token.
pub const MAX_BACKSTOP_WEIGHT_BPS: Bps = 10 * BPS_DENOM;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum VeTokenError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InvalidDuration = 4,
    NoLock = 5,
    LockNotExpired = 6,
    CannotShortenLock = 7,
    NothingToClaim = 8,
}

#[contract]
pub struct RefractVeToken;

#[contractimpl]
impl RefractVeToken {
    /// Initialize the contract with its admin (meant to be the timelock), the
    /// locked token and the treasury that routes fees.
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        treasury: Address,
    ) -> Result<(), VeTokenError> {
        if env.storage().instance().has(&DataKey::Token) {
            return Err(VeTokenError::AlreadyInitialized);
        }
        access::init(&env, &admin).map_err(|_| VeTokenError::AlreadyInitialized)?;
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage().instance().set(
            &DataKey::FeeCheckpoint,
            &FeeCheckpoint {
                acc_fee_per_power: 0,
                total_power: 0,
            },
        );
        Ok(())
    }

    /// Lock `amount` of the governance token for `duration` seconds, minting voting power.
    pub fn lock(
        env: Env,
        caller: Address,
        amount: i128,
        duration: u64,
    ) -> Result<(), VeTokenError> {
        caller.require_auth();
        if amount <= 0 {
            return Err(VeTokenError::InvalidAmount);
        }
        if duration == 0 || duration > MAX_LOCK_TIME {
            return Err(VeTokenError::InvalidDuration);
        }

        let token = Self::token(&env)?;
        let now = env.ledger().timestamp();
        let unlock_at = now + duration;

        // Settle any pending fees for the caller before their power changes.
        Self::settle_fee_account(&env, &caller)?;

        let existing = Self::get_lock(&env, &caller);
        let new_amount = existing.amount + amount;
        // Extending an existing lock must never shorten its effective unlock time.
        let new_unlock_at = if existing.unlock_at > unlock_at {
            existing.unlock_at
        } else {
            unlock_at
        };

        token::Client::new(&env, &token).transfer(
            &caller,
            &env.current_contract_address(),
            &amount,
        );

        env.storage().persistent().set(
            &DataKey::Lock(caller.clone()),
            &LockInfo {
                amount: new_amount,
                unlock_at: new_unlock_at,
            },
        );

        Self::refresh_total_power(&env, &caller);
        Ok(())
    }

    /// Extend the caller's lock forward by `duration` seconds. Never shortens the lock.
    pub fn extend_lock(env: Env, caller: Address, duration: u64) -> Result<(), VeTokenError> {
        caller.require_auth();
        if duration == 0 {
            return Err(VeTokenError::InvalidDuration);
        }

        let mut lock = Self::get_lock(&env, &caller);
        if lock.amount <= 0 {
            return Err(VeTokenError::NoLock);
        }

        let now = env.ledger().timestamp();
        let base = if lock.unlock_at > now {
            lock.unlock_at
        } else {
            now
        };
        let proposed = base + duration;

        // Only allow extending forward; reject any attempt to shorten effective lock time.
        if proposed <= lock.unlock_at {
            return Err(VeTokenError::CannotShortenLock);
        }
        if proposed - now > MAX_LOCK_TIME {
            return Err(VeTokenError::InvalidDuration);
        }

        Self::settle_fee_account(&env, &caller)?;

        lock.unlock_at = proposed;
        env.storage()
            .persistent()
            .set(&DataKey::Lock(caller.clone()), &lock);

        Self::refresh_total_power(&env, &caller);
        Ok(())
    }

    /// Withdraw locked tokens. Only permitted after the lock has fully expired.
    pub fn withdraw(env: Env, caller: Address) -> Result<i128, VeTokenError> {
        caller.require_auth();

        let lock = Self::get_lock(&env, &caller);
        if lock.amount <= 0 {
            return Err(VeTokenError::NoLock);
        }

        let now = env.ledger().timestamp();
        if now < lock.unlock_at {
            return Err(VeTokenError::LockNotExpired);
        }

        Self::settle_fee_account(&env, &caller)?;

        let amount = lock.amount;
        env.storage()
            .persistent()
            .remove(&DataKey::Lock(caller.clone()));

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &caller,
            &amount,
        );

        Self::refresh_total_power(&env, &caller);
        Ok(amount)
    }

    /// Time-weighted voting power using the standard linear-decay veToken curve:
    /// `amount * remaining_lock_time / max_lock_time`, decaying to zero at expiry.
    pub fn voting_power(env: Env, address: Address) -> i128 {
        let lock = Self::get_lock(&env, &address);
        if lock.amount <= 0 {
            return 0;
        }
        let now = env.ledger().timestamp();
        if now >= lock.unlock_at {
            return 0;
        }
        let remaining = (lock.unlock_at - now) as i128;
        lock.amount * remaining / (MAX_LOCK_TIME as i128)
    }

    // --- backstop weight & admin ---

    /// Count active `RefractBackstop` stake towards voting power. Admin only.
    /// `weight_bps` is capped at [`MAX_BACKSTOP_WEIGHT_BPS`]; `max_power`
    /// must not be negative (`0` = no cap).
    pub fn set_backstop_weight(
        env: Env,
        caller: Address,
        weight: BackstopWeight,
    ) -> Result<(), RefractError> {
        access::require_admin(&env, &caller)?;
        if weight.weight_bps > MAX_BACKSTOP_WEIGHT_BPS || weight.max_power < 0 {
            return Err(RefractError::InvalidArgument);
        }
        env.storage()
            .instance()
            .set(&DataKey::BackstopWeight, &weight);
        env.events().publish(
            (
                Symbol::new(&env, "ve_backstop_weight"),
                weight.backstop.clone(),
            ),
            (weight.weight_bps, weight.max_power),
        );
        Ok(())
    }

    /// Stop counting backstop stake. Admin only.
    pub fn clear_backstop_weight(env: Env, caller: Address) -> Result<(), RefractError> {
        access::require_admin(&env, &caller)?;
        env.storage().instance().remove(&DataKey::BackstopWeight);
        env.events()
            .publish((Symbol::new(&env, "ve_backstop_cleared"),), caller);
        Ok(())
    }

    /// The backstop weighting in force, if any.
    pub fn backstop_weight(env: Env) -> Option<BackstopWeight> {
        env.storage().instance().get(&DataKey::BackstopWeight)
    }

    /// Current admin.
    pub fn admin(env: Env) -> Result<Address, RefractError> {
        access::admin(&env)
    }

    /// Step one of the admin handover. Admin only.
    pub fn propose_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), RefractError> {
        access::propose_admin(&env, &caller, &new_admin)
    }

    /// Step two of the admin handover. Pending admin only.
    pub fn accept_admin(env: Env, caller: Address) -> Result<(), RefractError> {
        access::accept_admin(&env, &caller)
    }

    /// Route protocol fees from the treasury into the fee-sharing pool for lockers.
    /// Called by the treasury (or anyone) to distribute a fee amount across current voting power.
    pub fn distribute_fees(env: Env, amount: i128) -> Result<(), VeTokenError> {
        if amount <= 0 {
            return Err(VeTokenError::InvalidAmount);
        }
        let treasury = Self::treasury(&env)?;
        treasury.require_auth();

        let mut checkpoint = Self::fee_checkpoint(&env);
        if checkpoint.total_power > 0 {
            checkpoint.acc_fee_per_power += amount * 1_000_000_000 / checkpoint.total_power;
        }
        env.storage()
            .instance()
            .set(&DataKey::FeeCheckpoint, &checkpoint);
        Ok(())
    }

    /// Claim the caller's pro-rata share of distributed protocol fees.
    pub fn claim_fees(env: Env, caller: Address) -> Result<i128, VeTokenError> {
        caller.require_auth();
        let pending = Self::settle_fee_account(&env, &caller)?;
        if pending <= 0 {
            return Err(VeTokenError::NothingToClaim);
        }

        let mut account = Self::fee_account(&env, &caller);
        account.pending = 0;
        env.storage()
            .persistent()
            .set(&DataKey::FeeAccount(caller.clone()), &account);

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &caller,
            &pending,
        );
        Ok(pending)
    }

    /// View the caller's currently claimable (unclaimed) fee balance.
    pub fn claimable_fees(env: Env, address: Address) -> i128 {
        let checkpoint = Self::fee_checkpoint(&env);
        let account = Self::fee_account(&env, &address);
        let power = Self::recorded_power(&env, &address);
        let delta = checkpoint.acc_fee_per_power - account.reward_per_power_paid;
        if delta <= 0 {
            return account.pending;
        }
        account.pending + power * delta / 1_000_000_000
    }

    // --- internal helpers ---

    /// See the crate docs for the formula. Any failure to read the backstop
    /// (not configured, not initialized, call reverted) counts as zero, so a
    /// broken backstop can never block governance.
    fn backstop_power_of(env: &Env, address: &Address) -> i128 {
        let Some(weight) = env
            .storage()
            .instance()
            .get::<DataKey, BackstopWeight>(&DataKey::BackstopWeight)
        else {
            return 0;
        };
        let stake = match refract_interfaces::BackstopClient::new(env, &weight.backstop)
            .try_get_stake(address)
        {
            Ok(Ok(stake)) => stake,
            _ => return 0,
        };
        if stake.active_value <= 0 {
            return 0;
        }
        let power = apply_bps(stake.active_value, weight.weight_bps).unwrap_or(0);
        if weight.max_power > 0 && power > weight.max_power {
            weight.max_power
        } else {
            power
        }
    }

    fn token(env: &Env) -> Result<Address, VeTokenError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(VeTokenError::NotInitialized)
    }

    fn treasury(env: &Env) -> Result<Address, VeTokenError> {
        env.storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(VeTokenError::NotInitialized)
    }

    fn get_lock(env: &Env, address: &Address) -> LockInfo {
        env.storage()
            .persistent()
            .get(&DataKey::Lock(address.clone()))
            .unwrap_or(LockInfo {
                amount: 0,
                unlock_at: 0,
            })
    }

    fn fee_checkpoint(env: &Env) -> FeeCheckpoint {
        env.storage()
            .instance()
            .get(&DataKey::FeeCheckpoint)
            .unwrap_or(FeeCheckpoint {
                acc_fee_per_power: 0,
                total_power: 0,
            })
    }

    fn fee_account(env: &Env, address: &Address) -> FeeAccount {
        env.storage()
            .persistent()
            .get(&DataKey::FeeAccount(address.clone()))
            .unwrap_or(FeeAccount {
                reward_per_power_paid: 0,
                pending: 0,
            })
    }

    /// Accrue any newly-distributed fees into the caller's pending balance and
    /// record the checkpoint they have been paid up to.
    fn settle_fee_account(env: &Env, address: &Address) -> Result<i128, VeTokenError> {
        let checkpoint = Self::fee_checkpoint(env);
        let mut account = Self::fee_account(env, address);
        let power = Self::recorded_power(env, address);
        let delta = checkpoint.acc_fee_per_power - account.reward_per_power_paid;
        if delta > 0 {
            account.pending += power * delta / 1_000_000_000;
        }
        account.reward_per_power_paid = checkpoint.acc_fee_per_power;
        env.storage()
            .persistent()
            .set(&DataKey::FeeAccount(address.clone()), &account);
        Ok(account.pending)
    }

    /// Lock power recorded for `address` at its last lock change. Fees are
    /// split by recorded power, so the shares always add up to what was
    /// distributed; decay between an account's own actions is not counted.
    fn recorded_power(env: &Env, address: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::RecordedPower(address.clone()))
            .unwrap_or(0)
    }

    /// Re-record `address`'s lock power after its lock changed and update the
    /// total used as the fee-distribution denominator. Call after
    /// `settle_fee_account`, so fees up to now use the old power.
    fn refresh_total_power(env: &Env, address: &Address) {
        let old = Self::recorded_power(env, address);
        let new = Self::voting_power(env.clone(), address.clone());
        let key = DataKey::RecordedPower(address.clone());
        if new == 0 {
            env.storage().persistent().remove(&key);
        } else {
            env.storage().persistent().set(&key, &new);
        }
        let mut checkpoint = Self::fee_checkpoint(env);
        checkpoint.total_power = (checkpoint.total_power - old + new).max(0);
        env.storage()
            .instance()
            .set(&DataKey::FeeCheckpoint, &checkpoint);
    }
}

#[contractimpl]
impl VotingPowerInterface for RefractVeToken {
    /// Lock power plus backstop power. This is the weight governance uses
    /// for proposal thresholds and votes.
    fn get_voting_power(env: Env, account: Address) -> i128 {
        let locked = Self::voting_power(env.clone(), account.clone());
        locked.saturating_add(Self::backstop_power_of(&env, &account))
    }

    /// Lock power only (same as `voting_power`).
    fn locked_power(env: Env, account: Address) -> i128 {
        Self::voting_power(env, account)
    }

    /// Backstop power only.
    fn backstop_power(env: Env, account: Address) -> i128 {
        Self::backstop_power_of(&env, &account)
    }
}

#[cfg(test)]
mod backstop_test;

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{token, Env};

    fn setup() -> (
        Env,
        RefractVeTokenClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractVeToken);
        let client = RefractVeTokenClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let treasury = Address::generate(&env);

        client.initialize(&admin, &token_id, &treasury);
        (env, client, admin, token_id, treasury)
    }

    fn mint(env: &Env, token_id: &Address, to: &Address, amount: i128) {
        let sac = token::StellarAssetClient::new(env, token_id);
        sac.mint(to, &amount);
    }

    #[test]
    fn lock_and_decay() {
        let (env, client, user, token_id, _) = setup();
        mint(&env, &token_id, &user, 1_000);

        client.lock(&user, &1_000, &MAX_LOCK_TIME);
        assert_eq!(client.voting_power(&user), 1_000);

        env.ledger().with_mut(|l| l.timestamp += MAX_LOCK_TIME / 2);
        assert_eq!(client.voting_power(&user), 500);

        env.ledger().with_mut(|l| l.timestamp += MAX_LOCK_TIME / 2);
        assert_eq!(client.voting_power(&user), 0);
    }

    #[test]
    fn extend_lock_never_shortens() {
        let (env, client, user, token_id, _) = setup();
        mint(&env, &token_id, &user, 1_000);
        client.lock(&user, &1_000, &(MAX_LOCK_TIME / 2));

        let before = client.voting_power(&user);
        client.extend_lock(&user, &(MAX_LOCK_TIME / 4));
        assert!(client.voting_power(&user) > before);
    }

    #[test]
    fn premature_withdraw_rejected() {
        let (env, client, user, token_id, _) = setup();
        mint(&env, &token_id, &user, 1_000);
        client.lock(&user, &1_000, &MAX_LOCK_TIME);
        let res = client.try_withdraw(&user);
        assert!(res.is_err());
    }

    #[test]
    fn withdraw_after_expiry() {
        let (env, client, user, token_id, _) = setup();
        mint(&env, &token_id, &user, 1_000);
        client.lock(&user, &1_000, &MAX_LOCK_TIME);
        env.ledger().with_mut(|l| l.timestamp += MAX_LOCK_TIME + 1);
        assert_eq!(client.withdraw(&user), 1_000);
    }

    #[test]
    fn fee_distribution_pro_rata() {
        let (env, client, alice, token_id, treasury) = setup();
        let bob = Address::generate(&env);
        mint(&env, &token_id, &alice, 1_000);
        mint(&env, &token_id, &bob, 1_000);

        // Alice locks for the full max time, Bob for half -> Alice has 2x power.
        client.lock(&alice, &1_000, &MAX_LOCK_TIME);
        client.lock(&bob, &1_000, &(MAX_LOCK_TIME / 2));

        // Treasury routes 300 in fees.
        mint(&env, &token_id, &treasury, 300);
        client.distribute_fees(&300);

        let alice_claim = client.claimable_fees(&alice);
        let bob_claim = client.claimable_fees(&bob);
        assert!(alice_claim > bob_claim);
        assert_eq!(alice_claim + bob_claim, 300);
    }
}
