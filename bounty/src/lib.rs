#![no_std]

pub mod normalization;
pub use normalization::{
    normalize_inflow, normalize_outflow, to_protocol_units, to_token_units, NormalizationError,
    Rounding, PROTOCOL_DECIMALS, PROTOCOL_SCALE,
};

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN,
    Env, Symbol,
};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Configured refill rules for a bounty program.
    RefillRules(u64),
    /// Current balance allocated to a bounty program.
    ProgramBalance(u64),
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

/// Refill rules configured for a bounty program.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefillRules {
    pub min_threshold: i128,
    pub refill_amount: i128,
    pub allowance_source: Address,
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
    ProgramBelowThreshold = 10,
    RefillNotNeeded = 11,
    RulesNotFound = 12,
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

    /// Top up the general reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::get_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::get_balance(&env)?;
        let new_balance = balance
            .checked_add(amount)
            .ok_or(BountyError::InvalidAmount)?;
        env.storage().instance().set(&DataKey::Balance, &new_balance);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("fund")),
            (from, amount),
        );

        Ok(())
    }

    /// Top up a specific bounty program escrow balance on demand.
    /// Transfers tokens from the sponsor into the contract escrow and increments
    /// both the overall contract balance and the program-specific balance.
    /// Emits structured `ProgramToppedUp` event.
    pub fn top_up_program(
        env: Env,
        sponsor: Address,
        program_id: u64,
        amount: i128,
    ) -> Result<i128, BountyError> {
        sponsor.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::get_token(&env)?;
        token::Client::new(&env, &token).transfer(&sponsor, &env.current_contract_address(), &amount);

        let total_balance = Self::get_balance(&env)?;
        let new_total = total_balance
            .checked_add(amount)
            .ok_or(BountyError::InvalidAmount)?;
        env.storage().instance().set(&DataKey::Balance, &new_total);

        let prog_balance = Self::get_program_balance(&env, program_id);
        let new_prog_balance = prog_balance
            .checked_add(amount)
            .ok_or(BountyError::InvalidAmount)?;
        env.storage()
            .persistent()
            .set(&DataKey::ProgramBalance(program_id), &new_prog_balance);

        env.events().publish(
            (
                Symbol::new(&env, "ProgramToppedUp"),
                program_id,
            ),
            (sponsor, amount, new_prog_balance),
        );

        Ok(new_prog_balance)
    }

    /// Configure low-balance refill rules for a bounty program.
    /// Gated strictly by the governance authority.
    pub fn configure_refill_rules(
        env: Env,
        admin: Address,
        program_id: u64,
        min_threshold: i128,
        refill_amount: i128,
        allowance_source: Address,
    ) -> Result<(), BountyError> {
        admin.require_auth();
        let governance = Self::get_governance(&env)?;
        if admin != governance {
            return Err(BountyError::Unauthorized);
        }
        if min_threshold < 0 || refill_amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let rules = RefillRules {
            min_threshold,
            refill_amount,
            allowance_source: allowance_source.clone(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::RefillRules(program_id), &rules);

        let current_balance = Self::get_program_balance(&env, program_id);
        if current_balance < min_threshold {
            env.events().publish(
                (
                    Symbol::new(&env, "LowBalanceWarning"),
                    program_id,
                ),
                (current_balance, min_threshold),
            );
        }

        Ok(())
    }

    /// Auto-refill a bounty program if its balance falls below `min_threshold`.
    /// Callable by keepers; draws `refill_amount` from `allowance_source` into escrow.
    /// Emits structured `ProgramAutoRefilled` event.
    pub fn auto_refill(
        env: Env,
        keeper: Address,
        program_id: u64,
    ) -> Result<i128, BountyError> {
        keeper.require_auth();

        let rules = env
            .storage()
            .persistent()
            .get::<_, RefillRules>(&DataKey::RefillRules(program_id))
            .ok_or(BountyError::RulesNotFound)?;

        let current_balance = Self::get_program_balance(&env, program_id);
        if current_balance >= rules.min_threshold {
            return Err(BountyError::RefillNotNeeded);
        }

        let token = Self::get_token(&env)?;
        token::Client::new(&env, &token).transfer_from(
            &env.current_contract_address(),
            &rules.allowance_source,
            &env.current_contract_address(),
            &rules.refill_amount,
        );

        let total_balance = Self::get_balance(&env)?;
        let new_total = total_balance
            .checked_add(rules.refill_amount)
            .ok_or(BountyError::InvalidAmount)?;
        env.storage().instance().set(&DataKey::Balance, &new_total);

        let new_prog_balance = current_balance
            .checked_add(rules.refill_amount)
            .ok_or(BountyError::InvalidAmount)?;
        env.storage()
            .persistent()
            .set(&DataKey::ProgramBalance(program_id), &new_prog_balance);

        env.events().publish(
            (
                Symbol::new(&env, "ProgramAutoRefilled"),
                program_id,
            ),
            (keeper, rules.allowance_source, rules.refill_amount, new_prog_balance),
        );

        Ok(new_prog_balance)
    }

    /// Check if a program's balance is below its configured `min_threshold`.
    /// Emits `LowBalanceWarning` if below threshold.
    pub fn check_balance_threshold(env: Env, program_id: u64) -> Result<bool, BountyError> {
        let rules_opt = env
            .storage()
            .persistent()
            .get::<_, RefillRules>(&DataKey::RefillRules(program_id));
        if let Some(rules) = rules_opt {
            let current_balance = Self::get_program_balance(&env, program_id);
            if current_balance < rules.min_threshold {
                env.events().publish(
                    (
                        Symbol::new(&env, "LowBalanceWarning"),
                        program_id,
                    ),
                    (current_balance, rules.min_threshold),
                );
                return Ok(true);
            }
        }
        Ok(false)
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

    /// Award a bounty funded by a specific bounty program.
    /// Checks that the program has sufficient balance and is not below its configured
    /// `min_threshold`. Deducts from both the program balance and the contract total balance.
    /// Emits `LowBalanceWarning` if post-award balance drops below `min_threshold`.
    pub fn award_from_program(
        env: Env,
        caller: Address,
        program_id: u64,
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

        let prog_balance = Self::get_program_balance(&env, program_id);
        let rules_opt = env
            .storage()
            .persistent()
            .get::<_, RefillRules>(&DataKey::RefillRules(program_id));

        if let Some(ref rules) = rules_opt {
            if prog_balance < rules.min_threshold {
                return Err(BountyError::ProgramBelowThreshold);
            }
        }

        if amount > prog_balance {
            return Err(BountyError::InsufficientBalance);
        }

        let total_balance = Self::get_balance(&env)?;
        if amount > total_balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::get_token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);

        let new_total = total_balance - amount;
        env.storage().instance().set(&DataKey::Balance, &new_total);

        let new_prog_bal = prog_balance - amount;
        env.storage()
            .persistent()
            .set(&DataKey::ProgramBalance(program_id), &new_prog_bal);

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

        if let Some(rules) = rules_opt {
            if new_prog_bal < rules.min_threshold {
                env.events().publish(
                    (
                        Symbol::new(&env, "LowBalanceWarning"),
                        program_id,
                    ),
                    (new_prog_bal, rules.min_threshold),
                );
            }
        }

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

    /// Current funds held for a specific bounty program.
    pub fn program_balance(env: Env, program_id: u64) -> i128 {
        Self::get_program_balance(&env, program_id)
    }

    /// Configured refill rules for a specific bounty program.
    pub fn get_refill_rules(env: Env, program_id: u64) -> Option<RefillRules> {
        env.storage()
            .persistent()
            .get::<_, RefillRules>(&DataKey::RefillRules(program_id))
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

    fn get_program_balance(env: &Env, program_id: u64) -> i128 {
        env.storage()
            .persistent()
            .get::<_, i128>(&DataKey::ProgramBalance(program_id))
            .unwrap_or(0i128)
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
    use soroban_sdk::{testutils::Address as _, testutils::Events as _, Env, TryFromVal};

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

    #[test]
    fn test_top_up_program_success_and_events() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&governance, &token_id);

        let sponsor = Address::generate(&env);
        sac.mint(&sponsor, &500_000_000);

        // Top up program 1 with 200_000_000
        let prog_bal = client.top_up_program(&sponsor, &1, &200_000_000);
        assert_eq!(prog_bal, 200_000_000);
        assert_eq!(client.program_balance(&1), 200_000_000);
        assert_eq!(client.balance(), 200_000_000);

        // Top up again with 100_000_000
        let prog_bal_2 = client.top_up_program(&sponsor, &1, &100_000_000);
        assert_eq!(prog_bal_2, 300_000_000);
        assert_eq!(client.program_balance(&1), 300_000_000);
        assert_eq!(client.balance(), 300_000_000);

        // Check ProgramToppedUp event emitted
        let events = env.events().all();
        let topped_up_found = events.iter().any(|(_, topics, _)| {
            topics.iter().any(|t| {
                Symbol::try_from_val(&env, &t)
                    .map(|s| s == Symbol::new(&env, "ProgramToppedUp"))
                    .unwrap_or(false)
            })
        });
        assert!(topped_up_found, "ProgramToppedUp event must be emitted");

        // Reject invalid top-up amount
        assert_eq!(
            client.try_top_up_program(&sponsor, &1, &0),
            Err(Ok(BountyError::InvalidAmount))
        );
        assert_eq!(
            client.try_top_up_program(&sponsor, &1, &-10),
            Err(Ok(BountyError::InvalidAmount))
        );
    }

    #[test]
    fn test_configure_refill_rules_and_low_balance_warning() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();

        client.initialize(&governance, &token_id);

        let treasury_source = Address::generate(&env);
        let stranger = Address::generate(&env);

        // Stranger cannot configure refill rules
        assert_eq!(
            client.try_configure_refill_rules(&stranger, &1, &100_000_000, &500_000_000, &treasury_source),
            Err(Ok(BountyError::Unauthorized))
        );

        // Governance configures rules when program balance is 0 (< 100_000_000) -> emits LowBalanceWarning
        client.configure_refill_rules(&governance, &1, &100_000_000, &500_000_000, &treasury_source);

        let rules = client.get_refill_rules(&1).expect("Rules should be saved");
        assert_eq!(rules.min_threshold, 100_000_000);
        assert_eq!(rules.refill_amount, 500_000_000);
        assert_eq!(rules.allowance_source, treasury_source);

        // Check LowBalanceWarning was emitted
        let events = env.events().all();
        let warning_found = events.iter().any(|(_, topics, _)| {
            topics.iter().any(|t| {
                Symbol::try_from_val(&env, &t)
                    .map(|s| s == Symbol::new(&env, "LowBalanceWarning"))
                    .unwrap_or(false)
            })
        });
        assert!(warning_found, "LowBalanceWarning event must be emitted");

        // check_balance_threshold returns true
        assert_eq!(client.check_balance_threshold(&1), true);
    }

    #[test]
    fn test_auto_refill_lifecycle_and_rejections() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&governance, &token_id);

        let treasury = Address::generate(&env);
        let keeper = Address::generate(&env);

        sac.mint(&treasury, &1_000_000_000);
        let token_client = token::Client::new(&env, &token_id);
        token_client.approve(&treasury, &contract_id, &1_000_000_000, &100_000);

        // Attempt refill before rules configured -> RulesNotFound
        assert_eq!(
            client.try_auto_refill(&keeper, &1),
            Err(Ok(BountyError::RulesNotFound))
        );

        // Configure rules: min_threshold = 100_000_000, refill = 400_000_000
        client.configure_refill_rules(&governance, &1, &100_000_000, &400_000_000, &treasury);

        // Execute auto_refill via keeper
        let new_bal = client.auto_refill(&keeper, &1);
        assert_eq!(new_bal, 400_000_000);
        assert_eq!(client.program_balance(&1), 400_000_000);
        assert_eq!(client.balance(), 400_000_000);

        let token_client = token::Client::new(&env, &token_id);
        assert_eq!(token_client.balance(&contract_id), 400_000_000);
        assert_eq!(token_client.balance(&treasury), 600_000_000);

        // Attempting refill when balance is above threshold -> RefillNotNeeded
        assert_eq!(
            client.try_auto_refill(&keeper, &1),
            Err(Ok(BountyError::RefillNotNeeded))
        );

        // Verify ProgramAutoRefilled event
        let events = env.events().all();
        let refilled_found = events.iter().any(|(_, topics, _)| {
            topics.iter().any(|t| {
                Symbol::try_from_val(&env, &t)
                    .map(|s| s == Symbol::new(&env, "ProgramAutoRefilled"))
                    .unwrap_or(false)
            })
        });
        assert!(refilled_found, "ProgramAutoRefilled event must be emitted");
    }

    #[test]
    fn test_award_from_program_blocks_when_below_threshold() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&governance, &token_id);

        let sponsor = Address::generate(&env);
        sac.mint(&sponsor, &500_000_000);

        // Top up 250_000_000
        client.top_up_program(&sponsor, &1, &250_000_000);

        let treasury = Address::generate(&env);
        // Configure rules with threshold 100_000_000
        client.configure_refill_rules(&governance, &1, &100_000_000, &300_000_000, &treasury);

        let researcher = Address::generate(&env);
        let report_hash1 = BytesN::from_array(&env, &[1u8; 32]);

        // Award 160_000_000 -> balance becomes 90_000_000 (< 100_000_000 threshold)
        client.award_from_program(&governance, &1, &researcher, &160_000_000, &report_hash1);
        assert_eq!(client.program_balance(&1), 90_000_000);
        assert_eq!(client.balance(), 90_000_000);

        // Next award must be BLOCKED because program balance is below threshold
        let report_hash2 = BytesN::from_array(&env, &[2u8; 32]);
        let blocked = client.try_award_from_program(&governance, &1, &researcher, &10_000_000, &report_hash2);
        assert_eq!(blocked, Err(Ok(BountyError::ProgramBelowThreshold)));

        // Keeper triggers auto_refill
        sac.mint(&treasury, &500_000_000);
        let token_client = token::Client::new(&env, &token_id);
        token_client.approve(&treasury, &contract_id, &500_000_000, &100_000);
        let keeper = Address::generate(&env);
        client.auto_refill(&keeper, &1);

        assert_eq!(client.program_balance(&1), 390_000_000);
        assert_eq!(client.balance(), 390_000_000);

        // Now award succeeds!
        client.award_from_program(&governance, &1, &researcher, &10_000_000, &report_hash2);
        assert_eq!(client.program_balance(&1), 380_000_000);
    }
}
