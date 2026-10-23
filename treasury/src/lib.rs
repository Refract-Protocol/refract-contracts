#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
};

const MAX_BATCH_OPS: u32 = 16;

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    NextScheduleId,
    Schedule(u64),
    Reserved(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentSchedule {
    pub recipient: Address,
    pub asset: Address,
    pub amount_per_period: i128,
    pub period_secs: u64,
    pub start: u64,
    pub end: u64,
    pub total_budget: i128,
    pub paid: i128,
    pub paid_periods: u64,
    pub cancelled: bool,
}

#[contracttype]
#[derive(Clone, Debug)]
pub enum TreasuryOp {
    Withdraw {
        token: Address,
        amount: i128,
        to: Address,
    },
    CancelSchedule(u64),
    SetAdmin(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    InvalidSchedule = 4,
    InsufficientReserve = 5,
    TooManyOperations = 6,
    NoExcessFunds = 7,
}

/// `RefractTreasury` is a deliberately minimal token-custody vault.
///
/// Funds are deposited via plain token transfers to the contract address
/// (no `deposit` entrypoint is required). Withdrawals are admin-gated.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextScheduleId, &0u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        Self::require_admin(&env, &caller);
        Self::withdraw_unchecked(&env, &token, amount, &to);
    }

    pub fn create_schedule(
        env: Env,
        caller: Address,
        recipient: Address,
        asset: Address,
        amount_per_period: i128,
        period_secs: u64,
        start: u64,
        end: u64,
        total_budget: i128,
    ) -> Result<u64, TreasuryError> {
        caller.require_auth();
        Self::require_admin_result(&env, &caller)?;
        if amount_per_period <= 0
            || period_secs == 0
            || end < start
            || total_budget <= 0
            || total_budget < amount_per_period
        {
            return Err(TreasuryError::InvalidSchedule);
        }

        let reserved = Self::reserved(&env, &asset);
        let balance = token::Client::new(&env, &asset).balance(&env.current_contract_address());
        if balance < reserved + total_budget {
            return Err(TreasuryError::InsufficientReserve);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextScheduleId)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::NextScheduleId, &(id + 1));
        let schedule = PaymentSchedule {
            recipient,
            asset: asset.clone(),
            amount_per_period,
            period_secs,
            start,
            end,
            total_budget,
            paid: 0,
            paid_periods: 0,
            cancelled: false,
        };
        env.storage().persistent().set(&DataKey::Schedule(id), &schedule);
        env.storage()
            .persistent()
            .set(&DataKey::Reserved(asset), &(reserved + total_budget));
        env.events().publish(
            (Symbol::new(&env, "ScheduleCreated"),),
            (id, schedule.recipient, schedule.asset, total_budget),
        );
        Ok(id)
    }

    pub fn execute_schedule(env: Env, schedule_id: u64) -> Result<i128, TreasuryError> {
        let mut schedule: PaymentSchedule = env
            .storage()
            .persistent()
            .get(&DataKey::Schedule(schedule_id))
            .ok_or(TreasuryError::InvalidSchedule)?;
        let now = env.ledger().timestamp();
        if schedule.cancelled || now < schedule.start || now > schedule.end {
            return Err(TreasuryError::InvalidSchedule);
        }
        let elapsed = (now - schedule.start) / schedule.period_secs;
        if elapsed < schedule.paid_periods {
            return Err(TreasuryError::InvalidSchedule);
        }
        let remaining = schedule.total_budget - schedule.paid;
        if remaining <= 0 {
            return Err(TreasuryError::InvalidSchedule);
        }
        let amount = schedule.amount_per_period.min(remaining);
        Self::withdraw_unchecked(&env, &schedule.asset, amount, &schedule.recipient);
        schedule.paid += amount;
        schedule.paid_periods += 1;
        env.storage().persistent().set(&DataKey::Schedule(schedule_id), &schedule);
        env.events().publish(
            (Symbol::new(&env, "ScheduleExecuted"),),
            (schedule_id, amount, schedule.paid),
        );
        if schedule.paid == schedule.total_budget {
            Self::release_reserve(&env, &schedule.asset, schedule.total_budget);
            env.events()
                .publish((Symbol::new(&env, "ScheduleExhausted"),), (schedule_id,));
        }
        Ok(amount)
    }

    pub fn cancel_schedule(env: Env, caller: Address, schedule_id: u64) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin_result(&env, &caller)?;
        let mut schedule: PaymentSchedule = env
            .storage()
            .persistent()
            .get(&DataKey::Schedule(schedule_id))
            .ok_or(TreasuryError::InvalidSchedule)?;
        if schedule.cancelled {
            return Err(TreasuryError::InvalidSchedule);
        }
        let remaining = schedule.total_budget - schedule.paid;
        schedule.cancelled = true;
        env.storage().persistent().set(&DataKey::Schedule(schedule_id), &schedule);
        Self::release_reserve(&env, &schedule.asset, remaining);
        env.events().publish(
            (Symbol::new(&env, "ScheduleCancelled"),),
            (schedule_id, remaining),
        );
        Ok(())
    }

    pub fn schedule(env: Env, schedule_id: u64) -> Option<PaymentSchedule> {
        env.storage().persistent().get(&DataKey::Schedule(schedule_id))
    }

    pub fn next_payment_at(env: Env, schedule_id: u64) -> Option<u64> {
        Self::schedule(&env, schedule_id).map(|s| s.start + s.paid_periods * s.period_secs)
    }

    pub fn remaining_budget(env: Env, schedule_id: u64) -> Option<i128> {
        Self::schedule(&env, schedule_id).map(|s| s.total_budget - s.paid)
    }

    pub fn accounted_liabilities(env: Env, token: Address) -> i128 {
        Self::reserved(&env, &token)
    }

    pub fn rescue(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        Self::require_admin_result(&env, &caller)?;
        if to != caller || amount <= 0 {
            return Err(TreasuryError::Unauthorized);
        }
        let balance = token::Client::new(&env, &token).balance(&env.current_contract_address());
        let excess = balance - Self::reserved(&env, &token);
        if amount > excess {
            return Err(TreasuryError::NoExcessFunds);
        }
        Self::withdraw_unchecked(&env, &token, amount, &to);
        env.events().publish(
            (Symbol::new(&env, "FundsRescued"),),
            (token, to, amount, excess - amount),
        );
        Ok(excess - amount)
    }

    pub fn batch(env: Env, caller: Address, operations: Vec<TreasuryOp>) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin_result(&env, &caller)?;
        if operations.len() > MAX_BATCH_OPS {
            return Err(TreasuryError::TooManyOperations);
        }
        for operation in operations.iter() {
            match operation {
                TreasuryOp::Withdraw { token, amount, to } => {
                    Self::withdraw_unchecked(&env, &token, amount, &to)
                }
                TreasuryOp::CancelSchedule(id) => {
                    let mut schedule: PaymentSchedule = env
                        .storage()
                        .persistent()
                        .get(&DataKey::Schedule(id))
                        .ok_or(TreasuryError::InvalidSchedule)?;
                    if schedule.cancelled {
                        return Err(TreasuryError::InvalidSchedule);
                    }
                    let remaining = schedule.total_budget - schedule.paid;
                    schedule.cancelled = true;
                    env.storage().persistent().set(&DataKey::Schedule(id), &schedule);
                    Self::release_reserve(&env, &schedule.asset, remaining);
                }
                TreasuryOp::SetAdmin(admin) => env.storage().instance().set(&DataKey::Admin, &admin),
            }
        }
        env.events().publish(
            (Symbol::new(&env, "AdminBatch"),),
            operations.len(),
        );
        Ok(())
    }

    fn require_admin(env: &Env, caller: &Address) {
        if Self::require_admin_result(env, caller).is_err() {
            panic!("unauthorized");
        }
    }

    fn require_admin_result(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if *caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        Ok(())
    }

    fn withdraw_unchecked(env: &Env, token: &Address, amount: i128, to: &Address) {
        if amount <= 0 {
            panic!("amount must be positive");
        }
        token::Client::new(env, token).transfer(&env.current_contract_address(), to, &amount);
    }

    fn reserved(env: &Env, token: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Reserved(token.clone()))
            .unwrap_or(0)
    }

    fn release_reserve(env: &Env, token: &Address, amount: i128) {
        let reserved = Self::reserved(env, token);
        env.storage()
            .persistent()
            .set(&DataKey::Reserved(token.clone()), &(reserved - amount));
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract(token_admin.clone());
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }
}
