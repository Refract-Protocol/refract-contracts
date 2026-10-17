#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Symbol, Vec,
};

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

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
    GuardianConfig,
    PendingRecovery,
    NextProposalId,
}

#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    InvalidGuardianConfig = 7,
    RecoveryAlreadyPending = 8,
    NoPendingRecovery = 9,
    InsufficientApprovals = 10,
    RecoveryDelayNotElapsed = 11,
    AlreadyApproved = 12,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &1u64);
        Ok(())
    }

    pub fn configure_guardians(
        env: Env,
        caller: Address,
        guardians: Vec<Address>,
        threshold: u32,
        recovery_delay: u64,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        if guardians.is_empty()
            || threshold == 0
            || threshold > guardians.len()
            || recovery_delay == 0
        {
            return Err(BountyError::InvalidGuardianConfig);
        }
        let prev_epoch = env
            .storage()
            .instance()
            .get::<DataKey, GuardianSetConfig>(&DataKey::GuardianConfig)
            .map(|c| c.epoch)
            .unwrap_or(0);
        let cfg = GuardianSetConfig {
            guardians,
            threshold,
            recovery_delay,
            epoch: prev_epoch + 1,
        };
        env.storage().instance().set(&DataKey::GuardianConfig, &cfg);
        env.storage().instance().remove(&DataKey::PendingRecovery);
        Ok(())
    }

    pub fn propose_recovery(
        env: Env,
        guardian: Address,
        new_admin: Address,
    ) -> Result<u64, BountyError> {
        guardian.require_auth();
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&DataKey::GuardianConfig)
            .ok_or(BountyError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(BountyError::Unauthorized);
        }
        if env.storage().instance().has(&DataKey::PendingRecovery) {
            return Err(BountyError::RecoveryAlreadyPending);
        }
        let proposal_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProposalId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &(proposal_id + 1));

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
        env.storage()
            .instance()
            .set(&DataKey::PendingRecovery, &proposal);
        Ok(proposal_id)
    }

    pub fn approve_recovery(env: Env, guardian: Address) -> Result<u32, BountyError> {
        guardian.require_auth();
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&DataKey::GuardianConfig)
            .ok_or(BountyError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(BountyError::Unauthorized);
        }
        let mut proposal: RecoveryProposal = env
            .storage()
            .instance()
            .get(&DataKey::PendingRecovery)
            .ok_or(BountyError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(BountyError::NoPendingRecovery);
        }
        if proposal.approvals.contains(&guardian) {
            return Err(BountyError::AlreadyApproved);
        }
        proposal.approvals.push_back(guardian);
        if proposal.approvals.len() >= cfg.threshold && proposal.threshold_reached_at.is_none() {
            proposal.threshold_reached_at = Some(env.ledger().timestamp());
        }
        let count = proposal.approvals.len();
        env.storage()
            .instance()
            .set(&DataKey::PendingRecovery, &proposal);
        Ok(count)
    }

    pub fn cancel_recovery(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        if !env.storage().instance().has(&DataKey::PendingRecovery) {
            return Err(BountyError::NoPendingRecovery);
        }
        env.storage().instance().remove(&DataKey::PendingRecovery);
        Ok(())
    }

    pub fn execute_recovery(env: Env) -> Result<Address, BountyError> {
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&DataKey::GuardianConfig)
            .ok_or(BountyError::InvalidGuardianConfig)?;
        let proposal: RecoveryProposal = env
            .storage()
            .instance()
            .get(&DataKey::PendingRecovery)
            .ok_or(BountyError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(BountyError::NoPendingRecovery);
        }
        if proposal.approvals.len() < cfg.threshold {
            return Err(BountyError::InsufficientApprovals);
        }
        let reached_at = proposal
            .threshold_reached_at
            .ok_or(BountyError::InsufficientApprovals)?;
        if env.ledger().timestamp() < reached_at + cfg.recovery_delay {
            return Err(BountyError::RecoveryDelayNotElapsed);
        }
        let old_admin = Self::read_governance(&env)?;
        let new_admin = proposal.new_admin.clone();
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_admin);
        env.storage().instance().remove(&DataKey::PendingRecovery);
        env.events().publish(
            (Symbol::new(&env, "admin_recovered"), proposal.proposal_id),
            (old_admin, new_admin.clone(), proposal.proposal_id),
        );
        Ok(new_admin)
    }

    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
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
    fn bounty_guardian_recovery_rotates_governance() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let new_gov = Address::generate(&env);
        let g1 = Address::generate(&env);
        let g2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        client.initialize(&gov, &token_id);
        let mut guardians = Vec::new(&env);
        guardians.push_back(g1.clone());
        guardians.push_back(g2.clone());
        client.configure_guardians(&gov, &guardians, &2, &3_600);

        client.propose_recovery(&g1, &new_gov);
        client.approve_recovery(&g2);
        env.ledger().with_mut(|l| l.timestamp = 5_000);
        assert_eq!(client.execute_recovery(), new_gov);
        assert_eq!(client.governance(), new_gov);
    }
}
