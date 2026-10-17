#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Vec,
};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Marker set on initialization so missing instance keys can be distinguished as archived.
    Initialized,
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Running total of all bounty payouts awarded.
    TotalAwarded,
    /// Marker indicating a disclosure report hash was previously rewarded.
    AwardKnown(BytesN<32>),
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
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
    EntryArchived = 7,
    InconsistentRestoredState = 8,
}

/// Governance-controlled bug bounty escrow.
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
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::TotalAwarded, &0i128);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        let balance = Self::read_balance(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address. Prevents replay if an existing Award record
    /// was archived by checking `AwardKnown`.
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
        if env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::AwardKnown(report_hash.clone()))
            .unwrap_or(false)
        {
            return Err(BountyError::EntryArchived);
        }

        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let total_awarded: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalAwarded)
            .ok_or(BountyError::EntryArchived)?;

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));
        env.storage()
            .instance()
            .set(&DataKey::TotalAwarded, &(total_awarded + amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .instance()
            .set(&DataKey::AwardKnown(report_hash.clone()), &true);
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Restore an archived award record and verify it belongs to a known disclosure.
    pub fn restore_award_record(env: Env, record: AwardRecord) -> Result<(), BountyError> {
        let _ = Self::read_governance(&env)?;
        let known: bool = env
            .storage()
            .instance()
            .get(&DataKey::AwardKnown(record.report_hash.clone()))
            .unwrap_or(false);
        if !known || record.amount <= 0 {
            return Err(BountyError::InconsistentRestoredState);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Award(record.report_hash.clone()), &record);
        Ok(())
    }

    /// Post-restore consistency verification across escrow balance, token custody,
    /// and restored disclosure award records.
    pub fn verify_post_restore(
        env: Env,
        known_reports: Vec<BytesN<32>>,
    ) -> Result<i128, BountyError> {
        let _ = Self::read_governance(&env)?;
        let token = Self::read_token(&env)?;
        let balance = Self::read_balance(&env)?;
        if balance < 0 {
            return Err(BountyError::InconsistentRestoredState);
        }

        let total_awarded: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalAwarded)
            .ok_or(BountyError::EntryArchived)?;

        let mut sum_awards: i128 = 0;
        for hash in known_reports.iter() {
            let record = Self::read_live_award(&env, &hash)?
                .ok_or(BountyError::InconsistentRestoredState)?;
            sum_awards = sum_awards
                .checked_add(record.amount)
                .ok_or(BountyError::InconsistentRestoredState)?;
        }

        if sum_awards != total_awarded {
            return Err(BountyError::InconsistentRestoredState);
        }

        let actual_balance =
            token::Client::new(&env, &token).balance(&env.current_contract_address());
        if actual_balance < balance {
            return Err(BountyError::InconsistentRestoredState);
        }

        Ok(balance)
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, returning `EntryArchived` if the
    /// disclosure was rewarded previously but its persistent entry is missing.
    pub fn get_award(
        env: Env,
        report_hash: BytesN<32>,
    ) -> Result<Option<AwardRecord>, BountyError> {
        Self::read_live_award(&env, &report_hash)
    }

    fn read_live_award(
        env: &Env,
        report_hash: &BytesN<32>,
    ) -> Result<Option<AwardRecord>, BountyError> {
        if let Some(record) = env
            .storage()
            .persistent()
            .get::<_, AwardRecord>(&DataKey::Award(report_hash.clone()))
        {
            return Ok(Some(record));
        }
        let known: bool = env
            .storage()
            .instance()
            .get(&DataKey::AwardKnown(report_hash.clone()))
            .unwrap_or(false);
        if known {
            return Err(BountyError::EntryArchived);
        }
        Ok(None)
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        if let Some(gov) = env.storage().instance().get(&DataKey::Governance) {
            return Ok(gov);
        }
        Self::archived_or_uninitialized(env)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        if let Some(tok) = env.storage().instance().get(&DataKey::Token) {
            return Ok(tok);
        }
        Self::archived_or_uninitialized(env)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        if let Some(bal) = env.storage().instance().get(&DataKey::Balance) {
            return Ok(bal);
        }
        Self::archived_or_uninitialized(env)
    }

    fn archived_or_uninitialized<T>(env: &Env) -> Result<T, BountyError> {
        let init: bool = env
            .storage()
            .instance()
            .get(&DataKey::Initialized)
            .unwrap_or(false);
        if init {
            Err(BountyError::EntryArchived)
        } else {
            Err(BountyError::NotInitialized)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn bounty_blocks_replay_of_archived_award_and_validates_restore() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let sponsor = Address::generate(&env);
        let researcher = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin);
        let token_id = token_contract.address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&governance, &token_id);
        sac.mint(&sponsor, &1_000);
        client.fund(&sponsor, &1_000);

        let report_hash = BytesN::from_array(&env, &[7u8; 32]);
        client.award(&governance, &researcher, &400, &report_hash);
        assert_eq!(client.balance(), 600);

        // Simulate persistent Award entry archival
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .remove(&DataKey::Award(report_hash.clone()));
        });

        // Reading or re-awarding the archived report must return EntryArchived (no double payout!)
        assert_eq!(
            client.try_get_award(&report_hash),
            Err(Ok(BountyError::EntryArchived))
        );
        assert_eq!(
            client.try_award(&governance, &researcher, &400, &report_hash),
            Err(Ok(BountyError::EntryArchived))
        );

        // Restore with wrong amount fails post-restore consistency check
        client.restore_award_record(&AwardRecord {
            researcher: researcher.clone(),
            amount: 250,
            report_hash: report_hash.clone(),
            timestamp: 0,
        });
        assert_eq!(
            client.try_verify_post_restore(&vec![&env, report_hash.clone()]),
            Err(Ok(BountyError::InconsistentRestoredState))
        );

        // Restore exact award record and complete second disclosure payout
        client.restore_award_record(&AwardRecord {
            researcher: researcher.clone(),
            amount: 400,
            report_hash: report_hash.clone(),
            timestamp: 0,
        });
        assert_eq!(client.verify_post_restore(&vec![&env, report_hash]), 600);
    }
}
