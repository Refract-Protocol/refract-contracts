//! # RefractTreasury
//!
//! Protocol treasury with recurring operational payment schedules (`#258`).
//!
//! - Governance/admin creates bounded recurring schedules (`create_schedule`) that reserve
//!   `amount_per_period * total_periods` against uncommitted treasury funds.
//! - Permissionless `execute_schedule` settles due periods (including multi-period catch-up)
//!   while respecting protocol pause state.
//! - `cancel_schedule` releases unspent reserved budget without clawing back completed periods.
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ContractPaused = 3,
    InvalidScheduleConfig = 4,
    InsufficientUncommittedFunds = 5,
    ScheduleNotFound = 6,
    ScheduleCancelled = 7,
    ScheduleCompleted = 8,
    ScheduleNotDueYet = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringScheduleSpec {
    pub beneficiary: Address,
    pub asset: Address,
    pub amount_per_period: i128,
    pub period_ledgers: u32,
    pub total_periods: u32,
    pub start_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringSchedule {
    pub schedule_id: u64,
    pub beneficiary: Address,
    pub asset: Address,
    pub amount_per_period: i128,
    pub period_ledgers: u32,
    pub total_periods: u32,
    pub executed_periods: u32,
    pub start_ledger: u32,
    pub cancelled: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Paused,
    NextScheduleId,
    Schedule(u64),
    ReservedAsset(Address),
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage()
            .instance()
            .set(&DataKey::NextScheduleId, &1u64);
        Ok(())
    }

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), TreasuryError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may pause/unpause treasury");
        }
        env.storage().instance().set(&DataKey::Paused, &paused);
        Ok(())
    }

    /// Creates a bounded recurring payment schedule and reserves its total budget from
    /// uncommitted treasury balance.
    pub fn create_schedule(
        env: Env,
        caller: Address,
        spec: RecurringScheduleSpec,
    ) -> Result<u64, TreasuryError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may create recurring schedules");
        }
        Self::require_not_paused(&env)?;

        if spec.amount_per_period <= 0 || spec.period_ledgers == 0 || spec.total_periods == 0 {
            return Err(TreasuryError::InvalidScheduleConfig);
        }

        let total_commitment = spec.amount_per_period * (spec.total_periods as i128);
        let uncommitted = Self::uncommitted_balance(env.clone(), spec.asset.clone());
        if total_commitment > uncommitted {
            return Err(TreasuryError::InsufficientUncommittedFunds);
        }

        let schedule_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextScheduleId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextScheduleId, &(schedule_id + 1));

        let record = RecurringSchedule {
            schedule_id,
            beneficiary: spec.beneficiary.clone(),
            asset: spec.asset.clone(),
            amount_per_period: spec.amount_per_period,
            period_ledgers: spec.period_ledgers,
            total_periods: spec.total_periods,
            executed_periods: 0,
            start_ledger: spec.start_ledger,
            cancelled: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Schedule(schedule_id), &record);

        let prev_reserved = Self::reserved_balance(env.clone(), spec.asset.clone());
        env.storage().persistent().set(
            &DataKey::ReservedAsset(spec.asset.clone()),
            &(prev_reserved + total_commitment),
        );

        env.events().publish(
            (symbol_short!("sch_new"), schedule_id, spec.beneficiary),
            (spec.asset, spec.amount_per_period, spec.total_periods),
        );
        Ok(schedule_id)
    }

    /// Permissionlessly executes all due periods of `schedule_id` at the current ledger.
    pub fn execute_schedule(env: Env, schedule_id: u64) -> Result<i128, TreasuryError> {
        Self::require_admin(&env)?;
        Self::require_not_paused(&env)?;

        let mut record = Self::schedule(env.clone(), schedule_id)?;
        if record.cancelled {
            return Err(TreasuryError::ScheduleCancelled);
        }
        if record.executed_periods >= record.total_periods {
            return Err(TreasuryError::ScheduleCompleted);
        }

        let next_due = record.start_ledger.saturating_add(
            record
                .executed_periods
                .saturating_mul(record.period_ledgers),
        );
        let now = env.ledger().sequence();
        if now < next_due {
            return Err(TreasuryError::ScheduleNotDueYet);
        }

        let elapsed_since_next = (now - next_due) / record.period_ledgers + 1;
        let remaining_periods = record.total_periods - record.executed_periods;
        let periods_to_pay = elapsed_since_next.min(remaining_periods);
        let payout = record.amount_per_period * (periods_to_pay as i128);

        record.executed_periods += periods_to_pay;
        env.storage()
            .persistent()
            .set(&DataKey::Schedule(schedule_id), &record);

        let prev_reserved = Self::reserved_balance(env.clone(), record.asset.clone());
        env.storage().persistent().set(
            &DataKey::ReservedAsset(record.asset.clone()),
            &(prev_reserved - payout),
        );

        let token_client = token::Client::new(&env, &record.asset);
        token_client.transfer(
            &env.current_contract_address(),
            &record.beneficiary,
            &payout,
        );

        env.events().publish(
            (symbol_short!("sch_exec"), schedule_id, record.beneficiary),
            (periods_to_pay, payout, record.executed_periods),
        );
        Ok(payout)
    }

    /// Cancels a schedule and releases its remaining unspent budget back to uncommitted treasury funds.
    pub fn cancel_schedule(
        env: Env,
        caller: Address,
        schedule_id: u64,
    ) -> Result<i128, TreasuryError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may cancel schedules");
        }

        let mut record = Self::schedule(env.clone(), schedule_id)?;
        if record.cancelled {
            return Err(TreasuryError::ScheduleCancelled);
        }

        let remaining_periods = record.total_periods.saturating_sub(record.executed_periods);
        let released = record.amount_per_period * (remaining_periods as i128);

        record.cancelled = true;
        env.storage()
            .persistent()
            .set(&DataKey::Schedule(schedule_id), &record);

        if released > 0 {
            let prev_reserved = Self::reserved_balance(env.clone(), record.asset.clone());
            env.storage().persistent().set(
                &DataKey::ReservedAsset(record.asset.clone()),
                &(prev_reserved - released),
            );
        }

        env.events()
            .publish((symbol_short!("sch_cncl"), schedule_id, caller), released);
        Ok(released)
    }

    /// Direct admin withdrawal gated by `uncommitted_balance(asset)` so reserved schedule budgets
    /// cannot be starved.
    pub fn withdraw(
        env: Env,
        caller: Address,
        asset: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may withdraw from treasury");
        }
        Self::require_not_paused(&env)?;
        if amount <= 0 || amount > Self::uncommitted_balance(env.clone(), asset.clone()) {
            return Err(TreasuryError::InsufficientUncommittedFunds);
        }

        let token_client = token::Client::new(&env, &asset);
        token_client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    pub fn schedule(env: Env, schedule_id: u64) -> Result<RecurringSchedule, TreasuryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Schedule(schedule_id))
            .ok_or(TreasuryError::ScheduleNotFound)
    }

    pub fn next_payment_at(env: Env, schedule_id: u64) -> Result<u32, TreasuryError> {
        let record = Self::schedule(env, schedule_id)?;
        if record.cancelled {
            return Err(TreasuryError::ScheduleCancelled);
        }
        if record.executed_periods >= record.total_periods {
            return Err(TreasuryError::ScheduleCompleted);
        }
        Ok(record.start_ledger.saturating_add(
            record
                .executed_periods
                .saturating_mul(record.period_ledgers),
        ))
    }

    pub fn remaining_budget(env: Env, schedule_id: u64) -> Result<i128, TreasuryError> {
        let record = Self::schedule(env, schedule_id)?;
        if record.cancelled {
            return Ok(0);
        }
        let remaining_periods = record.total_periods.saturating_sub(record.executed_periods);
        Ok(record.amount_per_period * (remaining_periods as i128))
    }

    pub fn reserved_balance(env: Env, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::ReservedAsset(asset))
            .unwrap_or(0)
    }

    pub fn uncommitted_balance(env: Env, asset: Address) -> i128 {
        let token_client = token::Client::new(&env, &asset);
        let total = token_client.balance(&env.current_contract_address());
        let reserved = Self::reserved_balance(env, asset);
        (total - reserved).max(0)
    }

    fn require_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn require_not_paused(env: &Env) -> Result<(), TreasuryError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(TreasuryError::ContractPaused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn recurring_schedule_reservation_multi_period_catchup_pause_and_cancel() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.sequence_number = 100);

        let admin = Address::generate(&env);
        let vendor = Address::generate(&env);
        let other = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_admin = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);
        client.initialize(&admin);

        // Seed treasury with 1,000 USDC
        usdc_admin.mint(&contract_id, &1_000_0000000);

        // Create 4-period schedule of 200 USDC every 50 ledgers starting at ledger 150 (total 800 USDC reserved)
        let sched_id = client.create_schedule(
            &admin,
            &RecurringScheduleSpec {
                beneficiary: vendor.clone(),
                asset: usdc.clone(),
                amount_per_period: 200_0000000,
                period_ledgers: 50,
                total_periods: 4,
                start_ledger: 150,
            },
        );
        assert_eq!(client.reserved_balance(&usdc), 800_0000000);
        assert_eq!(client.uncommitted_balance(&usdc), 200_0000000);
        assert_eq!(client.next_payment_at(&sched_id), 150);

        // Direct withdrawal of 300 USDC fails because only 200 USDC is uncommitted
        assert_eq!(
            client.try_withdraw(&admin, &usdc, &other, &300_0000000),
            Err(Ok(TreasuryError::InsufficientUncommittedFunds))
        );

        // Early execution before ledger 150 fails
        assert_eq!(
            client.try_execute_schedule(&sched_id),
            Err(Ok(TreasuryError::ScheduleNotDueYet))
        );

        // Advance to ledger 210 -> periods at 150 and 200 are both due (2 periods = 400 USDC catch-up)
        env.ledger().with_mut(|li| li.sequence_number = 210);

        // While paused, execution is blocked
        client.set_paused(&admin, &true);
        assert_eq!(
            client.try_execute_schedule(&sched_id),
            Err(Ok(TreasuryError::ContractPaused))
        );
        client.set_paused(&admin, &false);

        let paid = client.execute_schedule(&sched_id);
        assert_eq!(paid, 400_0000000);
        assert_eq!(usdc_token.balance(&vendor), 400_0000000);
        assert_eq!(client.remaining_budget(&sched_id), 400_0000000);
        assert_eq!(client.next_payment_at(&sched_id), 250);

        // Cancel remaining 2 periods -> releases 400 USDC back to uncommitted balance
        let released = client.cancel_schedule(&admin, &sched_id);
        assert_eq!(released, 400_0000000);
        assert_eq!(client.reserved_balance(&usdc), 0);
        assert_eq!(client.uncommitted_balance(&usdc), 600_0000000);
        assert_eq!(usdc_token.balance(&vendor), 400_0000000);
    }
}
