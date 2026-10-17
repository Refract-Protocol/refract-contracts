#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{contract, contracterror, contractimpl, symbol_short, token, Address, Env};
use storage::{AllowanceRecord, AssetPolicy, DataKey, EpochBudget};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    ExceedsTxCap = 5,
    ExceedsEpochCap = 6,
    AllowanceExhausted = 7,
    AllowanceExpired = 8,
    InvalidPolicy = 9,
}

/// `RefractTreasury` custody vault with per-asset rolling budgets, spend caps, and named allowances (#208).
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin (or timelock) authorized to withdraw and configure policies.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Return the configured admin/timelock address.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    /// Configure or update the spending policy (`per_tx_cap`, `epoch_cap`, `epoch_length`) for `asset`.
    ///
    /// Gated by the admin/timelock. Preserves the current epoch's `spent` counter so mid-epoch
    /// cap changes never reset `epoch_spent` or permit a burst above the active epoch cap.
    pub fn set_asset_policy(
        env: Env,
        caller: Address,
        asset: Address,
        per_tx_cap: i128,
        epoch_cap: i128,
        epoch_length: u64,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        if per_tx_cap <= 0 || epoch_cap <= 0 || per_tx_cap > epoch_cap || epoch_length == 0 {
            return Err(TreasuryError::InvalidPolicy);
        }

        let now = env.ledger().timestamp();
        let policy = AssetPolicy {
            per_tx_cap,
            epoch_cap,
            epoch_length,
        };
        env.storage()
            .persistent()
            .set(&DataKey::AssetPolicy(asset.clone()), &policy);

        // Initialize EpochBudget if absent; if present, roll over only if the old epoch has elapsed,
        // preserving `spent` in the active epoch.
        let budget_key = DataKey::EpochBudget(asset.clone());
        if let Some(mut existing) = env
            .storage()
            .persistent()
            .get::<DataKey, EpochBudget>(&budget_key)
        {
            if now >= existing.epoch_start.saturating_add(epoch_length) {
                let elapsed = now - existing.epoch_start;
                let epochs_passed = elapsed / epoch_length;
                existing.epoch_start = existing
                    .epoch_start
                    .saturating_add(epochs_passed * epoch_length);
                existing.spent = 0;
                env.storage().persistent().set(&budget_key, &existing);
            }
        } else {
            env.storage().persistent().set(
                &budget_key,
                &EpochBudget {
                    epoch_start: now,
                    spent: 0,
                },
            );
        }

        env.events().publish(
            (symbol_short!("pol_set"), asset),
            (per_tx_cap, epoch_cap, epoch_length),
        );
        Ok(())
    }

    /// Grant or update a named allowance `(spender, asset, amount, expires_at)` through the admin/timelock.
    pub fn grant_allowance(
        env: Env,
        caller: Address,
        spender: Address,
        asset: Address,
        amount: i128,
        expires_at: u64,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        if amount < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let rec = AllowanceRecord {
            remaining: amount,
            expires_at,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Allowance(spender.clone(), asset.clone()), &rec);
        env.events().publish(
            (symbol_short!("alw_set"), spender, asset),
            (amount, expires_at),
        );
        Ok(())
    }

    /// Withdraw `amount` of `token` to `to`. Admin/timelock-gated and enforced by [`Self::check_and_record_spend`].
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

        Self::check_and_record_spend(&env, &token, amount)?;

        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        env.events()
            .publish((symbol_short!("wth_spnd"), token, to), amount);
        Ok(())
    }

    /// Spend `amount` of `asset` to `to` using `spender`'s granted allowance.
    /// Enforces allowance expiry, remaining allowance balance, AND [`Self::check_and_record_spend`].
    pub fn spend_from_allowance(
        env: Env,
        spender: Address,
        asset: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        spender.require_auth();
        let _admin = Self::read_admin(&env)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let alw_key = DataKey::Allowance(spender.clone(), asset.clone());
        let mut alw: AllowanceRecord = env
            .storage()
            .persistent()
            .get(&alw_key)
            .ok_or(TreasuryError::AllowanceExhausted)?;

        if env.ledger().timestamp() >= alw.expires_at {
            return Err(TreasuryError::AllowanceExpired);
        }
        if amount > alw.remaining {
            return Err(TreasuryError::AllowanceExhausted);
        }

        // Single enforcement point for per-tx and rolling-epoch caps.
        Self::check_and_record_spend(&env, &asset, amount)?;

        alw.remaining -= amount;
        env.storage().persistent().set(&alw_key, &alw);

        token::Client::new(&env, &asset).transfer(&env.current_contract_address(), &to, &amount);
        env.events().publish(
            (symbol_short!("alw_spnd"), spender, asset),
            (to, amount, alw.remaining),
        );
        Ok(())
    }

    /// Read the configured `AssetPolicy` for `asset`, if any.
    pub fn get_asset_policy(env: Env, asset: Address) -> Option<AssetPolicy> {
        env.storage().persistent().get(&DataKey::AssetPolicy(asset))
    }

    /// Read the active `EpochBudget` for `asset` (reflecting deterministic epoch rollover at the current timestamp).
    pub fn get_epoch_budget(env: Env, asset: Address) -> Option<EpochBudget> {
        let budget: EpochBudget = env
            .storage()
            .persistent()
            .get(&DataKey::EpochBudget(asset.clone()))?;
        if let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, AssetPolicy>(&DataKey::AssetPolicy(asset))
        {
            Some(Self::rollover_budget(
                budget,
                policy.epoch_length,
                env.ledger().timestamp(),
            ))
        } else {
            Some(budget)
        }
    }

    /// Read the `AllowanceRecord` for `(spender, asset)`, if any.
    pub fn get_allowance(env: Env, spender: Address, asset: Address) -> Option<AllowanceRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Allowance(spender, asset))
    }

    /// Single enforcement point for per-transaction and rolling-epoch spend caps across both
    /// [`Self::withdraw`] and [`Self::spend_from_allowance`].
    fn check_and_record_spend(
        env: &Env,
        asset: &Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, AssetPolicy>(&DataKey::AssetPolicy(asset.clone()))
        else {
            return Ok(());
        };

        if amount > policy.per_tx_cap {
            return Err(TreasuryError::ExceedsTxCap);
        }

        let now = env.ledger().timestamp();
        let budget_key = DataKey::EpochBudget(asset.clone());
        let raw_budget = env
            .storage()
            .persistent()
            .get::<DataKey, EpochBudget>(&budget_key)
            .unwrap_or(EpochBudget {
                epoch_start: now,
                spent: 0,
            });

        let mut budget = Self::rollover_budget(raw_budget, policy.epoch_length, now);
        let new_spent = budget
            .spent
            .checked_add(amount)
            .ok_or(TreasuryError::ExceedsEpochCap)?;
        if new_spent > policy.epoch_cap {
            return Err(TreasuryError::ExceedsEpochCap);
        }

        budget.spent = new_spent;
        env.storage().persistent().set(&budget_key, &budget);
        Ok(())
    }

    fn rollover_budget(mut budget: EpochBudget, epoch_length: u64, now: u64) -> EpochBudget {
        if epoch_length > 0 && now >= budget.epoch_start.saturating_add(epoch_length) {
            let elapsed = now - budget.epoch_start;
            let epochs_passed = elapsed / epoch_length;
            budget.epoch_start = budget
                .epoch_start
                .saturating_add(epochs_passed * epoch_length);
            budget.spent = 0;
        }
        budget
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        let admin = Self::read_admin(env)?;
        if caller != &admin {
            return Err(TreasuryError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};

    fn setup() -> (
        Env,
        RefractTreasuryClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
            li.min_temp_entry_ttl = 100_000;
            li.min_persistent_entry_ttl = 100_000;
            li.max_entry_ttl = 1_000_000;
        });
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_a = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_a).mint(&contract_id, &10_000);
        token::StellarAssetClient::new(&env, &token_b).mint(&contract_id, &10_000);

        (env, client, admin, token_a, token_b)
    }

    #[test]
    fn cap_boundaries_and_epoch_rollover_reset() {
        let (env, client, admin, token_a, _token_b) = setup();
        let to = Address::generate(&env);

        // per_tx_cap = 400, epoch_cap = 1_000, epoch_length = 3_600s.
        client.set_asset_policy(&admin, &token_a, &400, &1_000, &3_600);

        // 1 stroop over per_tx_cap -> ExceedsTxCap.
        assert_eq!(
            client.try_withdraw(&admin, &token_a, &401, &to),
            Err(Ok(TreasuryError::ExceedsTxCap))
        );

        // Exactly at per_tx_cap twice (400 + 400 = 800).
        client.withdraw(&admin, &token_a, &400, &to);
        client.withdraw(&admin, &token_a, &400, &to);
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 800);

        // 1 stroop over remaining epoch_cap (201 > 200) -> ExceedsEpochCap.
        assert_eq!(
            client.try_withdraw(&admin, &token_a, &201, &to),
            Err(Ok(TreasuryError::ExceedsEpochCap))
        );

        // Exactly at remaining epoch_cap (200 -> total 1_000).
        client.withdraw(&admin, &token_a, &200, &to);
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 1_000);

        // Advance timestamp to epoch boundary (1_700_000_000 + 3_600) -> spent resets to 0!
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_003_600;
        });
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 0);
        client.withdraw(&admin, &token_a, &400, &to);
        client.withdraw(&admin, &token_a, &100, &to);
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 500);
    }

    #[test]
    fn allowance_spend_shares_enforcement_and_expires_across_epoch() {
        let (env, client, admin, token_a, _token_b) = setup();
        let spender = Address::generate(&env);
        let recipient = Address::generate(&env);

        client.set_asset_policy(&admin, &token_a, &500, &700, &3_600);
        // Grant spender 600 expiring at 1_700_005_000 (across the first epoch boundary at +3_600).
        client.grant_allowance(&admin, &spender, &token_a, &600, &1_700_005_000);

        // Direct admin withdrawal consumes 400 of the 700 epoch cap.
        client.withdraw(&admin, &token_a, &400, &recipient);

        // Spender trying to spend 350 exceeds the remaining 300 epoch cap -> ExceedsEpochCap!
        assert_eq!(
            client.try_spend_from_allowance(&spender, &token_a, &350, &recipient),
            Err(Ok(TreasuryError::ExceedsEpochCap))
        );

        // Spender spends 300 (hits epoch_cap = 700; remaining allowance = 300).
        client.spend_from_allowance(&spender, &token_a, &300, &recipient);
        assert_eq!(
            client.get_allowance(&spender, &token_a).unwrap().remaining,
            300
        );

        // Roll over into next epoch (+3_600s); spender tries 301 -> AllowanceExhausted.
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_003_600;
        });
        assert_eq!(
            client.try_spend_from_allowance(&spender, &token_a, &301, &recipient),
            Err(Ok(TreasuryError::AllowanceExhausted))
        );

        // Advance past allowance expiry (1_700_005_000) -> AllowanceExpired without corrupting epoch budget.
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_005_000;
        });
        assert_eq!(
            client.try_spend_from_allowance(&spender, &token_a, &100, &recipient),
            Err(Ok(TreasuryError::AllowanceExpired))
        );
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 0);
    }

    #[test]
    fn mid_epoch_cap_change_preserves_spent_and_multi_asset_isolation() {
        let (env, client, admin, token_a, token_b) = setup();
        let to = Address::generate(&env);

        client.set_asset_policy(&admin, &token_a, &500, &1_000, &3_600);
        client.set_asset_policy(&admin, &token_b, &500, &1_000, &3_600);

        client.withdraw(&admin, &token_a, &450, &to);
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 450);
        assert_eq!(client.get_epoch_budget(&token_b).unwrap().spent, 0);

        // Mid-epoch governance tightening of token_a epoch_cap to 600 must NOT reset the 450 already spent.
        client.set_asset_policy(&admin, &token_a, &500, &600, &3_600);
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 450);
        assert_eq!(
            client.try_withdraw(&admin, &token_a, &200, &to),
            Err(Ok(TreasuryError::ExceedsEpochCap))
        );
        client.withdraw(&admin, &token_a, &150, &to);
        assert_eq!(client.get_epoch_budget(&token_a).unwrap().spent, 600);

        // token_b is completely isolated from token_a's spent counter.
        client.withdraw(&admin, &token_b, &500, &to);
        assert_eq!(client.get_epoch_budget(&token_b).unwrap().spent, 500);
    }
}
