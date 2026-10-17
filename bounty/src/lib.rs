#![no_std]

pub mod normalization;
pub use normalization::{
    normalize_inflow, normalize_outflow, to_protocol_units, to_token_units, NormalizationError,
    Rounding, PROTOCOL_DECIMALS, PROTOCOL_SCALE,
};

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone)]
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
    NormalizationError = 7,
    StateArchived = 8,
    InconsistentState = 9,
}

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address. There is
/// deliberately no single admin key with payout authority: a compromised admin
/// key authorizing arbitrary "bounty" payouts is a direct fund-drain vector.
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
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("init")),
            (governance, token),
        );

        Ok(())
    }

    /// Explicit post-restore consistency check and state integrity verification.
    /// Asserts that required instance storage entries are live (failing closed with
    /// `StateArchived` instead of reading zero/default values), verifies that the
    /// recorded balance is non-negative, and reconciles that the actual contract token
    /// balance is at least equal to the escrowed balance.
    pub fn on_restore(env: Env) -> Result<i128, BountyError> {
        let (balance, _token) = Self::assert_live_and_consistent(&env)?;
        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("restore")),
            balance,
        );
        Ok(balance)
    }

    /// Read-only integrity verification check.
    pub fn verify_integrity(env: Env) -> Result<bool, BountyError> {
        Self::assert_live_and_consistent(&env).map(|_| true)
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::get_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::get_balance(&env)?;
        env.storage().instance().set(&DataKey::Balance, &(balance + amount));

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("fund")),
            (from, amount),
        );

        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `report_hash` commitment is stored on-chain as an immutable,
    /// timestamped record of the rewarded disclosure.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::get_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::get_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::get_token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Award(report_hash.clone()), &record);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("award")),
            (researcher, amount, report_hash),
        );

        Ok(())
    }

    /// Award a bounty using canonical protocol units (1e7), normalizing to native token units
    /// with floor rounding (protecting against dust overpayment).
    /// Returns the actual native token amount awarded.
    pub fn award_canonical(
        env: Env,
        caller: Address,
        researcher: Address,
        protocol_amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<i128, BountyError> {
        let token = Self::get_token(&env)?;
        let decimals = token::Client::new(&env, &token).decimals();
        let token_amount = normalization::to_token_units(protocol_amount, decimals)
            .map_err(|_| BountyError::NormalizationError)?;
        Self::award(env, caller, researcher, token_amount, report_hash)?;
        Ok(token_amount)
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::get_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::get_governance(&env)
    }

    /// The token address.
    pub fn token(env: Env) -> Result<Address, BountyError> {
        Self::get_token(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn assert_live_and_consistent(env: &Env) -> Result<(i128, Address), BountyError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::StateArchived);
        }
        let token: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::StateArchived)?;
        let balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::StateArchived)?;
        if balance < 0 {
            return Err(BountyError::InconsistentState);
        }
        let real_balance = token::Client::new(env, &token).balance(&env.current_contract_address());
        if real_balance < balance {
            return Err(BountyError::InconsistentState);
        }
        Ok((balance, token))
    }

    fn get_governance(env: &Env) -> Result<Address, BountyError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::StateArchived);
        }
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::StateArchived)
    }

    fn get_token(env: &Env) -> Result<Address, BountyError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::StateArchived);
        }
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::StateArchived)
    }

    fn get_balance(env: &Env) -> Result<i128, BountyError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::StateArchived);
        }
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::StateArchived)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn test_bounty_canonical_award() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&governance, &token_id);

        let funder = Address::generate(&env);
        sac.mint(&funder, &100_000_000);
        client.fund(&funder, &100_000_000);

        let researcher = Address::generate(&env);
        let report_hash = BytesN::from_array(&env, &[42u8; 32]);

        // Stellar token has 7 decimals (1:1 with protocol scale)
        let awarded = client.award_canonical(&governance, &researcher, &50_000_000, &report_hash);
        assert_eq!(awarded, 50_000_000);
        assert_eq!(client.balance(), 50_000_000);

        let token_client = token::Client::new(&env, &token_id);
        assert_eq!(token_client.balance(&researcher), 50_000_000);
    }

    #[test]
    fn test_bounty_on_restore_and_integrity_checks() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&governance, &token_id);

        let funder = Address::generate(&env);
        sac.mint(&funder, &100_000_000);
        client.fund(&funder, &100_000_000);

        // Verification of live and consistent state
        let restored_bal = client.on_restore();
        assert_eq!(restored_bal, 100_000_000);
        assert!(client.verify_integrity());

        // Simulate inconsistency where contract recorded balance exceeds token reserves
        env.as_contract(&contract_id, || {
            env.storage().instance().set(&DataKey::Balance, &200_000_000i128);
        });
        assert_eq!(client.try_on_restore(), Err(Ok(BountyError::InconsistentState)));
        assert_eq!(client.try_verify_integrity(), Err(Ok(BountyError::InconsistentState)));
    }

    #[test]
    fn test_bounty_uninitialized_or_archived_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let res = client.try_on_restore();
        assert_eq!(res, Err(Ok(BountyError::StateArchived)));

        let res_bal = client.try_balance();
        assert_eq!(res_bal, Err(Ok(BountyError::StateArchived)));
    }
}

