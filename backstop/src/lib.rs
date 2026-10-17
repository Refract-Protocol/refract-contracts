#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

/// Default deposit lock-up length in ledgers (~1 day at 5s/ledger = 17,280 ledgers).
pub const DEFAULT_LOCKUP_LEDGERS: u32 = 100;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    PendingLoss,
    NextLotId,
    Lot(u64),
    StakerLots(Address),
    Shares(Address),
}

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
    PendingLossFreeze = 8,
    NoMatureCapital = 9,
    LotNotMatured = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
    pub lockup_ledgers: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositLot {
    pub lot_id: u64,
    pub staker: Address,
    pub principal: i128,
    pub shares: i128,
    pub deposited_ledger: u32,
    pub matures_at_ledger: u32,
    pub matured: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub mature_shares: i128,
    pub mature_capital: i128,
    pub unmatured_principal: i128,
    pub premium_income: i128,
    pub unabsorbed_shortfall: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche with default lock-up epochs.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
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
            lockup_ledgers: DEFAULT_LOCKUP_LEDGERS,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::PendingLoss, &false);
        env.storage().instance().set(&DataKey::NextLotId, &0u64);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                mature_shares: 0,
                mature_capital: 0,
                unmatured_principal: 0,
                premium_income: 0,
                unabsorbed_shortfall: 0,
            },
        );
        Ok(())
    }

    /// Update the lock-up length (in ledgers) for future deposits.
    /// Does not retroactively alter `matures_at_ledger` on existing deposit lots.
    pub fn set_lockup_ledgers(
        env: Env,
        caller: Address,
        lockup_ledgers: u32,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.lockup_ledgers = lockup_ledgers;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    /// Flag or clear a pending loss event. Callable by `pool` or `admin`.
    /// While `pending == true`, new deposits are rejected to prevent sniping.
    pub fn set_pending_loss(env: Env, caller: Address, pending: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingLoss, &pending);
        env.events()
            .publish((symbol_short!("PendLoss"), caller), pending);
        Ok(())
    }

    pub fn is_pending_loss(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::PendingLoss)
            .unwrap_or(false)
    }

    /// Promote any unmatured deposit lots whose `matures_at_ledger <= env.ledger().sequence()`
    /// into the mature capital/shares pool at the current mature share price.
    pub fn sync_matured_lots(env: Env) -> Result<BackstopState, BackstopError> {
        Self::sync_internal(&env)
    }

    /// Deposit `amount` of token into a new lock-up lot.
    /// Rejected while a loss event is pending (`PendingLossFreeze`).
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<u64, BackstopError> {
        staker.require_auth();
        if Self::is_pending_loss(env.clone()) {
            return Err(BackstopError::PendingLossFreeze);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::sync_internal(&env)?;

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let lot_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextLotId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextLotId, &(lot_id + 1));

        let now_seq = env.ledger().sequence();
        let matures_at = now_seq.saturating_add(config.lockup_ledgers);
        let matured = config.lockup_ledgers == 0;

        let shares = if matured {
            let s = Self::calc_shares(amount, state.mature_shares, state.mature_capital);
            state.mature_shares += s;
            state.mature_capital += amount;
            let key = DataKey::Shares(staker.clone());
            let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
            env.storage().persistent().set(&key, &(prev + s));
            s
        } else {
            state.unmatured_principal += amount;
            0
        };

        state.total_shares = state.mature_shares;
        state.total_capital = state.mature_capital + state.unmatured_principal;
        env.storage().instance().set(&DataKey::State, &state);

        let lot = DepositLot {
            lot_id,
            staker: staker.clone(),
            principal: amount,
            shares,
            deposited_ledger: now_seq,
            matures_at_ledger: matures_at,
            matured,
        };
        env.storage().persistent().set(&DataKey::Lot(lot_id), &lot);

        let staker_lots_key = DataKey::StakerLots(staker);
        let mut lots: Vec<u64> = env
            .storage()
            .persistent()
            .get(&staker_lots_key)
            .unwrap_or_else(|| Vec::new(&env));
        lots.push_back(lot_id);
        env.storage().persistent().set(&staker_lots_key, &lots);

        Ok(lot_id)
    }

    /// Burn `shares` of matured stake and return proportional mature capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::sync_internal(&env)?;

        let key = DataKey::Shares(staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.mature_shares, state.mature_capital);
        if amount > state.mature_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.mature_shares -= shares;
        state.mature_capital -= amount;
        state.total_shares = state.mature_shares;
        state.total_capital = state.mature_capital + state.unmatured_principal;
        env.storage().instance().set(&DataKey::State, &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        Ok(amount)
    }

    /// Drain up to `amount` of **mature** backstop capital to cover a claim shortfall.
    ///
    /// Unmatured deposit lots are excluded from loss absorption; if `amount`
    /// exceeds `mature_capital`, `mature_capital` is drained to 0 and the
    /// difference is recorded in `state.unabsorbed_shortfall`.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::sync_internal(&env)?;
        let covered = amount.min(state.mature_capital);
        let shortfall = amount - covered;

        state.mature_capital -= covered;
        state.unabsorbed_shortfall += shortfall;
        state.total_capital = state.mature_capital + state.unmatured_principal;
        env.storage().instance().set(&DataKey::State, &state);
        env.storage().instance().set(&DataKey::PendingLoss, &false);

        if covered > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &covered,
            );
        }

        Ok(covered)
    }

    /// Route a slice of premium income to **mature** backstop stakers only.
    /// Fails with `NoMatureCapital` if no stake has matured yet.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::sync_internal(&env)?;
        if state.mature_shares == 0 || state.mature_capital == 0 {
            return Err(BackstopError::NoMatureCapital);
        }

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.mature_capital += cut;
        state.total_capital = state.mature_capital + state.unmatured_principal;
        env.storage().instance().set(&DataKey::State, &state);

        Ok(cut)
    }

    pub fn get_lot(env: Env, lot_id: u64) -> Option<DepositLot> {
        let _ = Self::sync_internal(&env);
        env.storage().persistent().get(&DataKey::Lot(lot_id))
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        let _ = Self::sync_internal(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Shares(staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::sync_internal(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    fn sync_internal(env: &Env) -> Result<BackstopState, BackstopError> {
        let mut state: BackstopState = env
            .storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)?;
        let next_lot_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextLotId)
            .unwrap_or(0);
        let now_seq = env.ledger().sequence();

        for lot_id in 0..next_lot_id {
            let lot_key = DataKey::Lot(lot_id);
            if let Some(mut lot) = env.storage().persistent().get::<_, DepositLot>(&lot_key) {
                if !lot.matured && now_seq >= lot.matures_at_ledger {
                    let shares =
                        Self::calc_shares(lot.principal, state.mature_shares, state.mature_capital);
                    lot.matured = true;
                    lot.shares = shares;
                    env.storage().persistent().set(&lot_key, &lot);

                    state.unmatured_principal -= lot.principal;
                    state.mature_capital += lot.principal;
                    state.mature_shares += shares;

                    let share_key = DataKey::Shares(lot.staker);
                    let prev: i128 = env.storage().persistent().get(&share_key).unwrap_or(0);
                    env.storage().persistent().set(&share_key, &(prev + shares));
                }
            }
        }

        state.total_shares = state.mature_shares;
        state.total_capital = state.mature_capital + state.unmatured_principal;
        env.storage().instance().set(&DataKey::State, &state);
        Ok(state)
    }

    fn calc_shares(amount: i128, mature_shares: i128, mature_capital: i128) -> i128 {
        if mature_shares == 0 || mature_capital == 0 {
            amount
        } else {
            amount * mature_shares / mature_capital
        }
    }

    fn quote_withdrawal(shares: i128, mature_shares: i128, mature_capital: i128) -> i128 {
        if mature_shares == 0 {
            0
        } else {
            shares * mature_capital / mature_shares
        }
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    fn setup() -> (
        Env,
        RefractBackstopClient<'static>,
        Address,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 1_000;
        });

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker_a = Address::generate(&env);
        let staker_b = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&staker_a, &10_000);
        sac.mint(&staker_b, &10_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &2_000);

        (env, client, pool, admin, staker_a, staker_b)
    }

    #[test]
    fn unmatured_lots_do_not_earn_premium_or_absorb_losses_until_epoch_boundary() {
        let (env, client, pool, admin, staker_a, staker_b) = setup();

        // Staker A deposits 1_000 at ledger 1_000 (matures at 1_100).
        let lot_a = client.deposit(&staker_a, &1_000);
        assert_eq!(client.shares_of(&staker_a), 0);
        assert_eq!(
            client.try_receive_premium(&500),
            Err(Ok(BackstopError::NoMatureCapital))
        );

        // Changing lockup_ledgers to 200 affects only future deposits, not lot_a!
        client.set_lockup_ledgers(&admin, &200);
        assert_eq!(client.get_lot(&lot_a).unwrap().matures_at_ledger, 1_100);

        // Advance to 1_100: Staker A matures, Staker B deposits 1_000 (matures at 1_300).
        env.ledger().with_mut(|li| {
            li.sequence_number = 1_100;
        });
        let lot_b = client.deposit(&staker_b, &1_000);
        assert_eq!(client.get_lot(&lot_b).unwrap().matures_at_ledger, 1_300);
        assert_eq!(client.shares_of(&staker_a), 1_000);
        assert_eq!(client.shares_of(&staker_b), 0);

        // Premium of 500 (20% = 100 cut) accrues 100% to Staker A's mature pool (1_100).
        assert_eq!(client.receive_premium(&500), 100);
        let s = client.state();
        assert_eq!(s.mature_capital, 1_100);
        assert_eq!(s.unmatured_principal, 1_000);
        assert!(s.mature_capital <= s.total_capital);

        // Drawdown of 1_500 exceeds mature_capital (1_100): clamps at 1_100 and records 400 shortfall.
        assert_eq!(client.cover_shortfall(&pool, &1_500), 1_100);
        let s_after = client.state();
        assert_eq!(s_after.mature_capital, 0);
        assert_eq!(s_after.unmatured_principal, 1_000);
        assert_eq!(s_after.unabsorbed_shortfall, 400);
    }

    #[test]
    fn pending_loss_freeze_blocks_sniping_deposits_until_cleared_or_settled() {
        let (_env, client, pool, admin, staker_a, _staker_b) = setup();

        client.set_pending_loss(&pool, &true);
        assert!(client.is_pending_loss());
        assert_eq!(
            client.try_deposit(&staker_a, &1_000),
            Err(Ok(BackstopError::PendingLossFreeze))
        );

        // Cancelling pending loss re-opens deposits.
        client.set_pending_loss(&admin, &false);
        assert!(!client.is_pending_loss());
        client.deposit(&staker_a, &1_000);
    }
}
