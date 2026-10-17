//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with coordinated disclosure
//! embargo windows (`Submitted -> Triaged -> FixAcknowledged -> Embargoed -> Published`),
//! reporter-consented early publication, and sealed metadata read-gating.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

pub const MIN_EMBARGO_LEDGERS: u32 = 5;
pub const MAX_EMBARGO_LEDGERS: u32 = 200_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
    ProgramConfig(u64),
    NextReportId,
    Disclosure(u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramEmbargoConfig {
    pub program_id: u64,
    pub owner: Address,
    pub triager: Address,
    pub embargo_ledgers: u32,
    pub allow_early_publication: bool,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DisclosureStatus {
    Submitted = 0,
    Triaged = 1,
    FixAcknowledged = 2,
    Embargoed = 3,
    Published = 4,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisclosureRecord {
    pub id: u64,
    pub program_id: u64,
    pub reporter: Address,
    pub sealed_summary_hash: BytesN<32>,
    pub advisory_digest: BytesN<32>,
    pub submitted_ledger: u32,
    pub embargo_end_ledger: u32,
    pub published_ledger: u32,
    pub reporter_early_consent: bool,
    pub status: DisclosureStatus,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicDisclosureView {
    pub id: u64,
    pub program_id: u64,
    pub reporter: Address,
    pub status: DisclosureStatus,
    pub embargo_end_ledger: u32,
    pub published_ledger: u32,
    /// Zeroed (`[0u8; 32]`) while sealed under embargo; revealed once published or embargo expires.
    pub revealed_summary_hash: BytesN<32>,
    pub advisory_digest: BytesN<32>,
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
    InvalidEmbargoBounds = 7,
    ProgramNotFound = 8,
    DisclosureNotFound = 9,
    InvalidTransition = 10,
    EmbargoStillActive = 11,
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
        env.storage().instance().set(&DataKey::NextReportId, &0u64);
        Ok(())
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

    /// Configures a program's disclosure embargo duration and early-publication consent policy.
    pub fn configure_program_embargo(
        env: Env,
        caller: Address,
        program_id: u64,
        owner: Address,
        triager: Address,
        embargo_ledgers: u32,
        allow_early_publication: bool,
    ) -> Result<ProgramEmbargoConfig, BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, ProgramEmbargoConfig>(&DataKey::ProgramConfig(program_id))
        {
            if caller != gov && caller != existing.owner {
                return Err(BountyError::Unauthorized);
            }
        } else if caller != gov {
            return Err(BountyError::Unauthorized);
        }

        if !(MIN_EMBARGO_LEDGERS..=MAX_EMBARGO_LEDGERS).contains(&embargo_ledgers) {
            return Err(BountyError::InvalidEmbargoBounds);
        }

        let cfg = ProgramEmbargoConfig {
            program_id,
            owner,
            triager,
            embargo_ledgers,
            allow_early_publication,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ProgramConfig(program_id), &cfg);
        Ok(cfg)
    }

    /// Reporter submits a sealed vulnerability disclosure (`Submitted`).
    pub fn submit_disclosure(
        env: Env,
        program_id: u64,
        reporter: Address,
        sealed_summary_hash: BytesN<32>,
    ) -> Result<u64, BountyError> {
        reporter.require_auth();
        let prog: ProgramEmbargoConfig = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramConfig(program_id))
            .ok_or(BountyError::ProgramNotFound)?;

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextReportId)
            .unwrap_or(0);
        let now = env.ledger().sequence();
        let rec = DisclosureRecord {
            id,
            program_id,
            reporter: reporter.clone(),
            sealed_summary_hash,
            advisory_digest: BytesN::from_array(&env, &[0u8; 32]),
            submitted_ledger: now,
            embargo_end_ledger: now.saturating_add(prog.embargo_ledgers),
            published_ledger: 0,
            reporter_early_consent: false,
            status: DisclosureStatus::Submitted,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(id), &rec);
        env.storage()
            .instance()
            .set(&DataKey::NextReportId, &(id + 1));

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("dsc_sub")),
            (id, program_id, reporter, rec.embargo_end_ledger),
        );
        Ok(id)
    }

    /// Triager or program owner transitions `Submitted -> Triaged`.
    pub fn triage_disclosure(
        env: Env,
        caller: Address,
        disclosure_id: u64,
    ) -> Result<DisclosureStatus, BountyError> {
        caller.require_auth();
        let mut rec: DisclosureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Disclosure(disclosure_id))
            .ok_or(BountyError::DisclosureNotFound)?;
        let prog = Self::read_program(&env, rec.program_id)?;
        if caller != prog.triager && caller != prog.owner {
            return Err(BountyError::Unauthorized);
        }
        if rec.status != DisclosureStatus::Submitted {
            return Err(BountyError::InvalidTransition);
        }
        rec.status = DisclosureStatus::Triaged;
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(disclosure_id), &rec);
        Ok(rec.status)
    }

    /// Program owner acknowledges fix (`Triaged -> FixAcknowledged`).
    pub fn acknowledge_fix(
        env: Env,
        caller: Address,
        disclosure_id: u64,
    ) -> Result<DisclosureStatus, BountyError> {
        caller.require_auth();
        let mut rec: DisclosureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Disclosure(disclosure_id))
            .ok_or(BountyError::DisclosureNotFound)?;
        let prog = Self::read_program(&env, rec.program_id)?;
        if caller != prog.owner {
            return Err(BountyError::Unauthorized);
        }
        if rec.status != DisclosureStatus::Triaged {
            return Err(BountyError::InvalidTransition);
        }
        rec.status = DisclosureStatus::FixAcknowledged;
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(disclosure_id), &rec);
        Ok(rec.status)
    }

    /// Program owner or triager transitions `FixAcknowledged -> Embargoed`.
    pub fn enter_embargo(
        env: Env,
        caller: Address,
        disclosure_id: u64,
    ) -> Result<DisclosureStatus, BountyError> {
        caller.require_auth();
        let mut rec: DisclosureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Disclosure(disclosure_id))
            .ok_or(BountyError::DisclosureNotFound)?;
        let prog = Self::read_program(&env, rec.program_id)?;
        if caller != prog.owner && caller != prog.triager {
            return Err(BountyError::Unauthorized);
        }
        if rec.status != DisclosureStatus::FixAcknowledged {
            return Err(BountyError::InvalidTransition);
        }
        rec.status = DisclosureStatus::Embargoed;
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(disclosure_id), &rec);
        Ok(rec.status)
    }

    /// Reporter records explicit on-chain consent for early publication before `embargo_end_ledger`.
    pub fn consent_early_publication(
        env: Env,
        reporter: Address,
        disclosure_id: u64,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        let mut rec: DisclosureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Disclosure(disclosure_id))
            .ok_or(BountyError::DisclosureNotFound)?;
        if rec.reporter != reporter {
            return Err(BountyError::Unauthorized);
        }
        rec.reporter_early_consent = true;
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(disclosure_id), &rec);
        Ok(())
    }

    /// Publishes the disclosure advisory (`Published`) once either:
    /// - `ledger >= embargo_end_ledger` (callable by reporter or program owner even if unfixed), OR
    /// - `status == Embargoed` (or `FixAcknowledged`), `allow_early_publication`, and `reporter_early_consent`.
    pub fn publish_disclosure(
        env: Env,
        caller: Address,
        disclosure_id: u64,
        advisory_digest: BytesN<32>,
    ) -> Result<PublicDisclosureView, BountyError> {
        caller.require_auth();
        let mut rec: DisclosureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Disclosure(disclosure_id))
            .ok_or(BountyError::DisclosureNotFound)?;
        if rec.status == DisclosureStatus::Published {
            return Err(BountyError::InvalidTransition);
        }

        let prog = Self::read_program(&env, rec.program_id)?;
        if caller != rec.reporter && caller != prog.owner {
            return Err(BountyError::Unauthorized);
        }

        let now = env.ledger().sequence();
        let embargo_expired = now >= rec.embargo_end_ledger;
        let early_allowed = prog.allow_early_publication
            && rec.reporter_early_consent
            && (rec.status == DisclosureStatus::FixAcknowledged
                || rec.status == DisclosureStatus::Embargoed);

        if !embargo_expired && !early_allowed {
            return Err(BountyError::EmbargoStillActive);
        }

        rec.advisory_digest = advisory_digest;
        rec.published_ledger = now;
        rec.status = DisclosureStatus::Published;
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(disclosure_id), &rec);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("dsc_pub")),
            (disclosure_id, rec.program_id, rec.reporter.clone(), now),
        );
        Self::get_public_disclosure(env, disclosure_id).ok_or(BountyError::DisclosureNotFound)
    }

    /// Read view that keeps sealed metadata (`revealed_summary_hash`, `advisory_digest`)
    /// zeroed out until `status == Published || ledger >= embargo_end_ledger`.
    pub fn get_public_disclosure(env: Env, disclosure_id: u64) -> Option<PublicDisclosureView> {
        let rec: DisclosureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Disclosure(disclosure_id))?;
        let now = env.ledger().sequence();
        let unsealed = rec.status == DisclosureStatus::Published || now >= rec.embargo_end_ledger;
        let zero = BytesN::from_array(&env, &[0u8; 32]);
        Some(PublicDisclosureView {
            id: rec.id,
            program_id: rec.program_id,
            reporter: rec.reporter,
            status: rec.status,
            embargo_end_ledger: rec.embargo_end_ledger,
            published_ledger: rec.published_ledger,
            revealed_summary_hash: if unsealed {
                rec.sealed_summary_hash
            } else {
                zero.clone()
            },
            advisory_digest: if unsealed { rec.advisory_digest } else { zero },
        })
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    fn read_program(env: &Env, program_id: u64) -> Result<ProgramEmbargoConfig, BountyError> {
        env.storage()
            .persistent()
            .get(&DataKey::ProgramConfig(program_id))
            .ok_or(BountyError::ProgramNotFound)
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
    use soroban_sdk::testutils::{Address as _, Ledger};

    #[test]
    fn disclosure_embargo_seals_metadata_enforces_lifecycle_and_supports_consent_early_publish() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let owner = Address::generate(&env);
        let triager = Address::generate(&env);
        let reporter = Address::generate(&env);
        let token_id = Address::generate(&env);

        let cid = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &cid);

        client.initialize(&gov, &token_id);

        // Out-of-bounds embargo duration (< 5 ledgers) is rejected
        assert_eq!(
            client.try_configure_program_embargo(&gov, &1, &owner, &triager, &2, &true),
            Err(Ok(BountyError::InvalidEmbargoBounds))
        );

        client.configure_program_embargo(&gov, &1, &owner, &triager, &20, &true);

        let secret_summary = BytesN::from_array(&env, &[9u8; 32]);
        let advisory = BytesN::from_array(&env, &[7u8; 32]);
        let d0 = client.submit_disclosure(&1, &reporter, &secret_summary);

        // While under embargo, public view zeroes out sealed_summary_hash
        let sealed_view = client.get_public_disclosure(&d0).unwrap();
        assert_eq!(
            sealed_view.revealed_summary_hash,
            BytesN::from_array(&env, &[0u8; 32])
        );

        // Walk lifecycle: Submitted -> Triaged -> FixAcknowledged -> Embargoed
        client.triage_disclosure(&triager, &d0);
        client.acknowledge_fix(&owner, &d0);
        client.enter_embargo(&owner, &d0);

        // Early publication fails before reporter consents
        assert_eq!(
            client.try_publish_disclosure(&owner, &d0, &advisory),
            Err(Ok(BountyError::EmbargoStillActive))
        );

        // Reporter consents -> early publication succeeds at ledger 5 (< embargo_end 20)
        env.ledger().with_mut(|l| l.sequence_number = 5);
        client.consent_early_publication(&reporter, &d0);
        let pub_view = client.publish_disclosure(&owner, &d0, &advisory);
        assert_eq!(pub_view.status, DisclosureStatus::Published);
        assert_eq!(pub_view.revealed_summary_hash, secret_summary);
        assert_eq!(pub_view.advisory_digest, advisory);
    }
}
