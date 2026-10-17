#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol, Vec,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEntry {
    pub entry_id: u64,
    pub debit_account: Address,
    pub credit_account: Address,
    pub asset: Address,
    pub amount: i128,
    pub action_tag: Symbol,
    pub ledger_sequence: u32,
}

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Monotonic double-entry journal counter.
    NextEntryId,
    /// Append-only journal entry by id.
    JournalEntry(u64),
    /// Per-(account, asset) net balance `(debits - credits)`.
    AccountBalance(Address, Address),
    /// Set of accounts touched for a given asset.
    AssetAccounts(Address),
    /// Sum of all debits posted for an asset.
    TotalDebits(Address),
    /// Sum of all credits posted for an asset.
    TotalCredits(Address),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
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
    SelfTransfer = 7,
    AccountingImbalance = 8,
}

/// Governance-controlled bug bounty escrow with an append-only double-entry
/// accounting journal.
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
        env.storage().instance().set(&DataKey::NextEntryId, &1u64);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        let escrow_addr = env.current_contract_address();
        Self::post_journal_entry(
            &env,
            escrow_addr.clone(),
            from.clone(),
            token.clone(),
            amount,
            symbol_short!("fund"),
        )?;
        token::Client::new(&env, &token).transfer(&from, &escrow_addr, &amount);
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
        let escrow_addr = env.current_contract_address();
        Self::post_journal_entry(
            &env,
            researcher.clone(),
            escrow_addr.clone(),
            token.clone(),
            amount,
            symbol_short!("award"),
        )?;

        token::Client::new(&env, &token).transfer(&escrow_addr, &researcher, &amount);
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

    /// Net double-entry journal balance `(debits - credits)` for `(account, asset)`.
    pub fn balance_of(env: Env, account: Address, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AccountBalance(account, asset))
            .unwrap_or(0)
    }

    /// Total gross posted volume (`total_debits == total_credits`) for `asset`.
    pub fn total_supply(env: Env, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalDebits(asset))
            .unwrap_or(0)
    }

    /// Total number of journal entries recorded.
    pub fn journal_count(env: Env) -> u64 {
        let next: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextEntryId)
            .unwrap_or(1);
        next.saturating_sub(1)
    }

    /// Fetch a specific journal entry by `entry_id`.
    pub fn journal_entry(env: Env, entry_id: u64) -> Option<JournalEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::JournalEntry(entry_id))
    }

    /// Assert the double-entry accounting identity for `asset`.
    pub fn assert_balanced(env: Env, asset: Address) -> Result<bool, BountyError> {
        let debits: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDebits(asset.clone()))
            .unwrap_or(0);
        let credits: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalCredits(asset.clone()))
            .unwrap_or(0);
        if debits != credits {
            return Err(BountyError::AccountingImbalance);
        }

        let accounts: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::AssetAccounts(asset.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        let mut net_sum: i128 = 0;
        for acc in accounts.iter() {
            net_sum += Self::balance_of(env.clone(), acc, asset.clone());
        }
        if net_sum != 0 {
            return Err(BountyError::AccountingImbalance);
        }

        let escrow_bal = Self::read_balance(&env)?;
        let journal_escrow_bal =
            Self::balance_of(env.clone(), env.current_contract_address(), asset);
        if escrow_bal != journal_escrow_bal {
            return Err(BountyError::AccountingImbalance);
        }

        Ok(true)
    }

    fn post_journal_entry(
        env: &Env,
        debit_account: Address,
        credit_account: Address,
        asset: Address,
        amount: i128,
        action_tag: Symbol,
    ) -> Result<u64, BountyError> {
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if debit_account == credit_account {
            return Err(BountyError::SelfTransfer);
        }

        let entry_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextEntryId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextEntryId, &(entry_id + 1));

        let entry = JournalEntry {
            entry_id,
            debit_account: debit_account.clone(),
            credit_account: credit_account.clone(),
            asset: asset.clone(),
            amount,
            action_tag: action_tag.clone(),
            ledger_sequence: env.ledger().sequence(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::JournalEntry(entry_id), &entry);

        Self::track_account(env, &asset, &debit_account);
        Self::track_account(env, &asset, &credit_account);

        let debit_bal = Self::balance_of(env.clone(), debit_account.clone(), asset.clone());
        let credit_bal = Self::balance_of(env.clone(), credit_account.clone(), asset.clone());
        env.storage().persistent().set(
            &DataKey::AccountBalance(debit_account.clone(), asset.clone()),
            &(debit_bal + amount),
        );
        env.storage().persistent().set(
            &DataKey::AccountBalance(credit_account.clone(), asset.clone()),
            &(credit_bal - amount),
        );

        let debits: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalDebits(asset.clone()))
            .unwrap_or(0);
        let credits: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalCredits(asset.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalDebits(asset.clone()), &(debits + amount));
        env.storage()
            .persistent()
            .set(&DataKey::TotalCredits(asset.clone()), &(credits + amount));

        env.events().publish(
            (Symbol::new(env, "journal_posted"), entry_id),
            (
                debit_account,
                credit_account,
                asset,
                amount,
                action_tag,
                entry.ledger_sequence,
            ),
        );

        Ok(entry_id)
    }

    fn track_account(env: &Env, asset: &Address, account: &Address) {
        let mut accounts: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::AssetAccounts(asset.clone()))
            .unwrap_or_else(|| Vec::new(env));
        if !accounts.contains(account) {
            accounts.push_back(account.clone());
            env.storage()
                .persistent()
                .set(&DataKey::AssetAccounts(asset.clone()), &accounts);
        }
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
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn fund_and_award_maintain_double_entry_identity() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&funder, &1_000);
        client.initialize(&governance, &token_id);

        client.fund(&funder, &600);
        assert!(client.assert_balanced(&token_id));
        assert_eq!(client.balance_of(&contract_id, &token_id), 600);
        assert_eq!(client.balance_of(&funder, &token_id), -600);

        let report_hash = BytesN::from_array(&env, &[9u8; 32]);
        client.award(&governance, &researcher, &250, &report_hash);
        assert!(client.assert_balanced(&token_id));
        assert_eq!(client.balance_of(&contract_id, &token_id), 350);
        assert_eq!(client.balance_of(&researcher, &token_id), 250);
        assert_eq!(client.journal_count(), 2);

        // Self-transfer rejected if funder == contract_id.
        assert_eq!(
            client.try_fund(&contract_id, &100),
            Err(Ok(BountyError::SelfTransfer))
        );
    }
}
