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

/// Maximum evidence attachments permitted per bounty report.
pub const MAX_EVIDENCE_ENTRIES: u32 = 16;
/// Maximum declared attachment size in bytes (100 MB).
pub const MAX_EVIDENCE_BYTE_SIZE: u64 = 100_000_000;
/// Persistent storage TTL extension for long-lived bounty reports.
const MANIFEST_TTL_LEDGERS: u32 = 5_000;

/// Storage keys for the bounty escrow and evidence manifests.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
    Reviewer(Address),
    Manifest(u64),
    Evidence(u64, u32),
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
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EvidenceMediaType {
    PlainText = 0,
    Markdown = 1,
    ArchiveTarGz = 2,
    ProofOfConceptSource = 3,
    PacketCapture = 4,
    EncryptedBlob = 5,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum VerificationStatus {
    Unverified = 0,
    Verified = 1,
    Rejected = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReportManifest {
    pub report_id: u64,
    pub reporter: Address,
    pub adjudication_started: bool,
    pub entry_count: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceEntry {
    pub index: u32,
    pub content_hash: BytesN<32>,
    pub hash_alg: u32,
    pub media_type: EvidenceMediaType,
    pub byte_size: u64,
    pub has_uri: bool,
    pub uri_digest: BytesN<32>,
    pub encrypted: bool,
    pub status: VerificationStatus,
    pub reason_hash: BytesN<32>,
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
    ManifestNotFound = 7,
    ManifestLocked = 8,
    MaxEntriesExceeded = 9,
    InvalidEvidenceMetadata = 10,
    EntryNotFound = 11,
    ContentHashMismatch = 12,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    /// Governance entrypoint to authorize or revoke a security reviewer.
    pub fn set_reviewer(
        env: Env,
        caller: Address,
        reviewer: Address,
        active: bool,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::read_governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Reviewer(reviewer), &active);
        Ok(())
    }

    /// Reporter creates an evidence manifest for `report_id`.
    pub fn create_report_manifest(
        env: Env,
        reporter: Address,
        report_id: u64,
    ) -> Result<ReportManifest, BountyError> {
        reporter.require_auth();
        let key = DataKey::Manifest(report_id);
        if env.storage().persistent().has(&key) {
            return Err(BountyError::DuplicateReport);
        }
        let manifest = ReportManifest {
            report_id,
            reporter: reporter.clone(),
            adjudication_started: false,
            entry_count: 0,
        };
        env.storage().persistent().set(&key, &manifest);
        env.storage()
            .persistent()
            .extend_ttl(&key, MANIFEST_TTL_LEDGERS, MANIFEST_TTL_LEDGERS);
        env.events()
            .publish((symbol_short!("man_new"), reporter), report_id);
        Ok(manifest)
    }

    /// Reporter appends a content-addressed attachment entry before adjudication begins.
    pub fn append_evidence(
        env: Env,
        reporter: Address,
        report_id: u64,
        content_hash: BytesN<32>,
        hash_alg: u32,
        media_type: EvidenceMediaType,
        byte_size: u64,
        has_uri: bool,
        uri_digest: BytesN<32>,
        encrypted: bool,
    ) -> Result<u32, BountyError> {
        reporter.require_auth();
        Self::validate_evidence_fields(
            &env,
            &content_hash,
            hash_alg,
            byte_size,
            has_uri,
            &uri_digest,
        )?;
        let m_key = DataKey::Manifest(report_id);
        let mut manifest: ReportManifest = env
            .storage()
            .persistent()
            .get(&m_key)
            .ok_or(BountyError::ManifestNotFound)?;
        if manifest.reporter != reporter {
            return Err(BountyError::Unauthorized);
        }
        if manifest.adjudication_started {
            return Err(BountyError::ManifestLocked);
        }
        if manifest.entry_count >= MAX_EVIDENCE_ENTRIES {
            return Err(BountyError::MaxEntriesExceeded);
        }

        let idx = manifest.entry_count;
        let entry = EvidenceEntry {
            index: idx,
            content_hash: content_hash.clone(),
            hash_alg,
            media_type,
            byte_size,
            has_uri,
            uri_digest,
            encrypted,
            status: VerificationStatus::Unverified,
            reason_hash: BytesN::from_array(&env, &[0u8; 32]),
        };
        let e_key = DataKey::Evidence(report_id, idx);
        env.storage().persistent().set(&e_key, &entry);
        env.storage()
            .persistent()
            .extend_ttl(&e_key, MANIFEST_TTL_LEDGERS, MANIFEST_TTL_LEDGERS);

        manifest.entry_count += 1;
        env.storage().persistent().set(&m_key, &manifest);
        env.storage()
            .persistent()
            .extend_ttl(&m_key, MANIFEST_TTL_LEDGERS, MANIFEST_TTL_LEDGERS);
        env.events()
            .publish((symbol_short!("ev_add"), report_id, idx), content_hash);
        Ok(idx)
    }

    /// Reporter replaces an existing attachment entry before adjudication begins,
    /// resetting its verification status to `Unverified`.
    pub fn replace_evidence(
        env: Env,
        reporter: Address,
        report_id: u64,
        index: u32,
        content_hash: BytesN<32>,
        hash_alg: u32,
        media_type: EvidenceMediaType,
        byte_size: u64,
        has_uri: bool,
        uri_digest: BytesN<32>,
        encrypted: bool,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        Self::validate_evidence_fields(
            &env,
            &content_hash,
            hash_alg,
            byte_size,
            has_uri,
            &uri_digest,
        )?;
        let m_key = DataKey::Manifest(report_id);
        let manifest: ReportManifest = env
            .storage()
            .persistent()
            .get(&m_key)
            .ok_or(BountyError::ManifestNotFound)?;
        if manifest.reporter != reporter {
            return Err(BountyError::Unauthorized);
        }
        if manifest.adjudication_started {
            return Err(BountyError::ManifestLocked);
        }
        if index >= manifest.entry_count {
            return Err(BountyError::EntryNotFound);
        }

        let entry = EvidenceEntry {
            index,
            content_hash: content_hash.clone(),
            hash_alg,
            media_type,
            byte_size,
            has_uri,
            uri_digest,
            encrypted,
            status: VerificationStatus::Unverified,
            reason_hash: BytesN::from_array(&env, &[0u8; 32]),
        };
        let e_key = DataKey::Evidence(report_id, index);
        env.storage().persistent().set(&e_key, &entry);
        env.storage()
            .persistent()
            .extend_ttl(&e_key, MANIFEST_TTL_LEDGERS, MANIFEST_TTL_LEDGERS);
        env.events()
            .publish((symbol_short!("ev_rep"), report_id, index), content_hash);
        Ok(())
    }

    /// Reviewer or governance locks a report manifest when adjudication begins,
    /// preventing any subsequent reporter mutations.
    pub fn begin_adjudication(
        env: Env,
        caller: Address,
        report_id: u64,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_reviewer_or_gov(&env, &caller)?;
        let m_key = DataKey::Manifest(report_id);
        let mut manifest: ReportManifest = env
            .storage()
            .persistent()
            .get(&m_key)
            .ok_or(BountyError::ManifestNotFound)?;
        manifest.adjudication_started = true;
        env.storage().persistent().set(&m_key, &manifest);
        env.events()
            .publish((symbol_short!("adj_lock"), caller), report_id);
        Ok(())
    }

    /// Reviewer or governance updates the verification status of an attachment entry,
    /// checking that `observed_content_hash` matches the committed `content_hash` when marking `Verified`.
    pub fn verify_evidence(
        env: Env,
        caller: Address,
        report_id: u64,
        index: u32,
        observed_content_hash: BytesN<32>,
        status: VerificationStatus,
        reason_hash: BytesN<32>,
    ) -> Result<EvidenceEntry, BountyError> {
        caller.require_auth();
        Self::require_reviewer_or_gov(&env, &caller)?;
        let e_key = DataKey::Evidence(report_id, index);
        let mut entry: EvidenceEntry = env
            .storage()
            .persistent()
            .get(&e_key)
            .ok_or(BountyError::EntryNotFound)?;

        if status == VerificationStatus::Verified && observed_content_hash != entry.content_hash {
            return Err(BountyError::ContentHashMismatch);
        }

        entry.status = status;
        entry.reason_hash = reason_hash.clone();
        env.storage().persistent().set(&e_key, &entry);
        env.storage()
            .persistent()
            .extend_ttl(&e_key, MANIFEST_TTL_LEDGERS, MANIFEST_TTL_LEDGERS);
        env.events().publish(
            (symbol_short!("ev_ver"), report_id, index),
            (status, reason_hash),
        );
        Ok(entry)
    }

    pub fn get_manifest(env: Env, report_id: u64) -> Option<ReportManifest> {
        env.storage()
            .persistent()
            .get(&DataKey::Manifest(report_id))
    }

    pub fn get_evidence_entry(env: Env, report_id: u64, index: u32) -> Option<EvidenceEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::Evidence(report_id, index))
    }

    pub fn get_all_evidence(env: Env, report_id: u64) -> Result<Vec<EvidenceEntry>, BountyError> {
        let manifest: ReportManifest = env
            .storage()
            .persistent()
            .get(&DataKey::Manifest(report_id))
            .ok_or(BountyError::ManifestNotFound)?;
        let mut out = Vec::new(&env);
        for idx in 0..manifest.entry_count {
            if let Some(entry) = env
                .storage()
                .persistent()
                .get::<_, EvidenceEntry>(&DataKey::Evidence(report_id, idx))
            {
                out.push_back(entry);
            }
        }
        Ok(out)
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
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

    /// Award a bounty to a security researcher.
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

    fn validate_evidence_fields(
        env: &Env,
        content_hash: &BytesN<32>,
        hash_alg: u32,
        byte_size: u64,
        has_uri: bool,
        uri_digest: &BytesN<32>,
    ) -> Result<(), BountyError> {
        let zero = BytesN::from_array(env, &[0u8; 32]);
        if *content_hash == zero
            || hash_alg == 0
            || byte_size == 0
            || byte_size > MAX_EVIDENCE_BYTE_SIZE
        {
            return Err(BountyError::InvalidEvidenceMetadata);
        }
        if has_uri && *uri_digest == zero {
            return Err(BountyError::InvalidEvidenceMetadata);
        }
        Ok(())
    }

    fn require_reviewer_or_gov(env: &Env, caller: &Address) -> Result<(), BountyError> {
        let gov = Self::read_governance(env)?;
        let is_reviewer: bool = env
            .storage()
            .persistent()
            .get(&DataKey::Reviewer(caller.clone()))
            .unwrap_or(false);
        if *caller != gov && !is_reviewer {
            return Err(BountyError::Unauthorized);
        }
        Ok(())
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
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn evidence_manifest_lifecycle_enforces_pre_adjudication_edits_hash_checks_and_verification() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let reviewer = Address::generate(&env);
        let reporter = Address::generate(&env);
        let stranger = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &token_id).mint(&governance, &10_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);
        client.set_reviewer(&governance, &reviewer, &true);

        client.create_report_manifest(&reporter, &101);

        let zero_hash = BytesN::from_array(&env, &[0u8; 32]);
        let h1 = BytesN::from_array(&env, &[0x11u8; 32]);
        let h2 = BytesN::from_array(&env, &[0x22u8; 32]);
        let uri_h = BytesN::from_array(&env, &[0xAAu8; 32]);
        let reason_h = BytesN::from_array(&env, &[0xBBu8; 32]);

        // Reject zero content hash or missing URI digest when has_uri is true
        assert_eq!(
            client.try_append_evidence(
                &reporter,
                &101,
                &zero_hash,
                &1,
                &EvidenceMediaType::Markdown,
                &1024,
                &false,
                &zero_hash,
                &false,
            ),
            Err(Ok(BountyError::InvalidEvidenceMetadata))
        );
        assert_eq!(
            client.try_append_evidence(
                &reporter,
                &101,
                &h1,
                &1,
                &EvidenceMediaType::EncryptedBlob,
                &2048,
                &true,
                &zero_hash,
                &true,
            ),
            Err(Ok(BountyError::InvalidEvidenceMetadata))
        );

        // Append valid entry 0 and replace it before adjudication
        let idx = client.append_evidence(
            &reporter,
            &101,
            &h1,
            &1,
            &EvidenceMediaType::ProofOfConceptSource,
            &4096,
            &true,
            &uri_h,
            &true,
        );
        assert_eq!(idx, 0);

        // Stranger cannot replace reporter's evidence
        assert_eq!(
            client.try_replace_evidence(
                &stranger,
                &101,
                &0,
                &h2,
                &1,
                &EvidenceMediaType::ProofOfConceptSource,
                &8192,
                &true,
                &uri_h,
                &true,
            ),
            Err(Ok(BountyError::Unauthorized))
        );

        client.replace_evidence(
            &reporter,
            &101,
            &0,
            &h2,
            &1,
            &EvidenceMediaType::ProofOfConceptSource,
            &8192,
            &true,
            &uri_h,
            &true,
        );

        // Begin adjudication locks manifest against further reporter appends or replacements
        client.begin_adjudication(&reviewer, &101);
        assert_eq!(
            client.try_append_evidence(
                &reporter,
                &101,
                &h1,
                &1,
                &EvidenceMediaType::PlainText,
                &512,
                &false,
                &zero_hash,
                &false,
            ),
            Err(Ok(BountyError::ManifestLocked))
        );

        // Mismatched observed content hash cannot be marked Verified
        assert_eq!(
            client.try_verify_evidence(
                &reviewer,
                &101,
                &0,
                &h1,
                &VerificationStatus::Verified,
                &reason_h,
            ),
            Err(Ok(BountyError::ContentHashMismatch))
        );

        // Matching observed content hash succeeds
        let verified = client.verify_evidence(
            &reviewer,
            &101,
            &0,
            &h2,
            &VerificationStatus::Verified,
            &reason_h,
        );
        assert_eq!(verified.status, VerificationStatus::Verified);
        assert_eq!(verified.reason_hash, reason_h);
        assert_eq!(client.get_all_evidence(&101).len(), 1);
    }
}
