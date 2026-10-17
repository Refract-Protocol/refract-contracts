//! # RefractBackstop
//!
//! Second-loss reinsurance/backstop capital tranche requiring an on-chain
//! loss-event declaration (`declare_loss_event`), multi-attester verification
//! (`attest_loss_event`) within tolerance bounds, and a timelock delay before
//! any capital drawdown (`drawdown`).

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
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
    LossEventNotFound = 8,
    InvalidLossEventState = 9,
    DuplicateAttestation = 10,
    AttestationOutOfTolerance = 11,
    StaleOracleRound = 12,
    ExecutionDelayNotElapsed = 13,
    DrawdownExceedsAttested = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    Shares(Address),
    LossWorkflowConfig,
    NextLossEventId,
    LossEvent(u64),
    OutstandingAttestedCapacity,
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
pub struct LossWorkflowConfig {
    pub guardian: Address,
    pub attesters: Vec<Address>,
    pub attester_threshold: u32,
    pub tolerance_bps: u32,
    pub execution_delay_ledgers: u32,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum LossEventStatus {
    Declared = 0,
    Attested = 1,
    Executed = 2,
    Cancelled = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossEvent {
    pub id: u64,
    pub pool: Address,
    pub declared_amount: i128,
    pub attested_amount: i128,
    pub drawn_amount: i128,
    pub evidence_hash: BytesN<32>,
    pub oracle_round: u64,
    pub attesters: Vec<Address>,
    pub executable_at_ledger: u32,
    pub status: LossEventStatus,
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
        env.storage()
            .instance()
            .set(&DataKey::NextLossEventId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::OutstandingAttestedCapacity, &0i128);
        Ok(())
    }

    pub fn set_loss_workflow_config(
        env: Env,
        guardian: Address,
        attesters: Vec<Address>,
        attester_threshold: u32,
        tolerance_bps: u32,
        execution_delay_ledgers: u32,
    ) -> Result<LossWorkflowConfig, BackstopError> {
        let cfg = Self::read_config(&env)?;
        cfg.admin.require_auth();
        if attesters.is_empty()
            || attester_threshold == 0
            || attester_threshold > attesters.len()
            || tolerance_bps as i128 > BPS_DENOM
        {
            return Err(BackstopError::InvalidAmount);
        }
        let wf = LossWorkflowConfig {
            guardian,
            attesters,
            attester_threshold,
            tolerance_bps,
            execution_delay_ledgers,
        };
        env.storage()
            .instance()
            .set(&DataKey::LossWorkflowConfig, &wf);
        Ok(wf)
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let prev: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(staker), &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);
        Ok(shares)
    }

    /// Declares a pool shortfall loss event on-chain. Callable by the guardian, pool, or admin.
    pub fn declare_loss_event(
        env: Env,
        caller: Address,
        pool: Address,
        amount: i128,
        evidence_hash: BytesN<32>,
        oracle_round: u64,
    ) -> Result<u64, BackstopError> {
        caller.require_auth();
        let cfg = Self::read_config(&env)?;
        let wf = Self::read_workflow(&env)?;
        if caller != wf.guardian && caller != cfg.pool && caller != cfg.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 || oracle_round == 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextLossEventId)
            .unwrap_or(0);
        let ev = LossEvent {
            id,
            pool: pool.clone(),
            declared_amount: amount,
            attested_amount: amount,
            drawn_amount: 0,
            evidence_hash: evidence_hash.clone(),
            oracle_round,
            attesters: Vec::new(&env),
            executable_at_ledger: 0,
            status: LossEventStatus::Declared,
        };
        env.storage().persistent().set(&DataKey::LossEvent(id), &ev);
        env.storage()
            .instance()
            .set(&DataKey::NextLossEventId, &(id + 1));

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("loss_dec")),
            (id, pool, amount, evidence_hash, oracle_round),
        );
        Ok(id)
    }

    /// Attests a declared loss event. Requires `attested_amount` within `tolerance_bps` of
    /// `declared_amount` and an oracle round at least as fresh as `ev.oracle_round`.
    pub fn attest_loss_event(
        env: Env,
        attester: Address,
        event_id: u64,
        attested_amount: i128,
        oracle_round: u64,
    ) -> Result<LossEvent, BackstopError> {
        attester.require_auth();
        let wf = Self::read_workflow(&env)?;
        if !wf.attesters.contains(&attester) {
            return Err(BackstopError::Unauthorized);
        }

        let mut ev: LossEvent = env
            .storage()
            .persistent()
            .get(&DataKey::LossEvent(event_id))
            .ok_or(BackstopError::LossEventNotFound)?;
        if ev.status != LossEventStatus::Declared {
            return Err(BackstopError::InvalidLossEventState);
        }
        if ev.attesters.contains(&attester) {
            return Err(BackstopError::DuplicateAttestation);
        }
        if oracle_round < ev.oracle_round {
            return Err(BackstopError::StaleOracleRound);
        }

        let diff = (attested_amount - ev.declared_amount).abs();
        let max_diff = ev.declared_amount * (wf.tolerance_bps as i128) / BPS_DENOM;
        if attested_amount <= 0 || diff > max_diff {
            return Err(BackstopError::AttestationOutOfTolerance);
        }

        if attested_amount < ev.attested_amount {
            ev.attested_amount = attested_amount;
        }
        ev.attesters.push_back(attester.clone());

        if ev.attesters.len() >= wf.attester_threshold {
            ev.status = LossEventStatus::Attested;
            ev.executable_at_ledger = env
                .ledger()
                .sequence()
                .saturating_add(wf.execution_delay_ledgers);

            let out: i128 = env
                .storage()
                .instance()
                .get(&DataKey::OutstandingAttestedCapacity)
                .unwrap_or(0);
            env.storage().instance().set(
                &DataKey::OutstandingAttestedCapacity,
                &(out + ev.attested_amount),
            );
        }

        env.storage()
            .persistent()
            .set(&DataKey::LossEvent(event_id), &ev);

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("loss_att")),
            (event_id, attester, ev.attested_amount, ev.status),
        );
        Ok(ev)
    }

    /// Cancels a `Declared` or `Attested` loss event before full execution.
    pub fn cancel_loss_event(
        env: Env,
        caller: Address,
        event_id: u64,
    ) -> Result<LossEvent, BackstopError> {
        caller.require_auth();
        let cfg = Self::read_config(&env)?;
        let wf = Self::read_workflow(&env)?;
        if caller != cfg.admin && caller != wf.guardian {
            return Err(BackstopError::Unauthorized);
        }
        let mut ev: LossEvent = env
            .storage()
            .persistent()
            .get(&DataKey::LossEvent(event_id))
            .ok_or(BackstopError::LossEventNotFound)?;
        if ev.status != LossEventStatus::Declared && ev.status != LossEventStatus::Attested {
            return Err(BackstopError::InvalidLossEventState);
        }
        if ev.status == LossEventStatus::Attested {
            let remaining = ev.attested_amount - ev.drawn_amount;
            let out: i128 = env
                .storage()
                .instance()
                .get(&DataKey::OutstandingAttestedCapacity)
                .unwrap_or(0);
            env.storage().instance().set(
                &DataKey::OutstandingAttestedCapacity,
                &out.saturating_sub(remaining),
            );
        }
        ev.status = LossEventStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::LossEvent(event_id), &ev);
        Ok(ev)
    }

    /// Draws down backstop capital against an `Attested` loss event after the timelock delay.
    /// Supports incremental partial draws and transitions to `Executed` when fully drawn.
    pub fn drawdown(
        env: Env,
        caller: Address,
        event_id: u64,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut ev: LossEvent = env
            .storage()
            .persistent()
            .get(&DataKey::LossEvent(event_id))
            .ok_or(BackstopError::LossEventNotFound)?;
        if ev.status != LossEventStatus::Attested {
            return Err(BackstopError::InvalidLossEventState);
        }
        if env.ledger().sequence() < ev.executable_at_ledger {
            return Err(BackstopError::ExecutionDelayNotElapsed);
        }

        let remaining_attested = ev.attested_amount - ev.drawn_amount;
        if amount > remaining_attested {
            return Err(BackstopError::DrawdownExceedsAttested);
        }

        let mut state = Self::read_state(&env)?;
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        state.total_capital -= amount;
        env.storage().instance().set(&DataKey::State, &state);

        ev.drawn_amount += amount;
        if ev.drawn_amount == ev.attested_amount {
            ev.status = LossEventStatus::Executed;
        }
        env.storage()
            .persistent()
            .set(&DataKey::LossEvent(event_id), &ev);

        let out: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OutstandingAttestedCapacity)
            .unwrap_or(0);
        env.storage().instance().set(
            &DataKey::OutstandingAttestedCapacity,
            &out.saturating_sub(amount),
        );

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &ev.pool,
            &amount,
        );

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("drawdown")),
            (event_id, ev.pool, amount, ev.drawn_amount, ev.status),
        );
        Ok(amount)
    }

    pub fn loss_event(env: Env, event_id: u64) -> Option<LossEvent> {
        env.storage()
            .persistent()
            .get(&DataKey::LossEvent(event_id))
    }

    pub fn outstanding_attested_capacity(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::OutstandingAttestedCapacity)
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    fn read_workflow(env: &Env) -> Result<LossWorkflowConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::LossWorkflowConfig)
            .ok_or(BackstopError::NotInitialized)
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
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        vec,
    };

    #[test]
    fn loss_event_workflow_enforces_attestation_tolerance_delay_and_partial_drawdowns() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let guardian = Address::generate(&env);
        let att1 = Address::generate(&env);
        let att2 = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&staker, &10_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &cid);

        client.initialize(&pool, &admin, &token_id, &5_000);
        client.set_loss_workflow_config(
            &guardian,
            &vec![&env, att1.clone(), att2.clone()],
            &2,
            &500, // 5% tolerance
            &10,  // 10-ledger execution delay
        );
        client.deposit(&staker, &5_000);

        let eid = client.declare_loss_event(
            &guardian,
            &pool,
            &2_000,
            &BytesN::from_array(&env, &[7u8; 32]),
            &100,
        );

        // Out-of-tolerance attestation (2_500 vs 2_000 > 5%) is rejected
        assert_eq!(
            client.try_attest_loss_event(&att1, &eid, &2_500, &100),
            Err(Ok(BackstopError::AttestationOutOfTolerance))
        );

        // First valid attestation (1_950) + duplicate rejection
        client.attest_loss_event(&att1, &eid, &1_950, &100);
        assert_eq!(
            client.try_attest_loss_event(&att1, &eid, &1_950, &100),
            Err(Ok(BackstopError::DuplicateAttestation))
        );

        // Second valid attestation reaches threshold -> status = Attested, executable at ledger 10
        let attested = client.attest_loss_event(&att2, &eid, &2_000, &101);
        assert_eq!(attested.status, LossEventStatus::Attested);
        assert_eq!(attested.attested_amount, 1_950);
        assert_eq!(client.outstanding_attested_capacity(), 1_950);

        // Drawdown before delay elapses fails
        assert_eq!(
            client.try_drawdown(&pool, &eid, &1_000),
            Err(Ok(BackstopError::ExecutionDelayNotElapsed))
        );

        // Advance to ledger 10 -> partial draw of 1_000, then remaining 950 -> Executed
        env.ledger().with_mut(|l| l.sequence_number = 10);
        assert_eq!(client.drawdown(&pool, &eid, &1_000), 1_000);
        assert_eq!(
            client.try_drawdown(&pool, &eid, &1_000),
            Err(Ok(BackstopError::DrawdownExceedsAttested))
        );
        assert_eq!(client.drawdown(&pool, &eid, &950), 950);
        assert_eq!(
            client.loss_event(&eid).unwrap().status,
            LossEventStatus::Executed
        );
        assert_eq!(tok.balance(&pool), 1_950);
        assert_eq!(client.outstanding_attested_capacity(), 0);
    }
}
