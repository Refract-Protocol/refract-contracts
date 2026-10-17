//! # RefractBackstop
//!
//! Second-loss reinsurance/backstop capital tranche with committed capital
//! facilities (`commit`, `cancel_commitment`) and governance-triggered
//! pro-rata capital calls (`call_capital`, `fund_call`, `settle_defaults`).

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

const BPS_DENOM: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    InvalidCommitment = 8,
    CommitmentNotFound = 9,
    ActiveCallBlocksCancel = 10,
    InsufficientCommittedCapacity = 11,
    CallNotFound = 12,
    DuplicateCallId = 13,
    CallDeadlinePassed = 14,
    CallDeadlineNotReached = 15,
    ObligationAlreadyFunded = 16,
    DefaultsAlreadySettled = 17,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    Shares(Address),
    Commitment(Address),
    ProviderList,
    FacilitySummary,
    CapitalCall(u64),
    CallObligation(u64, Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitmentRecord {
    pub provider: Address,
    pub committed_amount: i128,
    pub bond_amount: i128,
    pub expiry_ledger: u32,
    pub active_calls: u32,
    pub active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FacilitySummary {
    pub total_committed: i128,
    pub total_called: i128,
    pub total_funded: i128,
    pub total_defaulted: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapitalCall {
    pub call_id: u64,
    pub total_amount: i128,
    pub funded_amount: i128,
    pub funding_deadline: u32,
    pub settled: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderObligation {
    pub call_id: u64,
    pub provider: Address,
    pub amount_due: i128,
    pub funded: bool,
    pub defaulted: bool,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool,
            admin,
            token,
            premium_share_bps,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(
            &DataKey::FacilitySummary,
            &FacilitySummary {
                total_committed: 0,
                total_called: 0,
                total_funded: 0,
                total_defaulted: 0,
            },
        );
        Ok(())
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );
        Self::credit_stake(&env, &staker, amount)
    }

    /// Commits a notional capital amount with an optional refundable good-faith bond.
    /// Committed-but-uncalled capital is never counted in `total_capital`.
    pub fn commit(
        env: Env,
        provider: Address,
        amount: i128,
        expiry_ledger: u32,
        bond_amount: i128,
    ) -> Result<CommitmentRecord, BackstopError> {
        provider.require_auth();
        let config = Self::read_config(&env)?;
        if amount <= 0 || bond_amount < 0 || expiry_ledger <= env.ledger().sequence() {
            return Err(BackstopError::InvalidCommitment);
        }

        if bond_amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &provider,
                &env.current_contract_address(),
                &bond_amount,
            );
        }

        let existing = env
            .storage()
            .persistent()
            .get::<DataKey, CommitmentRecord>(&DataKey::Commitment(provider.clone()));
        let prev_amt = existing
            .as_ref()
            .filter(|c| c.active)
            .map(|c| c.committed_amount)
            .unwrap_or(0);
        let prev_bond = existing
            .as_ref()
            .filter(|c| c.active)
            .map(|c| c.bond_amount)
            .unwrap_or(0);
        let prev_calls = existing
            .as_ref()
            .filter(|c| c.active)
            .map(|c| c.active_calls)
            .unwrap_or(0);

        let record = CommitmentRecord {
            provider: provider.clone(),
            committed_amount: prev_amt + amount,
            bond_amount: prev_bond + bond_amount,
            expiry_ledger,
            active_calls: prev_calls,
            active: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Commitment(provider.clone()), &record);

        let mut providers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::ProviderList)
            .unwrap_or_else(|| Vec::new(&env));
        if !providers.contains(&provider) {
            providers.push_back(provider.clone());
            env.storage()
                .instance()
                .set(&DataKey::ProviderList, &providers);
        }

        let mut summary = Self::read_summary(&env);
        summary.total_committed += amount;
        env.storage()
            .instance()
            .set(&DataKey::FacilitySummary, &summary);

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("commit")),
            (provider, amount, expiry_ledger, bond_amount),
        );
        Ok(record)
    }

    /// Cancels a live commitment before any active call (or after expiry) and refunds its bond.
    pub fn cancel_commitment(env: Env, provider: Address) -> Result<i128, BackstopError> {
        provider.require_auth();
        let config = Self::read_config(&env)?;
        let mut record: CommitmentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Commitment(provider.clone()))
            .ok_or(BackstopError::CommitmentNotFound)?;
        if !record.active {
            return Err(BackstopError::CommitmentNotFound);
        }
        if record.active_calls > 0 {
            return Err(BackstopError::ActiveCallBlocksCancel);
        }

        let mut summary = Self::read_summary(&env);
        summary.total_committed = summary
            .total_committed
            .saturating_sub(record.committed_amount);
        env.storage()
            .instance()
            .set(&DataKey::FacilitySummary, &summary);

        let refund_bond = record.bond_amount;
        record.committed_amount = 0;
        record.bond_amount = 0;
        record.active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Commitment(provider.clone()), &record);

        if refund_bond > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &provider,
                &refund_bond,
            );
        }
        Ok(refund_bond)
    }

    /// Governance-triggered pro-rata capital call against live, unexpired commitments.
    /// Deterministic dust rule: any 1-stroop rounding remainder is assigned to the last eligible provider.
    pub fn call_capital(
        env: Env,
        caller: Address,
        call_id: u64,
        total_amount: i128,
        funding_deadline: u32,
    ) -> Result<CapitalCall, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let now = env.ledger().sequence();
        if total_amount <= 0 || funding_deadline <= now {
            return Err(BackstopError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::CapitalCall(call_id))
        {
            return Err(BackstopError::DuplicateCallId);
        }

        let providers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::ProviderList)
            .unwrap_or_else(|| Vec::new(&env));

        let mut eligible: Vec<CommitmentRecord> = Vec::new(&env);
        let mut eligible_sum: i128 = 0;
        for addr in providers.iter() {
            if let Some(rec) = env
                .storage()
                .persistent()
                .get::<DataKey, CommitmentRecord>(&DataKey::Commitment(addr))
            {
                if rec.active && rec.committed_amount > 0 && rec.expiry_ledger > now {
                    eligible_sum += rec.committed_amount;
                    eligible.push_back(rec);
                }
            }
        }

        if eligible.is_empty() || total_amount > eligible_sum {
            return Err(BackstopError::InsufficientCommittedCapacity);
        }

        let mut allocated_so_far: i128 = 0;
        let count = eligible.len();
        for idx in 0..count {
            let mut rec = eligible.get(idx).unwrap();
            let due = if idx + 1 == count {
                total_amount - allocated_so_far
            } else {
                total_amount * rec.committed_amount / eligible_sum
            };
            allocated_so_far += due;
            rec.active_calls += 1;
            env.storage()
                .persistent()
                .set(&DataKey::Commitment(rec.provider.clone()), &rec);

            let ob = ProviderObligation {
                call_id,
                provider: rec.provider.clone(),
                amount_due: due,
                funded: false,
                defaulted: false,
            };
            env.storage()
                .persistent()
                .set(&DataKey::CallObligation(call_id, rec.provider), &ob);
        }

        let mut summary = Self::read_summary(&env);
        summary.total_called += total_amount;
        env.storage()
            .instance()
            .set(&DataKey::FacilitySummary, &summary);

        let call = CapitalCall {
            call_id,
            total_amount,
            funded_amount: 0,
            funding_deadline,
            settled: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::CapitalCall(call_id), &call);
        Ok(call)
    }

    /// Provider funds their called obligation before `funding_deadline`; funds convert into backstop shares.
    pub fn fund_call(env: Env, provider: Address, call_id: u64) -> Result<i128, BackstopError> {
        provider.require_auth();
        let config = Self::read_config(&env)?;
        let mut call: CapitalCall = env
            .storage()
            .persistent()
            .get(&DataKey::CapitalCall(call_id))
            .ok_or(BackstopError::CallNotFound)?;
        if env.ledger().sequence() > call.funding_deadline {
            return Err(BackstopError::CallDeadlinePassed);
        }

        let mut ob: ProviderObligation = env
            .storage()
            .persistent()
            .get(&DataKey::CallObligation(call_id, provider.clone()))
            .ok_or(BackstopError::CommitmentNotFound)?;
        if ob.funded {
            return Err(BackstopError::ObligationAlreadyFunded);
        }

        token::Client::new(&env, &config.token).transfer(
            &provider,
            &env.current_contract_address(),
            &ob.amount_due,
        );
        let minted_shares = Self::credit_stake(&env, &provider, ob.amount_due)?;

        ob.funded = true;
        env.storage()
            .persistent()
            .set(&DataKey::CallObligation(call_id, provider.clone()), &ob);

        call.funded_amount += ob.amount_due;
        env.storage()
            .persistent()
            .set(&DataKey::CapitalCall(call_id), &call);

        let mut rec: CommitmentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Commitment(provider.clone()))
            .ok_or(BackstopError::CommitmentNotFound)?;
        rec.committed_amount = rec.committed_amount.saturating_sub(ob.amount_due);
        rec.active_calls = rec.active_calls.saturating_sub(1);
        env.storage()
            .persistent()
            .set(&DataKey::Commitment(provider), &rec);

        let mut summary = Self::read_summary(&env);
        summary.total_committed = summary.total_committed.saturating_sub(ob.amount_due);
        summary.total_funded += ob.amount_due;
        env.storage()
            .instance()
            .set(&DataKey::FacilitySummary, &summary);

        Ok(minted_shares)
    }

    /// Settles defaults after `funding_deadline`, slashing defaulting providers' good-faith bonds into backstop capital.
    pub fn settle_defaults(env: Env, call_id: u64) -> Result<i128, BackstopError> {
        let mut call: CapitalCall = env
            .storage()
            .persistent()
            .get(&DataKey::CapitalCall(call_id))
            .ok_or(BackstopError::CallNotFound)?;
        if call.settled {
            return Err(BackstopError::DefaultsAlreadySettled);
        }
        if env.ledger().sequence() <= call.funding_deadline {
            return Err(BackstopError::CallDeadlineNotReached);
        }

        let providers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::ProviderList)
            .unwrap_or_else(|| Vec::new(&env));

        let mut summary = Self::read_summary(&env);
        let mut state = Self::read_state(&env)?;
        let mut total_penalties: i128 = 0;

        for addr in providers.iter() {
            if let Some(mut ob) = env
                .storage()
                .persistent()
                .get::<DataKey, ProviderObligation>(&DataKey::CallObligation(call_id, addr.clone()))
            {
                if !ob.funded && !ob.defaulted {
                    ob.defaulted = true;
                    env.storage()
                        .persistent()
                        .set(&DataKey::CallObligation(call_id, addr.clone()), &ob);

                    if let Some(mut rec) = env
                        .storage()
                        .persistent()
                        .get::<DataKey, CommitmentRecord>(&DataKey::Commitment(addr.clone()))
                    {
                        let penalty = rec.bond_amount;
                        rec.bond_amount = 0;
                        rec.committed_amount = rec.committed_amount.saturating_sub(ob.amount_due);
                        rec.active_calls = rec.active_calls.saturating_sub(1);
                        rec.active = false;
                        env.storage()
                            .persistent()
                            .set(&DataKey::Commitment(addr), &rec);

                        summary.total_committed =
                            summary.total_committed.saturating_sub(ob.amount_due);
                        summary.total_defaulted += ob.amount_due;
                        state.total_capital += penalty;
                        total_penalties += penalty;
                    }
                }
            }
        }

        call.settled = true;
        env.storage()
            .persistent()
            .set(&DataKey::CapitalCall(call_id), &call);
        env.storage()
            .instance()
            .set(&DataKey::FacilitySummary, &summary);
        env.storage().instance().set(&DataKey::State, &state);
        Ok(total_penalties)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn facility_summary(env: Env) -> FacilitySummary {
        Self::read_summary(&env)
    }

    pub fn provider_commitment(env: Env, provider: Address) -> Option<CommitmentRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Commitment(provider))
    }

    pub fn provider_call_obligation(
        env: Env,
        call_id: u64,
        provider: Address,
    ) -> Option<ProviderObligation> {
        env.storage()
            .persistent()
            .get(&DataKey::CallObligation(call_id, provider))
    }

    fn credit_stake(env: &Env, staker: &Address, amount: i128) -> Result<i128, BackstopError> {
        let mut state = Self::read_state(env)?;
        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let prev: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(staker.clone()), &(prev + shares));
        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);
        Ok(shares)
    }

    fn read_summary(env: &Env) -> FacilitySummary {
        env.storage()
            .instance()
            .get(&DataKey::FacilitySummary)
            .unwrap_or(FacilitySummary {
                total_committed: 0,
                total_called: 0,
                total_funded: 0,
                total_defaulted: 0,
            })
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    #[test]
    fn committed_capital_calls_pro_rata_converts_funded_to_stake_and_penalizes_defaults() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let p1 = Address::generate(&env);
        let p2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&p1, &10_000);
        asset.mint(&p2, &10_000);

        let cid = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &cid);

        client.initialize(&pool, &admin, &token_id, &5_000);

        // p1 commits 3_000 with 300 bond; p2 commits 1_000 with 100 bond
        client.commit(&p1, &3_000, &20, &300);
        client.commit(&p2, &1_000, &20, &100);

        // Committed capital is not counted as liquid total_capital
        assert_eq!(client.state().total_capital, 0);
        assert_eq!(client.facility_summary().total_committed, 4_000);

        // Issue pro-rata capital call for 2_000 (p1 due 1_500, p2 due 500)
        client.call_capital(&admin, &1, &2_000, &10);
        assert_eq!(
            client.provider_call_obligation(&1, &p1).unwrap().amount_due,
            1_500
        );
        assert_eq!(
            client.provider_call_obligation(&1, &p2).unwrap().amount_due,
            500
        );

        // Active call blocks commitment cancellation
        assert_eq!(
            client.try_cancel_commitment(&p1),
            Err(Ok(BackstopError::ActiveCallBlocksCancel))
        );

        // p1 funds their 1_500 call -> converts into 1_500 backstop shares & capital
        assert_eq!(client.fund_call(&p1, &1), 1_500);
        assert_eq!(client.shares_of(&p1), 1_500);
        assert_eq!(client.state().total_capital, 1_500);

        // Advance past deadline (ledger 11) and settle p2's default -> slashes p2's 100 bond into capital
        env.ledger().with_mut(|l| l.sequence_number = 11);
        assert_eq!(client.settle_defaults(&1), 100);
        assert_eq!(client.state().total_capital, 1_600);
        let sum = client.facility_summary();
        assert_eq!(sum.total_funded, 1_500);
        assert_eq!(sum.total_defaulted, 500);
    }
}
