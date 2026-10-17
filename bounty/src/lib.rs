//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with configurable protocol payout fee
//! routed to `RefractTreasury` (`#260`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

pub const MAX_FEE_BPS: u32 = 1_000; // 10% hard cap
const BPS_SCALE: i128 = 10_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeConfig {
    pub fee_bps: u32,
    pub treasury: Address,
    pub batch_sweep: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutQuote {
    pub gross: i128,
    pub net: i128,
    pub fee: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    FeeConfig,
    AccruedFees(Address),
    Award(BytesN<32>),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub gross_amount: i128,
    pub net_amount: i128,
    pub fee_amount: i128,
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
    FeeExceedsMaxCap = 7,
    NoFeesToSweep = 8,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        let default_fee = FeeConfig {
            fee_bps: 0,
            treasury: governance.clone(),
            batch_sweep: false,
        };
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::FeeConfig, &default_fee);
        Ok(())
    }

    /// Configures the protocol fee (capped at `MAX_FEE_BPS = 1_000` bps = 10%).
    pub fn set_fee_config(
        env: Env,
        caller: Address,
        fee_bps: u32,
        treasury: Address,
        batch_sweep: bool,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if fee_bps > MAX_FEE_BPS {
            return Err(BountyError::FeeExceedsMaxCap);
        }
        let cfg = FeeConfig {
            fee_bps,
            treasury,
            batch_sweep,
        };
        env.storage().instance().set(&DataKey::FeeConfig, &cfg);
        Ok(())
    }

    /// Quotes `(net, fee)` for a `gross` bounty award using floor rounding on `fee` so
    /// `net + fee == gross` holds for all inputs.
    pub fn quote_payout(env: Env, gross: i128) -> Result<PayoutQuote, BountyError> {
        if gross <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let cfg = Self::fee_config_internal(&env)?;
        let fee = (gross * (cfg.fee_bps as i128)) / BPS_SCALE;
        let net = gross - fee;
        Ok(PayoutQuote { gross, net, fee })
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

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        gross_amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<PayoutQuote, BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        let quote = Self::quote_payout(env.clone(), gross_amount)?;
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance_internal(&env)?;
        if gross_amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        let token_client = token::Client::new(&env, &token);
        let cfg = Self::fee_config_internal(&env)?;

        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - gross_amount));

        token_client.transfer(&env.current_contract_address(), &researcher, &quote.net);

        if quote.fee > 0 {
            if cfg.batch_sweep {
                let prev = Self::accrued_fees(env.clone(), token.clone());
                env.storage()
                    .persistent()
                    .set(&DataKey::AccruedFees(token.clone()), &(prev + quote.fee));
            } else {
                token_client.transfer(&env.current_contract_address(), &cfg.treasury, &quote.fee);
            }
            env.events().publish(
                (symbol_short!("fee_col"), token, cfg.treasury),
                (quote.gross, quote.net, quote.fee, cfg.batch_sweep),
            );
        }

        let record = AwardRecord {
            researcher,
            gross_amount: quote.gross,
            net_amount: quote.net,
            fee_amount: quote.fee,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(quote)
    }

    /// Permissionlessly sweeps accrued fees for `token` to the configured `treasury`.
    pub fn sweep_fees(env: Env, token: Address) -> Result<i128, BountyError> {
        let cfg = Self::fee_config_internal(&env)?;
        let accrued = Self::accrued_fees(env.clone(), token.clone());
        if accrued <= 0 {
            return Err(BountyError::NoFeesToSweep);
        }

        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token.clone()), &0i128);
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &cfg.treasury,
            &accrued,
        );

        env.events()
            .publish((symbol_short!("fee_swp"), token, cfg.treasury), accrued);
        Ok(accrued)
    }

    pub fn accrued_fees(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AccruedFees(token))
            .unwrap_or(0)
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance_internal(&env)
    }

    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
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

    fn fee_config_internal(env: &Env) -> Result<FeeConfig, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::FeeConfig)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn bounty_protocol_fee_immediate_batch_sweep_and_cap_enforcement() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let treasury = Address::generate(&env);
        let researcher = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);

        usdc_sac.mint(&gov, &2_000_0000000);
        client.fund(&gov, &2_000_0000000);

        // Over-cap fee (> 1000 bps) is rejected
        assert_eq!(
            client.try_set_fee_config(&gov, &1_001, &treasury, &false),
            Err(Ok(BountyError::FeeExceedsMaxCap))
        );

        // 1. Immediate transfer mode at 250 bps (2.5%)
        client.set_fee_config(&gov, &250, &treasury, &false);
        let q1 = client.award(
            &gov,
            &researcher,
            &1_000_0000000,
            &BytesN::from_array(&env, &[1u8; 32]),
        );
        assert_eq!(q1.net, 975_0000000);
        assert_eq!(q1.fee, 25_0000000);
        assert_eq!(q1.net + q1.fee, q1.gross);
        assert_eq!(usdc_token.balance(&researcher), 975_0000000);
        assert_eq!(usdc_token.balance(&treasury), 25_0000000);

        // 2. Batch sweep mode at 500 bps (5.0%)
        client.set_fee_config(&gov, &500, &treasury, &true);
        let q2 = client.award(
            &gov,
            &researcher,
            &1_000_0000000,
            &BytesN::from_array(&env, &[2u8; 32]),
        );
        assert_eq!(q2.net, 950_0000000);
        assert_eq!(q2.fee, 50_0000000);
        assert_eq!(client.accrued_fees(&usdc), 50_0000000);
        assert_eq!(client.sweep_fees(&usdc), 50_0000000);
        assert_eq!(client.accrued_fees(&usdc), 0);
        assert_eq!(usdc_token.balance(&treasury), 75_0000000);
    }
}
