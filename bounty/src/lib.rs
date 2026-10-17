//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with fee-bump sponsorship support,
//! claimant destination authorization, revocable sponsor accounts, and per-epoch
//! sponsor spend ceilings (`#266`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

const BPS_SCALE: i128 = 10_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SponsorConfig {
    pub sponsor: Address,
    pub active: bool,
    pub epoch_ledgers: u32,
    pub epoch_fee_ceiling: i128,
    pub alert_threshold_bps: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SponsorSpendWindow {
    pub epoch_start: u32,
    pub spent: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingAward {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub claimed: bool,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    SponsorConfig,
    SponsorSpend,
    PendingAward(BytesN<32>),
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
    AwardNotFound = 7,
    AlreadyClaimed = 8,
    SponsorshipRevoked = 9,
    SponsorCeilingExceeded = 10,
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
        Ok(())
    }

    pub fn set_sponsor_config(
        env: Env,
        caller: Address,
        config: SponsorConfig,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if config.epoch_ledgers == 0 || config.epoch_fee_ceiling <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::SponsorConfig, &config);
        Ok(())
    }

    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance_internal(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Governance approves a bounty credit for `researcher` that can be claimed directly or via
    /// a sponsor-relayed fee-bump transaction.
    pub fn approve_award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::PendingAward(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance_internal(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));
        env.storage().persistent().set(
            &DataKey::PendingAward(report_hash.clone()),
            &PendingAward {
                researcher,
                amount,
                report_hash,
                claimed: false,
            },
        );
        Ok(())
    }

    /// Relayed fee-bump claim (`#266`):
    /// - Requires `researcher.require_auth()` over `(report_hash, destination, expected_amount)`
    ///   so the sponsor cannot redirect funds or tamper with the inner operation.
    /// - Requires `sponsor.require_auth()` and verifies sponsorship is active and within the
    ///   per-epoch fee ceiling.
    pub fn claim_sponsored(
        env: Env,
        sponsor: Address,
        researcher: Address,
        destination: Address,
        report_hash: BytesN<32>,
        expected_amount: i128,
        fee_units: i128,
    ) -> Result<i128, BountyError> {
        researcher.require_auth();
        sponsor.require_auth();
        if fee_units <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let cfg: SponsorConfig = env
            .storage()
            .instance()
            .get(&DataKey::SponsorConfig)
            .ok_or(BountyError::SponsorshipRevoked)?;
        if !cfg.active || sponsor != cfg.sponsor {
            return Err(BountyError::SponsorshipRevoked);
        }

        let mut award: PendingAward = env
            .storage()
            .persistent()
            .get(&DataKey::PendingAward(report_hash.clone()))
            .ok_or(BountyError::AwardNotFound)?;
        if award.claimed {
            return Err(BountyError::AlreadyClaimed);
        }
        if award.researcher != researcher || award.amount != expected_amount {
            return Err(BountyError::Unauthorized);
        }

        let now = env.ledger().sequence();
        let mut spend: SponsorSpendWindow = env
            .storage()
            .instance()
            .get(&DataKey::SponsorSpend)
            .unwrap_or(SponsorSpendWindow {
                epoch_start: now,
                spent: 0,
            });
        if now >= spend.epoch_start.saturating_add(cfg.epoch_ledgers) {
            spend.epoch_start = now;
            spend.spent = 0;
        }
        if spend.spent + fee_units > cfg.epoch_fee_ceiling {
            return Err(BountyError::SponsorCeilingExceeded);
        }
        spend.spent += fee_units;
        env.storage().instance().set(&DataKey::SponsorSpend, &spend);

        if spend.spent * BPS_SCALE >= cfg.epoch_fee_ceiling * (cfg.alert_threshold_bps as i128) {
            env.events().publish(
                (symbol_short!("spn_warn"), sponsor),
                (spend.spent, cfg.epoch_fee_ceiling),
            );
        }

        award.claimed = true;
        env.storage()
            .persistent()
            .set(&DataKey::PendingAward(report_hash), &award);

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &destination,
            &award.amount,
        );
        Ok(award.amount)
    }

    pub fn sponsor_epoch_spend(env: Env) -> i128 {
        env.storage()
            .instance()
            .get::<_, SponsorSpendWindow>(&DataKey::SponsorSpend)
            .map(|w| w.spent)
            .unwrap_or(0)
    }

    fn governance_internal(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance_internal(env: &Env) -> Result<i128, BountyError> {
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
    fn sponsored_bounty_claim_verifies_amount_revocation_and_ceiling() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let sponsor = Address::generate(&env);
        let researcher = Address::generate(&env);
        let dest = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_token = token::Client::new(&env, &usdc);
        StellarAssetClient::new(&env, &usdc).mint(&gov, &2_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.fund(&gov, &2_000);

        client.set_sponsor_config(
            &gov,
            &SponsorConfig {
                sponsor: sponsor.clone(),
                active: true,
                epoch_ledgers: 500,
                epoch_fee_ceiling: 100,
                alert_threshold_bps: 8_000,
            },
        );

        let h1 = BytesN::from_array(&env, &[1u8; 32]);
        let h2 = BytesN::from_array(&env, &[2u8; 32]);
        client.approve_award(&gov, &researcher, &500, &h1);
        client.approve_award(&gov, &researcher, &500, &h2);

        // Tampered amount rejected
        assert_eq!(
            client.try_claim_sponsored(&sponsor, &researcher, &dest, &h1, &499, &40),
            Err(Ok(BountyError::Unauthorized))
        );

        // Valid relayed claim succeeds and increments sponsor epoch spend to 85 (triggers 80% warning)
        assert_eq!(
            client.claim_sponsored(&sponsor, &researcher, &dest, &h1, &500, &85),
            500
        );
        assert_eq!(usdc_token.balance(&dest), 500);
        assert_eq!(client.sponsor_epoch_spend(), 85);

        // Exceeding sponsor ceiling (85 + 20 > 100) rejected
        assert_eq!(
            client.try_claim_sponsored(&sponsor, &researcher, &dest, &h2, &500, &20),
            Err(Ok(BountyError::SponsorCeilingExceeded))
        );

        // Revoking sponsorship mid-flight returns SponsorshipRevoked
        client.set_sponsor_config(
            &gov,
            &SponsorConfig {
                sponsor: sponsor.clone(),
                active: false,
                epoch_ledgers: 500,
                epoch_fee_ceiling: 100,
                alert_threshold_bps: 8_000,
            },
        );
        assert_eq!(
            client.try_claim_sponsored(&sponsor, &researcher, &dest, &h2, &500, &10),
            Err(Ok(BountyError::SponsorshipRevoked))
        );
    }
}
