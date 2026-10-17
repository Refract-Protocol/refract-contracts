#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidGuardianConfig = 4,
    RecoveryAlreadyPending = 5,
    NoPendingRecovery = 6,
    InsufficientApprovals = 7,
    RecoveryDelayNotElapsed = 8,
    AlreadyApproved = 9,
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

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    GuardianConfig,
    PendingRecovery,
    NextProposalId,
}

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
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &1u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Configure the guardian set `(guardians, threshold, recovery_delay)`.
    /// Updating the guardian set increments `epoch` and invalidates any
    /// in-flight recovery proposal.
    pub fn configure_guardians(
        env: Env,
        caller: Address,
        guardians: Vec<Address>,
        threshold: u32,
        recovery_delay: u64,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if guardians.is_empty()
            || threshold == 0
            || threshold > guardians.len()
            || recovery_delay == 0
        {
            return Err(TreasuryError::InvalidGuardianConfig);
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

    /// Propose replacing a lost/compromised admin key with `new_admin`.
    /// Callable only by a registered guardian. Only one pending recovery may
    /// exist at a time.
    pub fn propose_recovery(
        env: Env,
        guardian: Address,
        new_admin: Address,
    ) -> Result<u64, TreasuryError> {
        guardian.require_auth();
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&DataKey::GuardianConfig)
            .ok_or(TreasuryError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(TreasuryError::Unauthorized);
        }
        if env.storage().instance().has(&DataKey::PendingRecovery) {
            return Err(TreasuryError::RecoveryAlreadyPending);
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

    /// Add a guardian approval to the currently pending recovery proposal.
    pub fn approve_recovery(env: Env, guardian: Address) -> Result<u32, TreasuryError> {
        guardian.require_auth();
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&DataKey::GuardianConfig)
            .ok_or(TreasuryError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(TreasuryError::Unauthorized);
        }
        let mut proposal: RecoveryProposal = env
            .storage()
            .instance()
            .get(&DataKey::PendingRecovery)
            .ok_or(TreasuryError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(TreasuryError::NoPendingRecovery);
        }
        if proposal.approvals.contains(&guardian) {
            return Err(TreasuryError::AlreadyApproved);
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

    /// Cancel a pending recovery proposal. Callable at any time before
    /// execution by the incumbent admin.
    pub fn cancel_recovery(env: Env, caller: Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if !env.storage().instance().has(&DataKey::PendingRecovery) {
            return Err(TreasuryError::NoPendingRecovery);
        }
        env.storage().instance().remove(&DataKey::PendingRecovery);
        Ok(())
    }

    /// Execute a matured M-of-N recovery proposal after `recovery_delay` has
    /// elapsed, rotating `Admin` atomically and emitting `admin_recovered`.
    pub fn execute_recovery(env: Env) -> Result<Address, TreasuryError> {
        let cfg: GuardianSetConfig = env
            .storage()
            .instance()
            .get(&DataKey::GuardianConfig)
            .ok_or(TreasuryError::InvalidGuardianConfig)?;
        let proposal: RecoveryProposal = env
            .storage()
            .instance()
            .get(&DataKey::PendingRecovery)
            .ok_or(TreasuryError::NoPendingRecovery)?;

        if proposal.guardian_epoch != cfg.epoch {
            return Err(TreasuryError::NoPendingRecovery);
        }
        if proposal.approvals.len() < cfg.threshold {
            return Err(TreasuryError::InsufficientApprovals);
        }
        let reached_at = proposal
            .threshold_reached_at
            .ok_or(TreasuryError::InsufficientApprovals)?;
        if env.ledger().timestamp() < reached_at + cfg.recovery_delay {
            return Err(TreasuryError::RecoveryDelayNotElapsed);
        }

        let old_admin = Self::require_admin_addr(&env)?;
        let new_admin = proposal.new_admin.clone();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingRecovery);

        env.events().publish(
            (Symbol::new(&env, "admin_recovered"), proposal.proposal_id),
            (old_admin, new_admin.clone(), proposal.proposal_id),
        );

        Ok(new_admin)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    fn require_admin_addr(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
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
    fn guardian_m_of_n_admin_recovery_delay_cancel_and_replay_protection() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let g1 = Address::generate(&env);
        let g2 = Address::generate(&env);
        let g3 = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.initialize(&admin);

        let mut guardians = Vec::new(&env);
        guardians.push_back(g1.clone());
        guardians.push_back(g2.clone());
        guardians.push_back(g3.clone());
        client.configure_guardians(&admin, &guardians, &2, &86_400);

        // Propose and cancel by incumbent admin.
        client.propose_recovery(&g1, &new_admin);
        client.cancel_recovery(&admin);
        assert_eq!(
            client.try_execute_recovery(),
            Err(Ok(TreasuryError::NoPendingRecovery))
        );

        // Re-propose; 1-of-3 approval is insufficient.
        client.propose_recovery(&g1, &new_admin);
        assert_eq!(
            client.try_execute_recovery(),
            Err(Ok(TreasuryError::InsufficientApprovals))
        );

        // 2-of-3 approval reaches threshold, but delay (86_400) has not elapsed.
        client.approve_recovery(&g2);
        assert_eq!(
            client.try_execute_recovery(),
            Err(Ok(TreasuryError::RecoveryDelayNotElapsed))
        );

        // Advance past recovery_delay -> execution succeeds and rotates admin!
        env.ledger().with_mut(|l| l.timestamp = 90_000);
        assert_eq!(client.execute_recovery(), new_admin);
        assert_eq!(client.admin(), new_admin);

        // Replay attempt after execution fails with NoPendingRecovery.
        assert_eq!(
            client.try_execute_recovery(),
            Err(Ok(TreasuryError::NoPendingRecovery))
        );
    }
}
