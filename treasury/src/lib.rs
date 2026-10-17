#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    SelfTransfer = 5,
    AccountingImbalance = 6,
}

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

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    NextEntryId,
    JournalEntry(u64),
    AccountBalance(Address, Address),
    AssetAccounts(Address),
    TotalDebits(Address),
    TotalCredits(Address),
}

/// `RefractTreasury` is a token-custody vault with an append-only double-entry
/// accounting journal.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextEntryId, &1u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Record an inbound deposit into the treasury and post a balanced
    /// double-entry journal record (`debit = treasury`, `credit = depositor`).
    pub fn deposit(
        env: Env,
        depositor: Address,
        token: Address,
        amount: i128,
    ) -> Result<u64, TreasuryError> {
        depositor.require_auth();
        let treasury_addr = env.current_contract_address();
        let entry_id = Self::post_journal_entry(
            &env,
            treasury_addr.clone(),
            depositor.clone(),
            token.clone(),
            amount,
            symbol_short!("deposit"),
        )?;
        token::Client::new(&env, &token).transfer(&depositor, &treasury_addr, &amount);
        Ok(entry_id)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    /// Posts a balanced double-entry journal record (`debit = to`, `credit = treasury`).
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let treasury_addr = env.current_contract_address();
        Self::post_journal_entry(
            &env,
            to.clone(),
            treasury_addr.clone(),
            token.clone(),
            amount,
            symbol_short!("withdraw"),
        )
        .expect("journal posting failed");
        let client = token::Client::new(&env, &token);
        client.transfer(&treasury_addr, &to, &amount);
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

    /// Assert the double-entry accounting identity for `asset`:
    /// 1. `sum(debits) == sum(credits)`
    /// 2. `sum(balance_of(account, asset) for all touched accounts) == 0`
    pub fn assert_balanced(env: Env, asset: Address) -> Result<bool, TreasuryError> {
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
            return Err(TreasuryError::AccountingImbalance);
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
            return Err(TreasuryError::AccountingImbalance);
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
    ) -> Result<u64, TreasuryError> {
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if debit_account == credit_account {
            return Err(TreasuryError::SelfTransfer);
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated_and_maintains_double_entry_balance() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let depositor = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&depositor, &1_000);
        client.deposit(&depositor, &token_id, &1_000);
        assert!(client.assert_balanced(&token_id));
        assert_eq!(client.balance_of(&contract_id, &token_id), 1_000);
        assert_eq!(client.balance_of(&depositor, &token_id), -1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert!(client.assert_balanced(&token_id));
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
        assert_eq!(client.balance_of(&contract_id, &token_id), 600);
        assert_eq!(client.balance_of(&to, &token_id), 400);
        assert_eq!(client.journal_count(), 2);
    }

    #[test]
    fn rejects_self_transfer_and_zero_amount_postings() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        // Zero amount rejected.
        assert_eq!(
            client.try_deposit(&admin, &token_id, &0),
            Err(Ok(TreasuryError::InvalidAmount))
        );

        // Self-transfer (depositor == contract_id) rejected.
        assert_eq!(
            client.try_deposit(&contract_id, &token_id, &100),
            Err(Ok(TreasuryError::SelfTransfer))
        );
    }
}
