//! # RefractTreasury
//!
//! Protocol fee and reserve vault with guardian set membership management,
//! delayed rotation proposals, threshold validation, and emergency guardian removal.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Vec,
};

const MAX_GUARDIANS: u32 = 15;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidGuardianConfig = 3,
    DuplicateGuardian = 4,
    GuardianNotFound = 5,
    SetBelowThreshold = 6,
    MaxGuardiansExceeded = 7,
    ProposalNotFound = 8,
    ProposalDelayNotElapsed = 9,
    ProposalAlreadyResolved = 10,
    InsufficientGuardianSigners = 11,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    Paused,
    GuardianRecord(Address),
    GuardianList,
    GuardianThreshold,
    RotationDelayLedgers,
    NextProposalId,
    GuardianProposal(u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub token: Address,
    pub max_single_withdrawal: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianRecord {
    pub address: Address,
    pub active: bool,
    pub activation_ledger: u32,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GuardianChangeKind {
    AddGuardian = 0,
    RemoveGuardian = 1,
    SetThreshold = 2,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ProposalStatus {
    Pending = 0,
    Executed = 1,
    Cancelled = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianProposal {
    pub id: u64,
    pub kind: GuardianChangeKind,
    pub target: Address,
    pub new_threshold: u32,
    pub effective_ledger: u32,
    pub status: ProposalStatus,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
        max_single_withdrawal: i128,
        initial_guardians: Vec<Address>,
        threshold: u32,
        rotation_delay_ledgers: u32,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        governance.require_auth();
        if max_single_withdrawal <= 0
            || initial_guardians.is_empty()
            || initial_guardians.len() > MAX_GUARDIANS
            || threshold == 0
            || threshold > initial_guardians.len()
        {
            return Err(TreasuryError::InvalidGuardianConfig);
        }

        let now = env.ledger().sequence();
        let mut list: Vec<Address> = Vec::new(&env);
        for g in initial_guardians.iter() {
            if list.contains(&g) {
                return Err(TreasuryError::DuplicateGuardian);
            }
            list.push_back(g.clone());
            env.storage().persistent().set(
                &DataKey::GuardianRecord(g.clone()),
                &GuardianRecord {
                    address: g,
                    active: true,
                    activation_ledger: now,
                },
            );
        }

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                governance,
                token,
                max_single_withdrawal,
            },
        );
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::GuardianList, &list);
        env.storage()
            .instance()
            .set(&DataKey::GuardianThreshold, &threshold);
        env.storage()
            .instance()
            .set(&DataKey::RotationDelayLedgers, &rotation_delay_ledgers);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &0u64);
        Ok(())
    }

    /// Proposes adding/removing a guardian or updating the guardian threshold after `rotation_delay_ledgers`.
    pub fn propose_guardian_change(
        env: Env,
        kind: GuardianChangeKind,
        target: Address,
        new_threshold: u32,
    ) -> Result<u64, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();

        let delay: u32 = env
            .storage()
            .instance()
            .get(&DataKey::RotationDelayLedgers)
            .unwrap_or(0);
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProposalId)
            .unwrap_or(0);
        let effective_ledger = env.ledger().sequence().saturating_add(delay);

        let prop = GuardianProposal {
            id,
            kind,
            target: target.clone(),
            new_threshold,
            effective_ledger,
            status: ProposalStatus::Pending,
        };
        env.storage()
            .persistent()
            .set(&DataKey::GuardianProposal(id), &prop);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &(id + 1));

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("grd_prop")),
            (id, kind, target, new_threshold, effective_ledger),
        );
        Ok(id)
    }

    /// Cancels a pending guardian rotation proposal before execution.
    pub fn cancel_guardian_change(
        env: Env,
        proposal_id: u64,
    ) -> Result<GuardianProposal, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut prop: GuardianProposal = env
            .storage()
            .persistent()
            .get(&DataKey::GuardianProposal(proposal_id))
            .ok_or(TreasuryError::ProposalNotFound)?;
        if prop.status != ProposalStatus::Pending {
            return Err(TreasuryError::ProposalAlreadyResolved);
        }
        prop.status = ProposalStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::GuardianProposal(proposal_id), &prop);
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("grd_cncl")),
            (proposal_id,),
        );
        Ok(prop)
    }

    /// Executes a pending guardian rotation proposal once `effective_ledger` is reached.
    pub fn execute_guardian_change(
        env: Env,
        proposal_id: u64,
    ) -> Result<GuardianProposal, TreasuryError> {
        let _cfg = Self::read_config(&env)?;
        let mut prop: GuardianProposal = env
            .storage()
            .persistent()
            .get(&DataKey::GuardianProposal(proposal_id))
            .ok_or(TreasuryError::ProposalNotFound)?;
        if prop.status != ProposalStatus::Pending {
            return Err(TreasuryError::ProposalAlreadyResolved);
        }
        let now = env.ledger().sequence();
        if now < prop.effective_ledger {
            return Err(TreasuryError::ProposalDelayNotElapsed);
        }

        let mut list = Self::read_guardian_list(&env);
        let threshold = Self::read_threshold(&env);

        match prop.kind {
            GuardianChangeKind::AddGuardian => {
                if list.contains(&prop.target) {
                    return Err(TreasuryError::DuplicateGuardian);
                }
                if list.len() >= MAX_GUARDIANS {
                    return Err(TreasuryError::MaxGuardiansExceeded);
                }
                list.push_back(prop.target.clone());
                env.storage().instance().set(&DataKey::GuardianList, &list);
                env.storage().persistent().set(
                    &DataKey::GuardianRecord(prop.target.clone()),
                    &GuardianRecord {
                        address: prop.target.clone(),
                        active: true,
                        activation_ledger: now,
                    },
                );
                env.events().publish(
                    (symbol_short!("treasury"), symbol_short!("grd_add")),
                    (prop.target.clone(), now),
                );
            }
            GuardianChangeKind::RemoveGuardian => {
                if !list.contains(&prop.target) {
                    return Err(TreasuryError::GuardianNotFound);
                }
                if list.len() <= threshold {
                    return Err(TreasuryError::SetBelowThreshold);
                }
                Self::remove_from_list(&env, &mut list, &prop.target);
                env.events().publish(
                    (symbol_short!("treasury"), symbol_short!("grd_rem")),
                    (prop.target.clone(), list.len()),
                );
            }
            GuardianChangeKind::SetThreshold => {
                if prop.new_threshold == 0 || prop.new_threshold > list.len() {
                    return Err(TreasuryError::SetBelowThreshold);
                }
                env.storage()
                    .instance()
                    .set(&DataKey::GuardianThreshold, &prop.new_threshold);
                env.events().publish(
                    (symbol_short!("treasury"), symbol_short!("grd_thr")),
                    (threshold, prop.new_threshold),
                );
            }
        }

        prop.status = ProposalStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::GuardianProposal(proposal_id), &prop);
        Ok(prop)
    }

    /// Emergency removal of a single compromised guardian signed by at least `threshold`
    /// of the remaining active guardians. Atomically lowers threshold if needed so the set stays live.
    pub fn emergency_remove_guardian(
        env: Env,
        signers: Vec<Address>,
        target: Address,
    ) -> Result<u32, TreasuryError> {
        let mut list = Self::read_guardian_list(&env);
        let mut threshold = Self::read_threshold(&env);
        if !list.contains(&target) {
            return Err(TreasuryError::GuardianNotFound);
        }
        if list.len() <= 1 {
            return Err(TreasuryError::SetBelowThreshold);
        }

        let mut unique_valid: Vec<Address> = Vec::new(&env);
        for s in signers.iter() {
            if s != target && list.contains(&s) && !unique_valid.contains(&s) {
                s.require_auth();
                unique_valid.push_back(s);
            }
        }
        if unique_valid.len() < threshold {
            return Err(TreasuryError::InsufficientGuardianSigners);
        }

        Self::remove_from_list(&env, &mut list, &target);
        if threshold > list.len() {
            threshold = list.len();
            env.storage()
                .instance()
                .set(&DataKey::GuardianThreshold, &threshold);
        }
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("grd_emrm")),
            (target, list.len(), threshold),
        );
        Ok(threshold)
    }

    /// Circuit-breaker pause callable by any threshold of active guardians (validates rotated set).
    pub fn guardian_pause(env: Env, signers: Vec<Address>) -> Result<bool, TreasuryError> {
        let list = Self::read_guardian_list(&env);
        let threshold = Self::read_threshold(&env);
        let mut valid: Vec<Address> = Vec::new(&env);
        for s in signers.iter() {
            if list.contains(&s) && !valid.contains(&s) {
                s.require_auth();
                valid.push_back(s);
            }
        }
        if valid.len() < threshold {
            return Err(TreasuryError::InsufficientGuardianSigners);
        }
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(true)
    }

    pub fn active_guardians(env: Env) -> Vec<Address> {
        Self::read_guardian_list(&env)
    }

    pub fn guardian_threshold(env: Env) -> u32 {
        Self::read_threshold(&env)
    }

    pub fn guardian_record(env: Env, guardian: Address) -> Option<GuardianRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::GuardianRecord(guardian))
    }

    fn remove_from_list(env: &Env, list: &mut Vec<Address>, target: &Address) {
        let mut next = Vec::new(env);
        for g in list.iter() {
            if g != *target {
                next.push_back(g);
            }
        }
        *list = next.clone();
        env.storage().instance().set(&DataKey::GuardianList, &next);
        if let Some(mut rec) = env
            .storage()
            .persistent()
            .get::<DataKey, GuardianRecord>(&DataKey::GuardianRecord(target.clone()))
        {
            rec.active = false;
            env.storage()
                .persistent()
                .set(&DataKey::GuardianRecord(target.clone()), &rec);
        }
    }

    fn read_guardian_list(env: &Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::GuardianList)
            .unwrap_or_else(|| Vec::new(env))
    }

    fn read_threshold(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::GuardianThreshold)
            .unwrap_or(1)
    }

    fn read_config(env: &Env) -> Result<Config, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(TreasuryError::NotInitialized)
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
    fn guardian_set_rotation_enforces_delays_threshold_bounds_and_emergency_removal() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let token_id = Address::generate(&env);
        let g1 = Address::generate(&env);
        let g2 = Address::generate(&env);
        let g3 = Address::generate(&env);

        let cid = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &cid);

        client.initialize(
            &gov,
            &token_id,
            &5_000,
            &vec![&env, g1.clone(), g2.clone()],
            &2,
            &10,
        );

        // Attempting to remove g2 while set size == threshold (2) fails on execution
        let bad_rem = client.propose_guardian_change(&GuardianChangeKind::RemoveGuardian, &g2, &0);
        // Propose adding g3
        let add_g3 = client.propose_guardian_change(&GuardianChangeKind::AddGuardian, &g3, &0);
        assert_eq!(
            client.try_execute_guardian_change(&add_g3),
            Err(Ok(TreasuryError::ProposalDelayNotElapsed))
        );

        env.ledger().with_mut(|l| l.sequence_number = 10);
        assert_eq!(
            client.try_execute_guardian_change(&bad_rem),
            Err(Ok(TreasuryError::SetBelowThreshold))
        );
        client.execute_guardian_change(&add_g3);
        assert_eq!(client.active_guardians().len(), 3);

        // Emergency removal of compromised g1 by remaining guardians g2 and g3 (>= threshold 2)
        assert_eq!(
            client.emergency_remove_guardian(&vec![&env, g2.clone(), g3.clone()], &g1),
            2
        );
        assert!(!client.guardian_record(&g1).unwrap().active);

        // Rotated set (g2, g3) can trigger circuit-breaker pause
        assert!(client.guardian_pause(&vec![&env, g2, g3]));
    }
}
