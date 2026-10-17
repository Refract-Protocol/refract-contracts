//! # RefractBounty
//!
//! Bug bounty escrow with anti-spam submission staking, reporter reputation scoring,
//! stake slashing to treasury on spam/rejected reports, and bounded reputation reward multipliers (`#257`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

const BPS_SCALE: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InvalidConfig = 4,
    StakeBelowMinimum = 5,
    DuplicateReport = 6,
    ReportNotFound = 7,
    AlreadyAdjudicated = 8,
    InsufficientBudget = 9,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdjudicationOutcome {
    Accepted = 0,
    ValidDuplicate = 1,
    Rejected = 2,
    Spam = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReputationConfig {
    pub min_submission_stake: i128,
    pub bonus_bps_per_point: u32,
    pub max_multiplier_bps: u32,
    pub accepted_delta: u32,
    pub duplicate_delta: u32,
    pub rejected_penalty: u32,
    pub spam_penalty: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakedDisclosure {
    pub reporter: Address,
    pub report_hash: BytesN<32>,
    pub stake_amount: i128,
    pub adjudicated: bool,
    pub outcome: AdjudicationOutcome,
    pub payout_amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Asset,
    Treasury,
    Budget,
    Config,
    Reputation(Address),
    Disclosure(BytesN<32>),
}

#[contract]
pub struct RefractBounty;

#[contractimpl]
impl RefractBounty {
    pub fn initialize(
        env: Env,
        admin: Address,
        asset: Address,
        treasury: Address,
        min_submission_stake: i128,
        bonus_bps_per_point: u32,
        max_multiplier_bps: u32,
    ) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(BountyError::AlreadyInitialized);
        }
        admin.require_auth();
        if min_submission_stake < 0 || max_multiplier_bps < 10_000 {
            return Err(BountyError::InvalidConfig);
        }

        let config = ReputationConfig {
            min_submission_stake,
            bonus_bps_per_point,
            max_multiplier_bps,
            accepted_delta: 10,
            duplicate_delta: 2,
            rejected_penalty: 5,
            spam_penalty: 20,
        };

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Asset, &asset);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage().instance().set(&DataKey::Budget, &0i128);
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    pub fn set_reputation_config(
        env: Env,
        caller: Address,
        config: ReputationConfig,
    ) -> Result<(), BountyError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may update reputation config");
        }
        if config.min_submission_stake < 0 || config.max_multiplier_bps < 10_000 {
            return Err(BountyError::InvalidConfig);
        }
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    pub fn fund_pool(env: Env, from: Address, amount: i128) -> Result<i128, BountyError> {
        Self::require_admin(&env)?;
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
        let token_client = token::Client::new(&env, &asset);
        token_client.transfer(&from, &env.current_contract_address(), &amount);

        let new_budget = Self::budget(env.clone()) + amount;
        env.storage().instance().set(&DataKey::Budget, &new_budget);
        Ok(new_budget)
    }

    /// Submits a disclosure along with an anti-spam stake (`stake_amount >= min_submission_stake`).
    pub fn submit_disclosure(
        env: Env,
        reporter: Address,
        report_hash: BytesN<32>,
        stake_amount: i128,
    ) -> Result<(), BountyError> {
        Self::require_admin(&env)?;
        reporter.require_auth();

        let config = Self::reputation_config(env.clone())?;
        if stake_amount < config.min_submission_stake {
            return Err(BountyError::StakeBelowMinimum);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Disclosure(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        if stake_amount > 0 {
            let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
            let token_client = token::Client::new(&env, &asset);
            token_client.transfer(&reporter, &env.current_contract_address(), &stake_amount);
        }

        let record = StakedDisclosure {
            reporter: reporter.clone(),
            report_hash: report_hash.clone(),
            stake_amount,
            adjudicated: false,
            outcome: AdjudicationOutcome::Rejected,
            payout_amount: 0,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Disclosure(report_hash.clone()), &record);

        env.events().publish(
            (symbol_short!("submit"), reporter),
            (report_hash, stake_amount),
        );
        Ok(())
    }

    /// Adjudicates a disclosure:
    /// - `Accepted`: refunds stake, applies pre-submission reputation multiplier to `base_reward`,
    ///   pays the boosted reward from the bounty pool budget, and increments reputation.
    /// - `ValidDuplicate`: refunds stake and increments reputation by `duplicate_delta` with 0 reward.
    /// - `Rejected` / `Spam`: slashes stake to `Treasury` and decrements reputation (floored at 0).
    pub fn adjudicate_disclosure(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        outcome: AdjudicationOutcome,
        base_reward: i128,
    ) -> Result<i128, BountyError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may adjudicate disclosures");
        }

        let disc_key = DataKey::Disclosure(report_hash.clone());
        let mut record: StakedDisclosure = env
            .storage()
            .persistent()
            .get(&disc_key)
            .ok_or(BountyError::ReportNotFound)?;
        if record.adjudicated {
            return Err(BountyError::AlreadyAdjudicated);
        }

        let config = Self::reputation_config(env.clone())?;
        let current_rep = Self::reputation_score(env.clone(), record.reporter.clone());
        let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
        let token_client = token::Client::new(&env, &asset);

        let (payout, new_rep) = match outcome {
            AdjudicationOutcome::Accepted => {
                if base_reward <= 0 {
                    return Err(BountyError::InvalidAmount);
                }
                let mult_bps = Self::multiplier_bps(env.clone(), record.reporter.clone());
                let boosted_reward = (base_reward * (mult_bps as i128)) / BPS_SCALE;
                let current_budget = Self::budget(env.clone());
                if boosted_reward > current_budget {
                    return Err(BountyError::InsufficientBudget);
                }
                env.storage()
                    .instance()
                    .set(&DataKey::Budget, &(current_budget - boosted_reward));

                let total_transfer = record.stake_amount + boosted_reward;
                if total_transfer > 0 {
                    token_client.transfer(
                        &env.current_contract_address(),
                        &record.reporter,
                        &total_transfer,
                    );
                }
                (
                    boosted_reward,
                    current_rep.saturating_add(config.accepted_delta),
                )
            }
            AdjudicationOutcome::ValidDuplicate => {
                if record.stake_amount > 0 {
                    token_client.transfer(
                        &env.current_contract_address(),
                        &record.reporter,
                        &record.stake_amount,
                    );
                }
                (0, current_rep.saturating_add(config.duplicate_delta))
            }
            AdjudicationOutcome::Rejected => {
                if record.stake_amount > 0 {
                    let treasury: Address =
                        env.storage().instance().get(&DataKey::Treasury).unwrap();
                    token_client.transfer(
                        &env.current_contract_address(),
                        &treasury,
                        &record.stake_amount,
                    );
                }
                (0, current_rep.saturating_sub(config.rejected_penalty))
            }
            AdjudicationOutcome::Spam => {
                if record.stake_amount > 0 {
                    let treasury: Address =
                        env.storage().instance().get(&DataKey::Treasury).unwrap();
                    token_client.transfer(
                        &env.current_contract_address(),
                        &treasury,
                        &record.stake_amount,
                    );
                }
                (0, current_rep.saturating_sub(config.spam_penalty))
            }
        };

        record.adjudicated = true;
        record.outcome = outcome;
        record.payout_amount = payout;
        env.storage().persistent().set(&disc_key, &record);
        env.storage()
            .persistent()
            .set(&DataKey::Reputation(record.reporter.clone()), &new_rep);

        env.events().publish(
            (symbol_short!("adjudic"), record.reporter),
            (report_hash, outcome, payout, new_rep),
        );
        Ok(payout)
    }

    pub fn reputation_score(env: Env, reporter: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::Reputation(reporter))
            .unwrap_or(0)
    }

    /// Returns the bounded reward multiplier in basis points (`10_000` = 1.0x, capped at `max_multiplier_bps`).
    pub fn multiplier_bps(env: Env, reporter: Address) -> u32 {
        let rep = Self::reputation_score(env.clone(), reporter);
        let config = match Self::reputation_config(env) {
            Ok(c) => c,
            Err(_) => return 10_000,
        };
        let bonus = rep.saturating_mul(config.bonus_bps_per_point);
        10_000u32
            .saturating_add(bonus)
            .min(config.max_multiplier_bps)
    }

    pub fn reputation_config(env: Env) -> Result<ReputationConfig, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BountyError::NotInitialized)
    }

    pub fn disclosure(env: Env, report_hash: BytesN<32>) -> Result<StakedDisclosure, BountyError> {
        env.storage()
            .persistent()
            .get(&DataKey::Disclosure(report_hash))
            .ok_or(BountyError::ReportNotFound)
    }

    pub fn budget(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::Budget).unwrap_or(0)
    }

    fn require_admin(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn staking_reputation_multiplier_and_spam_slashing_flow() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let reporter = Address::generate(&env);
        let spammer = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_admin = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        usdc_admin.mint(&admin, &10_000_0000000);
        usdc_admin.mint(&reporter, &500_0000000);
        usdc_admin.mint(&spammer, &500_0000000);

        let contract_id = env.register_contract(None, RefractBounty);
        let client = RefractBountyClient::new(&env, &contract_id);
        // Min stake = 50 USDC, +100 bps per rep point, capped at 15,000 bps (1.5x)
        client.initialize(&admin, &usdc, &treasury, &50_0000000, &100, &15_000);
        client.fund_pool(&admin, &10_000_0000000);

        // Stake below minimum is rejected
        let r1 = BytesN::from_array(&env, &[1u8; 32]);
        assert_eq!(
            client.try_submit_disclosure(&reporter, &r1, &10_0000000),
            Err(Ok(BountyError::StakeBelowMinimum))
        );

        // 1. First accepted disclosure: 1.0x multiplier (1000 USDC) + 50 USDC stake refunded, rep -> 10
        client.submit_disclosure(&reporter, &r1, &50_0000000);
        let payout1 = client.adjudicate_disclosure(
            &admin,
            &r1,
            &AdjudicationOutcome::Accepted,
            &1_000_0000000,
        );
        assert_eq!(payout1, 1_000_0000000);
        assert_eq!(client.reputation_score(&reporter), 10);
        assert_eq!(client.multiplier_bps(&reporter), 11_000); // 1.1x

        // 2. Second accepted disclosure with 10 rep -> 1.1x multiplier (1100 USDC)
        let r2 = BytesN::from_array(&env, &[2u8; 32]);
        client.submit_disclosure(&reporter, &r2, &50_0000000);
        let payout2 = client.adjudicate_disclosure(
            &admin,
            &r2,
            &AdjudicationOutcome::Accepted,
            &1_000_0000000,
        );
        assert_eq!(payout2, 1_100_0000000);
        assert_eq!(client.reputation_score(&reporter), 20);

        // 3. Spammer submits spam report -> 50 USDC stake slashed to treasury, rep floored at 0
        let spam_hash = BytesN::from_array(&env, &[9u8; 32]);
        client.submit_disclosure(&spammer, &spam_hash, &50_0000000);
        let spam_payout =
            client.adjudicate_disclosure(&admin, &spam_hash, &AdjudicationOutcome::Spam, &0);
        assert_eq!(spam_payout, 0);
        assert_eq!(client.reputation_score(&spammer), 0);
        assert_eq!(usdc_token.balance(&treasury), 50_0000000);
    }
}
