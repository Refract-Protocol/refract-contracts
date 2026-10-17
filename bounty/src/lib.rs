//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with per-recipient payout velocity limits,
//! minimum spacing cooldowns, and rolling-window outflow caps (`#265`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VelocityConfig {
    pub min_spacing_ledgers: u32,
    pub window_ledgers: u32,
    pub recipient_window_cap: i128,
    pub global_epoch_cap: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VelocityWindow {
    pub has_paid: bool,
    pub last_payout_ledger: u32,
    pub window_start: u32,
    pub window_outflow: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VelocityPreview {
    pub accepted: bool,
    pub next_eligible_ledger: u32,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    VelocityConfig,
    RecipientWindow(Address, Address),
    GlobalEpochWindow(Address),
    Award(BytesN<32>),
}

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
    CooldownNotElapsed = 7,
    RecipientVelocityCapExceeded = 8,
    GlobalEpochCapExceeded = 9,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        let default_velocity = VelocityConfig {
            min_spacing_ledgers: 100,
            window_ledgers: 1_000,
            recipient_window_cap: 10_000_0000000,
            global_epoch_cap: 50_000_0000000,
        };
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::VelocityConfig, &default_velocity);
        Ok(())
    }

    pub fn set_velocity_config(
        env: Env,
        caller: Address,
        config: VelocityConfig,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if config.window_ledgers == 0
            || config.recipient_window_cap <= 0
            || config.global_epoch_cap <= 0
        {
            return Err(BountyError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::VelocityConfig, &config);
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

    /// Read-only preview reporting whether `amount` to `recipient` would be accepted now and when
    /// it would next be eligible under the configured velocity limits (`#265`).
    pub fn preview_payout(
        env: Env,
        recipient: Address,
        amount: i128,
    ) -> Result<VelocityPreview, BountyError> {
        let token = Self::token(&env)?;
        let cfg = Self::velocity_config_internal(&env)?;
        let now = env.ledger().sequence();

        let rec_win = Self::recipient_window(&env, &recipient, &token, now);
        let glob_win = Self::global_window(&env, &token, now);

        let mut next_ledger = now;
        if rec_win.has_paid {
            let cooldown_end = rec_win
                .last_payout_ledger
                .saturating_add(cfg.min_spacing_ledgers);
            if now < cooldown_end {
                next_ledger = next_ledger.max(cooldown_end);
            }
        }

        let active_rec_outflow = if now >= rec_win.window_start.saturating_add(cfg.window_ledgers) {
            0
        } else {
            rec_win.window_outflow
        };
        if active_rec_outflow + amount > cfg.recipient_window_cap {
            let window_reset = rec_win.window_start.saturating_add(cfg.window_ledgers);
            next_ledger = next_ledger.max(window_reset);
        }

        let active_glob_outflow = if now >= glob_win.window_start.saturating_add(cfg.window_ledgers)
        {
            0
        } else {
            glob_win.window_outflow
        };
        if active_glob_outflow + amount > cfg.global_epoch_cap {
            let glob_reset = glob_win.window_start.saturating_add(cfg.window_ledgers);
            next_ledger = next_ledger.max(glob_reset);
        }

        let accepted = next_ledger == now
            && amount <= cfg.recipient_window_cap
            && amount <= cfg.global_epoch_cap;
        Ok(VelocityPreview {
            accepted,
            next_eligible_ledger: next_ledger,
        })
    }

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
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
        let balance = Self::balance_internal(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        Self::enforce_and_record_velocity(&env, &researcher, &token, amount)?;

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));
        env.storage().persistent().set(
            &DataKey::Award(report_hash.clone()),
            &AwardRecord {
                researcher,
                amount,
                report_hash,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(())
    }

    fn enforce_and_record_velocity(
        env: &Env,
        recipient: &Address,
        token: &Address,
        amount: i128,
    ) -> Result<(), BountyError> {
        let cfg = Self::velocity_config_internal(env)?;
        let now = env.ledger().sequence();

        let mut rec_win = Self::recipient_window(env, recipient, token, now);
        // (a) Minimum spacing between payouts to the same recipient
        if rec_win.has_paid
            && now
                < rec_win
                    .last_payout_ledger
                    .saturating_add(cfg.min_spacing_ledgers)
        {
            env.events().publish(
                (symbol_short!("vel_hit"), recipient.clone(), token.clone()),
                (amount, symbol_short!("cooldown")),
            );
            return Err(BountyError::CooldownNotElapsed);
        }

        // Roll over recipient window if expired
        if now >= rec_win.window_start.saturating_add(cfg.window_ledgers) {
            rec_win.window_start = now;
            rec_win.window_outflow = 0;
        }
        // (b) Per-recipient rolling-window outflow cap
        if rec_win.window_outflow + amount > cfg.recipient_window_cap {
            env.events().publish(
                (symbol_short!("vel_hit"), recipient.clone(), token.clone()),
                (amount, symbol_short!("rec_cap")),
            );
            return Err(BountyError::RecipientVelocityCapExceeded);
        }

        // (c) Global per-epoch outflow cap
        let mut glob_win = Self::global_window(env, token, now);
        if now >= glob_win.window_start.saturating_add(cfg.window_ledgers) {
            glob_win.window_start = now;
            glob_win.window_outflow = 0;
        }
        if glob_win.window_outflow + amount > cfg.global_epoch_cap {
            env.events().publish(
                (symbol_short!("vel_hit"), recipient.clone(), token.clone()),
                (amount, symbol_short!("glob_cap")),
            );
            return Err(BountyError::GlobalEpochCapExceeded);
        }

        rec_win.has_paid = true;
        rec_win.last_payout_ledger = now;
        rec_win.window_outflow += amount;
        glob_win.window_outflow += amount;

        env.storage().persistent().set(
            &DataKey::RecipientWindow(recipient.clone(), token.clone()),
            &rec_win,
        );
        env.storage()
            .persistent()
            .set(&DataKey::GlobalEpochWindow(token.clone()), &glob_win);
        Ok(())
    }

    fn recipient_window(
        env: &Env,
        recipient: &Address,
        token: &Address,
        now: u32,
    ) -> VelocityWindow {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientWindow(recipient.clone(), token.clone()))
            .unwrap_or(VelocityWindow {
                has_paid: false,
                last_payout_ledger: 0,
                window_start: now,
                window_outflow: 0,
            })
    }

    fn global_window(env: &Env, token: &Address, now: u32) -> VelocityWindow {
        env.storage()
            .persistent()
            .get(&DataKey::GlobalEpochWindow(token.clone()))
            .unwrap_or(VelocityWindow {
                has_paid: false,
                last_payout_ledger: 0,
                window_start: now,
                window_outflow: 0,
            })
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

    fn velocity_config_internal(env: &Env) -> Result<VelocityConfig, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::VelocityConfig)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn bounty_velocity_enforces_spacing_recipient_cap_global_cap_and_window_rollover() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.sequence_number = 100);

        let gov = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&gov, &5_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.fund(&gov, &5_000);

        // min_spacing = 50 ledgers, window = 200 ledgers, recipient_cap = 600, global_cap = 1,000
        client.set_velocity_config(
            &gov,
            &VelocityConfig {
                min_spacing_ledgers: 50,
                window_ledgers: 200,
                recipient_window_cap: 600,
                global_epoch_cap: 1_000,
            },
        );

        // First payout of 400 to r1 at ledger 100 succeeds
        client.award(&gov, &r1, &400, &BytesN::from_array(&env, &[1u8; 32]));

        // Immediate second payout to r1 at ledger 120 fails cooldown (needs ledger 150)
        env.ledger().with_mut(|li| li.sequence_number = 120);
        let preview = client.preview_payout(&r1, &200);
        assert!(!preview.accepted);
        assert_eq!(preview.next_eligible_ledger, 150);
        assert_eq!(
            client.try_award(&gov, &r1, &200, &BytesN::from_array(&env, &[2u8; 32])),
            Err(Ok(BountyError::CooldownNotElapsed))
        );

        // Advance to ledger 150 -> 201 to r1 exceeds recipient_window_cap (400 + 201 > 600)
        env.ledger().with_mut(|li| li.sequence_number = 150);
        assert_eq!(
            client.try_award(&gov, &r1, &201, &BytesN::from_array(&env, &[2u8; 32])),
            Err(Ok(BountyError::RecipientVelocityCapExceeded))
        );

        // Exact remaining recipient cap (200) succeeds -> r1 total = 600, global = 600
        client.award(&gov, &r1, &200, &BytesN::from_array(&env, &[2u8; 32]));

        // r2 attempts 401 in the same epoch -> exceeds global_epoch_cap (600 + 401 > 1000)
        assert_eq!(
            client.try_award(&gov, &r2, &401, &BytesN::from_array(&env, &[3u8; 32])),
            Err(Ok(BountyError::GlobalEpochCapExceeded))
        );

        // Advance past window boundary (100 + 200 = 300) -> window resets!
        env.ledger().with_mut(|li| li.sequence_number = 300);
        assert!(client.preview_payout(&r1, &500).accepted);
        client.award(&gov, &r1, &500, &BytesN::from_array(&env, &[4u8; 32]));
    }
}
