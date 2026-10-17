//! # RefractGrants
//!
//! Milestone-based ecosystem grant disbursement contract with deliverable
//! SPDX licensing declarations, content-addressed deliverable hashes, and
//! IP-assignment attestations.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidConfig = 3,
    InvalidGrant = 4,
    GrantNotFound = 5,
    MilestoneNotFound = 6,
    MilestoneAlreadyReleased = 7,
    GrantNotActive = 8,
    DuplicateApproval = 9,
    MalformedLicenseIdentifier = 10,
    UnacceptableLicense = 11,
    MissingRequiredLicense = 12,
    DeliverableImmutable = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    NextGrantId,
    Grant(u64),
    ProgramLicensePolicy(u64),
    Deliverable(u64, u32),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub reviewers: Vec<Address>,
    pub approval_threshold: u32,
    pub token: Address,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GrantStatus {
    Active = 0,
    Completed = 1,
    Cancelled = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub description: Symbol,
    pub amount: i128,
    pub approvals: Vec<Address>,
    pub released: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub id: u64,
    pub program_id: u64,
    pub recipient: Address,
    pub total_amount: i128,
    pub released_amount: i128,
    pub milestones: Vec<Milestone>,
    pub status: GrantStatus,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramLicensePolicy {
    pub program_id: u64,
    pub license_required: bool,
    pub require_ip_attestation: bool,
    pub acceptable_licenses: Vec<Symbol>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliverableDeclaration {
    pub grant_id: u64,
    pub milestone_idx: u32,
    pub spdx_license: Symbol,
    pub deliverable_hash: BytesN<32>,
    pub has_ip_attestation: bool,
    pub ip_attestation: BytesN<32>,
    pub accepted: bool,
    pub updated_at: u64,
}

const KNOWN_SPDX: &[&str] = &[
    "MIT", "Apache2", "GPL3", "BSD3", "BSD2", "ISC", "MPL2", "LGPL3", "CC0", "Unlicens",
];

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(
        env: Env,
        governance: Address,
        reviewers: Vec<Address>,
        approval_threshold: u32,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(GrantsError::AlreadyInitialized);
        }
        governance.require_auth();
        if reviewers.is_empty() || approval_threshold == 0 || approval_threshold > reviewers.len() {
            return Err(GrantsError::InvalidConfig);
        }
        let cfg = Config {
            governance,
            reviewers,
            approval_threshold,
            token,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    /// Configures per-program licensing policy.
    pub fn set_program_license_policy(
        env: Env,
        program_id: u64,
        license_required: bool,
        require_ip_attestation: bool,
        acceptable_licenses: Vec<Symbol>,
    ) -> Result<ProgramLicensePolicy, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();

        for lic in acceptable_licenses.iter() {
            if !Self::is_valid_spdx(&env, &lic) {
                return Err(GrantsError::MalformedLicenseIdentifier);
            }
        }
        if license_required && acceptable_licenses.is_empty() {
            return Err(GrantsError::InvalidConfig);
        }

        let policy = ProgramLicensePolicy {
            program_id,
            license_required,
            require_ip_attestation,
            acceptable_licenses,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ProgramLicensePolicy(program_id), &policy);
        env.events().publish(
            (symbol_short!("grants"), symbol_short!("lic_pol")),
            (program_id, license_required, require_ip_attestation),
        );
        Ok(policy)
    }

    pub fn create_grant(
        env: Env,
        recipient: Address,
        milestones: Vec<(Symbol, i128)>,
    ) -> Result<u64, GrantsError> {
        Self::create_program_grant(env, 0, recipient, milestones)
    }

    pub fn create_program_grant(
        env: Env,
        program_id: u64,
        recipient: Address,
        milestones: Vec<(Symbol, i128)>,
    ) -> Result<u64, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if milestones.is_empty() {
            return Err(GrantsError::InvalidGrant);
        }

        let mut total: i128 = 0;
        let mut built: Vec<Milestone> = Vec::new(&env);
        for (desc, amount) in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidGrant);
            }
            total = total.checked_add(amount).ok_or(GrantsError::InvalidGrant)?;
            built.push_back(Milestone {
                description: desc,
                amount,
                approvals: Vec::new(&env),
                released: false,
            });
        }

        token::Client::new(&env, &cfg.token).transfer(
            &cfg.governance,
            &env.current_contract_address(),
            &total,
        );

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        let grant = Grant {
            id,
            program_id,
            recipient: recipient.clone(),
            total_amount: total,
            released_amount: 0,
            milestones: built,
            status: GrantStatus::Active,
        };
        env.storage().persistent().set(&DataKey::Grant(id), &grant);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(id + 1));

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("create")),
            (id, recipient, total),
        );
        Ok(id)
    }

    /// Submits or amends deliverable evidence and SPDX licensing declaration for a milestone.
    /// Immutable once the milestone has been released.
    pub fn submit_deliverable(
        env: Env,
        grant_id: u64,
        milestone_idx: u32,
        spdx_license: Symbol,
        deliverable_hash: BytesN<32>,
        ip_attestation: Option<BytesN<32>>,
    ) -> Result<DeliverableDeclaration, GrantsError> {
        let _cfg = Self::read_config(&env)?;
        let grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        grant.recipient.require_auth();

        let ms = grant
            .milestones
            .get(milestone_idx)
            .ok_or(GrantsError::MilestoneNotFound)?;
        if ms.released {
            return Err(GrantsError::DeliverableImmutable);
        }

        if !Self::is_valid_spdx(&env, &spdx_license) {
            return Err(GrantsError::MalformedLicenseIdentifier);
        }
        if deliverable_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(GrantsError::InvalidGrant);
        }

        let accepted =
            Self::check_policy_acceptance(&env, grant.program_id, &spdx_license, &ip_attestation);

        let (has_ip_attestation, ip_attestation_bytes) = match ip_attestation {
            Some(att) => (true, att),
            None => (false, BytesN::from_array(&env, &[0u8; 32])),
        };

        let decl = DeliverableDeclaration {
            grant_id,
            milestone_idx,
            spdx_license: spdx_license.clone(),
            deliverable_hash,
            has_ip_attestation,
            ip_attestation: ip_attestation_bytes,
            accepted,
            updated_at: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Deliverable(grant_id, milestone_idx), &decl);

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("deliv")),
            (grant_id, milestone_idx, spdx_license, accepted),
        );
        Ok(decl)
    }

    pub fn approve_milestone(
        env: Env,
        reviewer: Address,
        grant_id: u64,
        milestone_idx: u32,
    ) -> Result<bool, GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::InvalidConfig);
        }
        reviewer.require_auth();

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        let mut ms = grant
            .milestones
            .get(milestone_idx)
            .ok_or(GrantsError::MilestoneNotFound)?;
        if ms.released {
            return Err(GrantsError::MilestoneAlreadyReleased);
        }
        if ms.approvals.contains(&reviewer) {
            return Err(GrantsError::DuplicateApproval);
        }

        ms.approvals.push_back(reviewer);
        let reached = ms.approvals.len() >= cfg.approval_threshold;

        // Check whether releasing this milestone would complete the final unreleased milestone.
        let mut remaining_unreleased = 0u32;
        for (idx, m) in grant.milestones.iter().enumerate() {
            if idx as u32 != milestone_idx && !m.released {
                remaining_unreleased += 1;
            }
        }
        let is_final_release = remaining_unreleased == 0;

        if reached && is_final_release {
            Self::enforce_final_milestone_license(&env, &grant, milestone_idx)?;
        }

        let mut released_now = false;
        if reached {
            ms.released = true;
            grant.released_amount += ms.amount;
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &grant.recipient,
                &ms.amount,
            );
            released_now = true;
            env.events().publish(
                (symbol_short!("grants"), symbol_short!("release")),
                (grant_id, milestone_idx, ms.amount),
            );
        }
        grant.milestones.set(milestone_idx, ms);

        let all_done = grant.milestones.iter().all(|m| m.released);
        if all_done {
            grant.status = GrantStatus::Completed;
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(released_now)
    }

    pub fn cancel_grant(env: Env, grant_id: u64) -> Result<i128, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        let refund = grant.total_amount - grant.released_amount;
        grant.status = GrantStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        if refund > 0 {
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &cfg.governance,
                &refund,
            );
        }
        env.events().publish(
            (symbol_short!("grants"), symbol_short!("cancel")),
            (grant_id, refund),
        );
        Ok(refund)
    }

    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn program_license_policy(env: Env, program_id: u64) -> Option<ProgramLicensePolicy> {
        env.storage()
            .persistent()
            .get(&DataKey::ProgramLicensePolicy(program_id))
    }

    pub fn milestone_deliverable(
        env: Env,
        grant_id: u64,
        milestone_idx: u32,
    ) -> Option<DeliverableDeclaration> {
        env.storage()
            .persistent()
            .get(&DataKey::Deliverable(grant_id, milestone_idx))
    }

    pub fn grant_deliverables(env: Env, grant_id: u64) -> Vec<DeliverableDeclaration> {
        let mut out = Vec::new(&env);
        if let Some(grant) = Self::get_grant(env.clone(), grant_id) {
            for idx in 0..grant.milestones.len() {
                if let Some(decl) = Self::milestone_deliverable(env.clone(), grant_id, idx) {
                    out.push_back(decl);
                }
            }
        }
        out
    }

    fn is_valid_spdx(env: &Env, spdx: &Symbol) -> bool {
        for &known in KNOWN_SPDX {
            if *spdx == Symbol::new(env, known) {
                return true;
            }
        }
        false
    }

    fn check_policy_acceptance(
        env: &Env,
        program_id: u64,
        spdx: &Symbol,
        ip_attestation: &Option<BytesN<32>>,
    ) -> bool {
        if let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, ProgramLicensePolicy>(&DataKey::ProgramLicensePolicy(program_id))
        {
            if policy.require_ip_attestation && ip_attestation.is_none() {
                return false;
            }
            if !policy.acceptable_licenses.is_empty() && !policy.acceptable_licenses.contains(spdx)
            {
                return false;
            }
        }
        true
    }

    fn enforce_final_milestone_license(
        env: &Env,
        grant: &Grant,
        milestone_idx: u32,
    ) -> Result<(), GrantsError> {
        let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, ProgramLicensePolicy>(&DataKey::ProgramLicensePolicy(grant.program_id))
        else {
            return Ok(());
        };

        if !policy.license_required {
            return Ok(());
        }

        let decl: DeliverableDeclaration = env
            .storage()
            .persistent()
            .get(&DataKey::Deliverable(grant.id, milestone_idx))
            .ok_or(GrantsError::MissingRequiredLicense)?;

        if !policy.acceptable_licenses.contains(&decl.spdx_license) {
            env.events().publish(
                (symbol_short!("grants"), symbol_short!("lic_rej")),
                (grant.id, milestone_idx, decl.spdx_license),
            );
            return Err(GrantsError::UnacceptableLicense);
        }
        if policy.require_ip_attestation && !decl.has_ip_attestation {
            return Err(GrantsError::UnacceptableLicense);
        }

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("lic_acc")),
            (grant.id, milestone_idx, decl.spdx_license),
        );
        Ok(())
    }

    fn read_config(env: &Env) -> Result<Config, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec};

    #[test]
    fn deliverable_licensing_enforces_final_milestone_and_immutability() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let r1 = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&gov, &10_000);

        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &cid);

        client.initialize(&gov, &vec![&env, r1.clone()], &1, &token_id);

        client.set_program_license_policy(
            &1,
            &true,
            &true,
            &vec![&env, symbol_short!("MIT"), symbol_short!("Apache2")],
        );

        let gid = client.create_program_grant(
            &1,
            &recipient,
            &vec![
                &env,
                (symbol_short!("m1"), 1_000),
                (symbol_short!("m2"), 2_000),
            ],
        );

        // First milestone is not the final one, so it can release without a deliverable
        assert!(client.approve_milestone(&r1, &gid, &0));

        // Second (final) milestone is blocked when required declaration is missing
        let missing = client.try_approve_milestone(&r1, &gid, &1);
        assert_eq!(missing, Err(Ok(GrantsError::MissingRequiredLicense)));

        // Malformed SPDX is rejected
        let bad_spdx = client.try_submit_deliverable(
            &gid,
            &1,
            &symbol_short!("FAKE_LIC"),
            &BytesN::from_array(&env, &[1u8; 32]),
            &Some(BytesN::from_array(&env, &[2u8; 32])),
        );
        assert_eq!(bad_spdx, Err(Ok(GrantsError::MalformedLicenseIdentifier)));

        // Submit valid SPDX (GPL3) that is not in program acceptable list
        let decl = client.submit_deliverable(
            &gid,
            &1,
            &symbol_short!("GPL3"),
            &BytesN::from_array(&env, &[1u8; 32]),
            &Some(BytesN::from_array(&env, &[2u8; 32])),
        );
        assert!(!decl.accepted);
        assert_eq!(
            client.try_approve_milestone(&r1, &gid, &1),
            Err(Ok(GrantsError::UnacceptableLicense))
        );

        // Amend to acceptable MIT license + IP attestation before release
        let decl2 = client.submit_deliverable(
            &gid,
            &1,
            &symbol_short!("MIT"),
            &BytesN::from_array(&env, &[3u8; 32]),
            &Some(BytesN::from_array(&env, &[4u8; 32])),
        );
        assert!(decl2.accepted);

        // Final milestone now releases cleanly
        assert!(client.approve_milestone(&r1, &gid, &1));
        assert_eq!(
            client.get_grant(&gid).unwrap().status,
            GrantStatus::Completed
        );
        assert_eq!(client.grant_deliverables(&gid).len(), 1);
    }
}
