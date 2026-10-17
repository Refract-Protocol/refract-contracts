//! # RefractTreasury
//!
//! Protocol fee and reserve vault with a governance-managed SEP-41 asset
//! allowlist and per-asset risk parameters (`deposit_cap`, `valuation_feed_id`,
//! `haircut_bps`, `enabled`).

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

const BPS_DENOM: i128 = 10_000;
const MAX_PAGE_SIZE: u32 = 25;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    InvalidCap = 5,
    CapExceeded = 6,
    InvalidAssetParams = 7,
    AssetNotAllowed = 8,
    AssetCapExceeded = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    AssetParams(Address),
    AssetBalance(Address),
    AssetList,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub token: Address,
    pub max_single_withdrawal: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetRiskParams {
    pub asset: Address,
    pub enabled: bool,
    pub deposit_cap: i128,
    pub valuation_feed_id: u64,
    pub haircut_bps: u32,
    pub last_updated_ledger: u32,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
        max_single_withdrawal: i128,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        governance.require_auth();
        if max_single_withdrawal <= 0 {
            return Err(TreasuryError::InvalidCap);
        }
        let cfg = Config {
            governance,
            token: token.clone(),
            max_single_withdrawal,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);

        // Register the primary token in the allowlist by default
        let default_params = AssetRiskParams {
            asset: token.clone(),
            enabled: true,
            deposit_cap: i128::MAX / 2,
            valuation_feed_id: 1,
            haircut_bps: 0,
            last_updated_ledger: env.ledger().sequence(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::AssetParams(token.clone()), &default_params);
        let mut list = Vec::new(&env);
        list.push_back(token);
        env.storage().instance().set(&DataKey::AssetList, &list);
        Ok(())
    }

    /// Registers or updates an asset in the allowlist with validated risk parameters.
    pub fn configure_asset(
        env: Env,
        asset: Address,
        enabled: bool,
        deposit_cap: i128,
        valuation_feed_id: u64,
        haircut_bps: u32,
    ) -> Result<AssetRiskParams, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();

        if deposit_cap <= 0 || valuation_feed_id == 0 || (haircut_bps as i128) > BPS_DENOM {
            return Err(TreasuryError::InvalidAssetParams);
        }

        let params = AssetRiskParams {
            asset: asset.clone(),
            enabled,
            deposit_cap,
            valuation_feed_id,
            haircut_bps,
            last_updated_ledger: env.ledger().sequence(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::AssetParams(asset.clone()), &params);

        let mut list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AssetList)
            .unwrap_or_else(|| Vec::new(&env));
        if !list.contains(&asset) {
            list.push_back(asset.clone());
            env.storage().instance().set(&DataKey::AssetList, &list);
        }

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("ast_cfg")),
            (asset, enabled, deposit_cap, valuation_feed_id, haircut_bps),
        );
        Ok(params)
    }

    /// Enables or disables inflows for a registered asset without altering its risk parameters.
    pub fn set_asset_enabled(
        env: Env,
        asset: Address,
        enabled: bool,
    ) -> Result<AssetRiskParams, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut params: AssetRiskParams = env
            .storage()
            .persistent()
            .get(&DataKey::AssetParams(asset.clone()))
            .ok_or(TreasuryError::AssetNotAllowed)?;
        params.enabled = enabled;
        params.last_updated_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::AssetParams(asset.clone()), &params);
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("ast_en")),
            (asset, enabled),
        );
        Ok(params)
    }

    /// Deposits an allowlisted, enabled asset within its `deposit_cap`.
    pub fn deposit_asset(
        env: Env,
        from: Address,
        asset: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        let _cfg = Self::read_config(&env)?;
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let params: AssetRiskParams = env
            .storage()
            .persistent()
            .get(&DataKey::AssetParams(asset.clone()))
            .ok_or(TreasuryError::AssetNotAllowed)?;
        if !params.enabled {
            return Err(TreasuryError::AssetNotAllowed);
        }

        let current_bal: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::AssetBalance(asset.clone()))
            .unwrap_or(0);
        let next_bal = current_bal
            .checked_add(amount)
            .ok_or(TreasuryError::AssetCapExceeded)?;
        if next_bal > params.deposit_cap {
            return Err(TreasuryError::AssetCapExceeded);
        }

        token::Client::new(&env, &asset).transfer(&from, &env.current_contract_address(), &amount);
        env.storage()
            .persistent()
            .set(&DataKey::AssetBalance(asset.clone()), &next_bal);

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("deposit")),
            (from, asset, amount, next_bal),
        );
        Ok(next_bal)
    }

    /// Withdraws or refunds an existing balance of `asset`.
    /// Disabling an asset blocks new inflows but NEVER blocks exits/withdrawals of held balances.
    pub fn withdraw_asset(
        env: Env,
        recipient: Address,
        asset: Address,
        amount: i128,
        reason: Symbol,
    ) -> Result<i128, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if amount > cfg.max_single_withdrawal {
            return Err(TreasuryError::CapExceeded);
        }
        if !env
            .storage()
            .persistent()
            .has(&DataKey::AssetParams(asset.clone()))
        {
            return Err(TreasuryError::AssetNotAllowed);
        }

        let current_bal: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::AssetBalance(asset.clone()))
            .unwrap_or(0);
        if amount > current_bal {
            return Err(TreasuryError::InsufficientBalance);
        }

        let next_bal = current_bal - amount;
        env.storage()
            .persistent()
            .set(&DataKey::AssetBalance(asset.clone()), &next_bal);

        token::Client::new(&env, &asset).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("withdraw")),
            (recipient, asset, amount, reason),
        );
        Ok(next_bal)
    }

    pub fn asset_params(env: Env, asset: Address) -> Option<AssetRiskParams> {
        env.storage().persistent().get(&DataKey::AssetParams(asset))
    }

    pub fn asset_balance(env: Env, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AssetBalance(asset))
            .unwrap_or(0)
    }

    /// Returns the haircut-adjusted balance of `asset` (`balance * (10_000 - haircut_bps) / 10_000`).
    pub fn asset_risk_adjusted_balance(env: Env, asset: Address) -> i128 {
        let Some(params) = Self::asset_params(env.clone(), asset.clone()) else {
            return 0;
        };
        let bal = Self::asset_balance(env, asset);
        bal * (BPS_DENOM - params.haircut_bps as i128) / BPS_DENOM
    }

    /// Cursor-paginated view of enabled assets.
    pub fn list_enabled_assets(
        env: Env,
        cursor: u32,
        limit: u32,
    ) -> (Vec<AssetRiskParams>, Option<u32>) {
        let list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AssetList)
            .unwrap_or_else(|| Vec::new(&env));
        let capped = limit.clamp(1, MAX_PAGE_SIZE);
        let mut page = Vec::new(&env);
        let mut idx = cursor;
        while idx < list.len() && page.len() < capped {
            if let Some(addr) = list.get(idx) {
                if let Some(p) = env
                    .storage()
                    .persistent()
                    .get::<DataKey, AssetRiskParams>(&DataKey::AssetParams(addr))
                {
                    if p.enabled {
                        page.push_back(p);
                    }
                }
            }
            idx += 1;
        }
        let next = if idx < list.len() { Some(idx) } else { None };
        (page, next)
    }

    fn read_config(env: &Env) -> Result<Config, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(TreasuryError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn asset_allowlist_enforces_caps_rejects_unapproved_and_allows_exit_when_disabled() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let user = Address::generate(&env);
        let recipient = Address::generate(&env);

        let t1_admin = Address::generate(&env);
        let usdc = env.register_stellar_asset_contract_v2(t1_admin).address();
        token::StellarAssetClient::new(&env, &usdc).mint(&user, &10_000);

        let t2_admin = Address::generate(&env);
        let rogue_token = env.register_stellar_asset_contract_v2(t2_admin).address();
        token::StellarAssetClient::new(&env, &rogue_token).mint(&user, &10_000);

        let cid = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &cid);

        client.initialize(&gov, &usdc, &5_000);

        // Configure USDC with deposit_cap = 3_000, feed = 10, haircut = 500 bps (5%)
        client.configure_asset(&usdc, &true, &3_000, &10, &500);

        // Invalid parameters (feed = 0 or haircut > 10_000) are rejected
        assert_eq!(
            client.try_configure_asset(&usdc, &true, &3_000, &0, &500),
            Err(Ok(TreasuryError::InvalidAssetParams))
        );

        // Unapproved rogue_token is rejected at inflow
        assert_eq!(
            client.try_deposit_asset(&user, &rogue_token, &500),
            Err(Ok(TreasuryError::AssetNotAllowed))
        );

        // Deposit 2_000 USDC succeeds; depositing another 1_500 exceeds 3_000 cap
        assert_eq!(client.deposit_asset(&user, &usdc, &2_000), 2_000);
        assert_eq!(client.asset_risk_adjusted_balance(&usdc), 1_900);
        assert_eq!(
            client.try_deposit_asset(&user, &usdc, &1_500),
            Err(Ok(TreasuryError::AssetCapExceeded))
        );

        // Disabling USDC blocks new inflows but still allows withdrawals of existing balance
        client.set_asset_enabled(&usdc, &false);
        assert_eq!(
            client.try_deposit_asset(&user, &usdc, &500),
            Err(Ok(TreasuryError::AssetNotAllowed))
        );
        assert_eq!(
            client.withdraw_asset(&recipient, &usdc, &1_200, &symbol_short!("exit")),
            800
        );
    }
}
