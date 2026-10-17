#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{
    contract, contracterror, contractimpl, symbol_short, token, Address, BytesN, Env,
};
use storage::{
    AwardRecord, Bounty, BountyStatus, DataKey, ExpiryConfig, ReclaimPolicy,
    DEFAULT_EXPIRY_WINDOW_SECS, MAX_EXPIRY_WINDOW_SECS, MIN_EXPIRY_WINDOW_SECS,
};

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
    BountyNotFound = 7,
    NotExpired = 8,
    AlreadyPaid = 9,
    AlreadyReclaimed = 10,
    InvalidExpiryWindow = 11,
    SameLedgerConflict = 12,
}

/// Governance-controlled bug bounty escrow with bounded expiry and permissionless reclaim (#207).
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
        env.storage().instance().set(&DataKey::NextBountyId, &1u64);
        env.storage().instance().set(
            &DataKey::ExpiryConfig,
            &ExpiryConfig {
                default_window_secs: DEFAULT_EXPIRY_WINDOW_SECS,
                min_window_secs: MIN_EXPIRY_WINDOW_SECS,
                max_window_secs: MAX_EXPIRY_WINDOW_SECS,
            },
        );
        env.storage()
            .instance()
            .set(&DataKey::ReclaimPolicy, &ReclaimPolicy::ReturnToFunder);
        Ok(())
    }

    /// Governance-gated configuration of the default/min/max expiry windows.
    pub fn set_expiry_config(
        env: Env,
        caller: Address,
        default_window_secs: u64,
        min_window_secs: u64,
        max_window_secs: u64,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if min_window_secs == 0
            || min_window_secs > default_window_secs
            || default_window_secs > max_window_secs
        {
            return Err(BountyError::InvalidExpiryWindow);
        }
        env.storage().instance().set(
            &DataKey::ExpiryConfig,
            &ExpiryConfig {
                default_window_secs,
                min_window_secs,
                max_window_secs,
            },
        );
        Ok(())
    }

    /// Governance-gated update of the reclaim policy (`ReturnToFunder` vs `SweepToTreasury(Address)`).
    pub fn set_reclaim_policy(
        env: Env,
        caller: Address,
        policy: ReclaimPolicy,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::ReclaimPolicy, &policy);
        Ok(())
    }

    /// Top up the reward pool and create a bounded-lifetime bounty escrow using the default expiry window.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<u64, BountyError> {
        Self::fund_with_expiry(env, from, amount, None)
    }

    /// Create a funded bounty escrow with an optional custom expiry window bounded by `[min_window_secs, max_window_secs]`.
    pub fn fund_with_expiry(
        env: Env,
        from: Address,
        amount: i128,
        custom_window_secs: Option<u64>,
    ) -> Result<u64, BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let expiry_cfg = Self::read_expiry_config(&env)?;
        let window = match custom_window_secs {
            Some(w) => {
                if w < expiry_cfg.min_window_secs || w > expiry_cfg.max_window_secs {
                    return Err(BountyError::InvalidExpiryWindow);
                }
                w
            }
            None => expiry_cfg.default_window_secs,
        };

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);

        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));

        let bounty_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextBountyId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextBountyId, &(bounty_id + 1));

        let expiry = env.ledger().timestamp().saturating_add(window);
        let bounty = Bounty {
            id: bounty_id,
            funder: from,
            amount,
            expiry,
            status: BountyStatus::Funded,
            last_updated_ledger: env.ledger().sequence(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Bounty(bounty_id), &bounty);

        Ok(bounty_id)
    }

    /// Award a specific funded bounty (`bounty_id`) to `researcher`, transitioning its status to `Paid`.
    pub fn award_bounty(
        env: Env,
        caller: Address,
        bounty_id: u64,
        researcher: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        let mut bounty: Bounty = env
            .storage()
            .persistent()
            .get(&DataKey::Bounty(bounty_id))
            .ok_or(BountyError::BountyNotFound)?;

        match bounty.status {
            BountyStatus::Paid => return Err(BountyError::AlreadyPaid),
            BountyStatus::Reclaimed => return Err(BountyError::AlreadyReclaimed),
            BountyStatus::Funded => {}
        }

        let balance = Self::read_balance(&env)?;
        if bounty.amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        bounty.status = BountyStatus::Paid;
        bounty.last_updated_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::Bounty(bounty_id), &bounty);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &bounty.amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - bounty.amount));

        let record = AwardRecord {
            researcher,
            amount: bounty.amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Award a general bounty payout from the escrow pool.
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

    /// Permissionlessly reclaim an expired bounty (`now >= bounty.expiry`), returning the escrowed
    /// amount to `bounty.funder` or sweeping to the treasury under the stored [`ReclaimPolicy`].
    pub fn reclaim_expired(env: Env, bounty_id: u64) -> Result<i128, BountyError> {
        let mut bounty: Bounty = env
            .storage()
            .persistent()
            .get(&DataKey::Bounty(bounty_id))
            .ok_or(BountyError::BountyNotFound)?;

        match bounty.status {
            BountyStatus::Paid => return Err(BountyError::AlreadyPaid),
            BountyStatus::Reclaimed => return Err(BountyError::AlreadyReclaimed),
            BountyStatus::Funded => {}
        }

        let now = env.ledger().timestamp();
        if now < bounty.expiry {
            return Err(BountyError::NotExpired);
        }
        if env.ledger().sequence() == bounty.last_updated_ledger {
            return Err(BountyError::SameLedgerConflict);
        }

        let balance = Self::read_balance(&env)?;
        if bounty.amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let policy = Self::read_reclaim_policy(&env);
        let destination = match policy {
            ReclaimPolicy::ReturnToFunder => bounty.funder.clone(),
            ReclaimPolicy::SweepToTreasury(treasury) => treasury,
        };

        bounty.status = BountyStatus::Reclaimed;
        bounty.last_updated_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::Bounty(bounty_id), &bounty);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - bounty.amount));

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &destination,
            &bounty.amount,
        );

        env.events().publish(
            (symbol_short!("bty_exp"), bounty_id),
            (bounty.funder.clone(), bounty.amount, bounty.expiry),
        );
        env.events().publish(
            (symbol_short!("bty_recl"), bounty_id),
            (destination, bounty.amount, now),
        );

        Ok(bounty.amount)
    }

    /// Return the stored `Bounty` record for `bounty_id`, if any.
    pub fn get_bounty(env: Env, bounty_id: u64) -> Option<Bounty> {
        env.storage().persistent().get(&DataKey::Bounty(bounty_id))
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
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

    fn read_expiry_config(env: &Env) -> Result<ExpiryConfig, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::ExpiryConfig)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_reclaim_policy(env: &Env) -> ReclaimPolicy {
        env.storage()
            .instance()
            .get(&DataKey::ReclaimPolicy)
            .unwrap_or(ReclaimPolicy::ReturnToFunder)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};

    fn setup() -> (Env, RefractBountyEscrowClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
            li.sequence_number = 100;
            li.min_temp_entry_ttl = 100_000;
            li.min_persistent_entry_ttl = 100_000;
            li.max_entry_ttl = 1_000_000;
        });
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        client.initialize(&governance, &token_id);
        (env, client, governance, token_id)
    }

    #[test]
    fn expiry_boundary_and_double_reclaim_rejection() {
        let (env, client, _governance, token_id) = setup();
        let funder = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &500_0000000);

        let bounty_id = client.fund(&funder, &500_0000000);
        let rec = client.get_bounty(&bounty_id).unwrap();
        assert_eq!(rec.expiry, 1_700_000_000 + DEFAULT_EXPIRY_WINDOW_SECS);

        // 1 second before expiry -> rejected with NotExpired.
        env.ledger().with_mut(|li| {
            li.timestamp = rec.expiry - 1;
            li.sequence_number = 101;
        });
        assert_eq!(
            client.try_reclaim_expired(&bounty_id),
            Err(Ok(BountyError::NotExpired))
        );

        // Exactly at expiry -> permissionlessly reclaimed back to funder.
        env.ledger().with_mut(|li| {
            li.timestamp = rec.expiry;
            li.sequence_number = 102;
        });
        let reclaimed = client.reclaim_expired(&bounty_id);
        assert_eq!(reclaimed, 500_0000000);
        assert_eq!(
            token::Client::new(&env, &token_id).balance(&funder),
            500_0000000
        );
        assert_eq!(client.balance(), 0);

        // Second reclaim attempt fails with AlreadyReclaimed.
        assert_eq!(
            client.try_reclaim_expired(&bounty_id),
            Err(Ok(BountyError::AlreadyReclaimed))
        );
    }

    #[test]
    fn reclaim_after_payout_is_rejected() {
        let (env, client, governance, token_id) = setup();
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &300_0000000);

        let bounty_id = client.fund(&funder, &300_0000000);
        let report_hash = BytesN::from_array(&env, &[0x11; 32]);
        client.award_bounty(&governance, &bounty_id, &researcher, &report_hash);

        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000 + DEFAULT_EXPIRY_WINDOW_SECS + 100;
            li.sequence_number = 200;
        });
        assert_eq!(
            client.try_reclaim_expired(&bounty_id),
            Err(Ok(BountyError::AlreadyPaid))
        );
    }

    #[test]
    fn sweep_to_treasury_policy_and_custom_window_bounds() {
        let (env, client, governance, token_id) = setup();
        let funder = Address::generate(&env);
        let treasury = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &400_0000000);

        // Out-of-bounds custom window is rejected.
        assert_eq!(
            client.try_fund_with_expiry(&funder, &400_0000000, &Some(10)),
            Err(Ok(BountyError::InvalidExpiryWindow))
        );

        let bounty_id =
            client.fund_with_expiry(&funder, &400_0000000, &Some(MIN_EXPIRY_WINDOW_SECS));
        client.set_reclaim_policy(
            &governance,
            &ReclaimPolicy::SweepToTreasury(treasury.clone()),
        );

        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000 + MIN_EXPIRY_WINDOW_SECS;
            li.sequence_number = 105;
        });
        let swept = client.reclaim_expired(&bounty_id);
        assert_eq!(swept, 400_0000000);
        assert_eq!(
            token::Client::new(&env, &token_id).balance(&treasury),
            400_0000000
        );
        assert_eq!(token::Client::new(&env, &token_id).balance(&funder), 0);
    }
}
