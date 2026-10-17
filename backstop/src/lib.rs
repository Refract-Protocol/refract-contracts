#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
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
    InvalidGuardianConfig = 8,
    RecoveryAlreadyPending = 9,
    NoPendingRecovery = 10,
    InsufficientApprovals = 11,
    RecoveryDelayNotElapsed = 12,
    AlreadyApproved = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianSetConfig {
    pub guardians: Vec<Address>,
    pub threshold: u32,
    pub recovery_delay: u64,
    pub epoch: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryProposal {
    pub proposal_id: u64,
    pub new_admin: Address,
    pub approvals: Vec<Address>,
    pub threshold_reached_at: Option<u64>,
    pub guardian_epoch: u64,
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
        if env.storage().instance().has(&"config") {
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
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(&"next_prop_id", &1u64);
        Ok(())
    }

    pub fn configure_guardians(
        env: Env,
        caller: Address,
        guardians: Vec<Address>,
        threshold: u32,
        recovery_delay: u64,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if guardians.is_empty()
            || threshold == 0
            || threshold > guardians.len()
            || recovery_delay == 0
        {
            return Err(BackstopError::InvalidGuardianConfig);
        }
        let prev_epoch = env
            .storage()
            .instance()
            .get::<_, GuardianSetConfig>(&"guard_cfg")
            .map(|c| c.epoch)
            .unwrap_or(0);
        let cfg = GuardianSetConfig {
            guardians,
            threshold,
            recovery_delay,
            epoch: prev_epoch + 1,
        };
        env.storage().instance().set(&"guard_cfg", &cfg);
        env.storage().instance().remove(&"pending_rec");
        Ok(())
    }

    pub fn propose_recovery(
        env: Env,
        guardian: Address,
        new_admin: Address,
    ) -> Result<u64, BackstopError> {
        guardian.require_auth();
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&"guard_cfg")
            .ok_or(BackstopError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(BackstopError::Unauthorized);
        }
        if env.storage().instance().has(&"pending_rec") {
            return Err(BackstopError::RecoveryAlreadyPending);
        }
        let proposal_id: u64 = env.storage().instance().get(&"next_prop_id").unwrap_or(1);
        env.storage()
            .instance()
            .set(&"next_prop_id", &(proposal_id + 1));

        let mut approvals = Vec::new(&env);
        approvals.push_back(guardian);
        let threshold_reached_at = if approvals.len() >= cfg.threshold {
            Some(env.ledger().timestamp())
        } else {
            None
        };
        let proposal = RecoveryProposal {
            proposal_id,
            new_admin,
            approvals,
            threshold_reached_at,
            guardian_epoch: cfg.epoch,
        };
        env.storage().instance().set(&"pending_rec", &proposal);
        Ok(proposal_id)
    }

    pub fn approve_recovery(env: Env, guardian: Address) -> Result<u32, BackstopError> {
        guardian.require_auth();
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&"guard_cfg")
            .ok_or(BackstopError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(BackstopError::Unauthorized);
        }
        let mut proposal: RecoveryProposal = env
            .storage()
            .instance()
            .get(&"pending_rec")
            .ok_or(BackstopError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(BackstopError::NoPendingRecovery);
        }
        if proposal.approvals.contains(&guardian) {
            return Err(BackstopError::AlreadyApproved);
        }
        proposal.approvals.push_back(guardian);
        if proposal.approvals.len() >= cfg.threshold && proposal.threshold_reached_at.is_none() {
            proposal.threshold_reached_at = Some(env.ledger().timestamp());
        }
        let count = proposal.approvals.len();
        env.storage().instance().set(&"pending_rec", &proposal);
        Ok(count)
    }

    pub fn cancel_recovery(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if !env.storage().instance().has(&"pending_rec") {
            return Err(BackstopError::NoPendingRecovery);
        }
        env.storage().instance().remove(&"pending_rec");
        Ok(())
    }

    pub fn execute_recovery(env: Env) -> Result<Address, BackstopError> {
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&"guard_cfg")
            .ok_or(BackstopError::InvalidGuardianConfig)?;
        let proposal: RecoveryProposal = env
            .storage()
            .instance()
            .get(&"pending_rec")
            .ok_or(BackstopError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(BackstopError::NoPendingRecovery);
        }
        if proposal.approvals.len() < cfg.threshold {
            return Err(BackstopError::InsufficientApprovals);
        }
        let reached_at = proposal
            .threshold_reached_at
            .ok_or(BackstopError::InsufficientApprovals)?;
        if env.ledger().timestamp() < reached_at + cfg.recovery_delay {
            return Err(BackstopError::RecoveryDelayNotElapsed);
        }
        let mut config = Self::read_config(&env)?;
        let old_admin = config.admin.clone();
        let new_admin = proposal.new_admin.clone();
        config.admin = new_admin.clone();
        env.storage().instance().set(&"config", &config);
        env.storage().instance().remove(&"pending_rec");
        env.events().publish(
            (Symbol::new(&env, "admin_recovered"), proposal.proposal_id),
            (old_admin, new_admin.clone(), proposal.proposal_id),
        );
        Ok(new_admin)
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        Ok(amount)
    }

    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn backstop_guardian_recovery_and_guardian_set_rotation_invalidation() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let g1 = Address::generate(&env);
        let g2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        client.initialize(&pool, &admin, &token_id, &2_000);
        let mut guardians = Vec::new(&env);
        guardians.push_back(g1.clone());
        guardians.push_back(g2.clone());
        client.configure_guardians(&admin, &guardians, &2, &3_600);

        // Pending proposal invalidated by a guardian set update!
        client.propose_recovery(&g1, &new_admin);
        client.configure_guardians(&admin, &guardians, &2, &3_600);
        assert_eq!(
            client.try_approve_recovery(&g2),
            Err(Ok(BackstopError::NoPendingRecovery))
        );

        // Fresh proposal succeeds after delay.
        client.propose_recovery(&g1, &new_admin);
        client.approve_recovery(&g2);
        env.ledger().with_mut(|l| l.timestamp = 5_000);
        assert_eq!(client.execute_recovery(), new_admin);
        assert_eq!(client.config().admin, new_admin);
    }
}
