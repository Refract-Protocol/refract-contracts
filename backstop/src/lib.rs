#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    SelfTransfer = 8,
    AccountingImbalance = 9,
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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&"config") {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool,
            admin,
            token,
            premium_share_bps,
        };
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(&"next_entry_id", &1u64);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let backstop_addr = env.current_contract_address();
        Self::post_journal_entry(
            &env,
            backstop_addr.clone(),
            staker.clone(),
            config.token.clone(),
            amount,
            symbol_short!("deposit"),
        )?;

        token::Client::new(&env, &config.token).transfer(&staker, &backstop_addr, &amount);

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        let backstop_addr = env.current_contract_address();
        if amount > 0 {
            Self::post_journal_entry(
                &env,
                staker.clone(),
                backstop_addr.clone(),
                config.token.clone(),
                amount,
                symbol_short!("withdraw"),
            )?;
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(&backstop_addr, &staker, &amount);
        }

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        let backstop_addr = env.current_contract_address();
        Self::post_journal_entry(
            &env,
            config.pool.clone(),
            backstop_addr.clone(),
            config.token.clone(),
            covered,
            symbol_short!("shortfall"),
        )?;

        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(&backstop_addr, &config.pool, &covered);

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        if cut > 0 {
            Self::post_journal_entry(
                &env,
                env.current_contract_address(),
                config.pool.clone(),
                config.token.clone(),
                cut,
                symbol_short!("premium"),
            )?;
        }
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    /// Net double-entry journal balance `(debits - credits)` for `(account, asset)`.
    pub fn balance_of(env: Env, account: Address, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("acct_bal", account, asset))
            .unwrap_or(0)
    }

    /// Total gross posted volume (`total_debits == total_credits`) for `asset`.
    pub fn total_supply(env: Env, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("tot_debit", asset))
            .unwrap_or(0)
    }

    /// Total number of journal entries recorded.
    pub fn journal_count(env: Env) -> u64 {
        let next: u64 = env.storage().instance().get(&"next_entry_id").unwrap_or(1);
        next.saturating_sub(1)
    }

    /// Fetch a specific journal entry by `entry_id`.
    pub fn journal_entry(env: Env, entry_id: u64) -> Option<JournalEntry> {
        env.storage().persistent().get(&("journal", entry_id))
    }

    /// Assert the double-entry accounting identity for `asset`.
    pub fn assert_balanced(env: Env, asset: Address) -> Result<bool, BackstopError> {
        let debits: i128 = env
            .storage()
            .persistent()
            .get(&("tot_debit", asset.clone()))
            .unwrap_or(0);
        let credits: i128 = env
            .storage()
            .persistent()
            .get(&("tot_credit", asset.clone()))
            .unwrap_or(0);
        if debits != credits {
            return Err(BackstopError::AccountingImbalance);
        }

        let accounts: Vec<Address> = env
            .storage()
            .persistent()
            .get(&("asset_accts", asset.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        let mut net_sum: i128 = 0;
        for acc in accounts.iter() {
            net_sum += Self::balance_of(env.clone(), acc, asset.clone());
        }
        if net_sum != 0 {
            return Err(BackstopError::AccountingImbalance);
        }

        let state = Self::read_state(&env)?;
        let journal_capital = Self::balance_of(env.clone(), env.current_contract_address(), asset);
        if state.total_capital != journal_capital {
            return Err(BackstopError::AccountingImbalance);
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
    ) -> Result<u64, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        if debit_account == credit_account {
            return Err(BackstopError::SelfTransfer);
        }

        let entry_id: u64 = env.storage().instance().get(&"next_entry_id").unwrap_or(1);
        env.storage()
            .instance()
            .set(&"next_entry_id", &(entry_id + 1));

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
            .set(&("journal", entry_id), &entry);

        Self::track_account(env, &asset, &debit_account);
        Self::track_account(env, &asset, &credit_account);

        let debit_bal = Self::balance_of(env.clone(), debit_account.clone(), asset.clone());
        let credit_bal = Self::balance_of(env.clone(), credit_account.clone(), asset.clone());
        env.storage().persistent().set(
            &("acct_bal", debit_account.clone(), asset.clone()),
            &(debit_bal + amount),
        );
        env.storage().persistent().set(
            &("acct_bal", credit_account.clone(), asset.clone()),
            &(credit_bal - amount),
        );

        let debits: i128 = env
            .storage()
            .persistent()
            .get(&("tot_debit", asset.clone()))
            .unwrap_or(0);
        let credits: i128 = env
            .storage()
            .persistent()
            .get(&("tot_credit", asset.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&("tot_debit", asset.clone()), &(debits + amount));
        env.storage()
            .persistent()
            .set(&("tot_credit", asset.clone()), &(credits + amount));

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
            .get(&("asset_accts", asset.clone()))
            .unwrap_or_else(|| Vec::new(env));
        if !accounts.contains(account) {
            accounts.push_back(account.clone());
            env.storage()
                .persistent()
                .set(&("asset_accts", asset.clone()), &accounts);
        }
    }

    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn backstop_lifecycle_maintains_double_entry_identity() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&staker, &1_000);
        client.initialize(&pool, &admin, &token_id, &2_000);

        client.deposit(&staker, &1_000);
        assert!(client.assert_balanced(&token_id));

        client.receive_premium(&500);
        assert!(client.assert_balanced(&token_id));

        client.cover_shortfall(&pool, &300);
        assert!(client.assert_balanced(&token_id));

        client.withdraw(&staker, &500);
        assert!(client.assert_balanced(&token_id));
        assert_eq!(client.journal_count(), 4);
    }
}
