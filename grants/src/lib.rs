//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with per-recipient payout
//! velocity limits, minimum spacing cooldowns, and rolling-window outflow caps (`#265`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    GrantNotFound = 4,
    NoMilestones = 5,
    MilestoneSumMismatch = 6,
    InvalidMilestoneAmount = 7,
    InvalidTotalAmount = 8,
    InvalidMilestoneIndex = 9,
    MilestoneAlreadyConfirmed = 10,
    MilestoneOutOfOrder = 11,
    NotRecipient = 12,
    NothingToClaim = 13,
    CooldownNotElapsed = 14,
    RecipientVelocityCapExceeded = 15,
    GlobalEpochCapExceeded = 16,
}

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    VelocityConfig,
    RecipientWindow(Address, Address),
    GlobalEpochWindow(Address),
    NextGrantId,
    Grant(u64),
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
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
        env.storage()
            .instance()
            .set(&DataKey::VelocityConfig, &default_velocity);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    pub fn set_velocity_config(
        env: Env,
        caller: Address,
        config: VelocityConfig,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if config.window_ledgers == 0
            || config.recipient_window_cap <= 0
            || config.global_epoch_cap <= 0
        {
            return Err(GrantsError::InvalidTotalAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::VelocityConfig, &config);
        Ok(())
    }

    pub fn preview_payout(
        env: Env,
        recipient: Address,
        amount: i128,
    ) -> Result<VelocityPreview, GrantsError> {
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
        let active_rec = if now >= rec_win.window_start.saturating_add(cfg.window_ledgers) {
            0
        } else {
            rec_win.window_outflow
        };
        if active_rec + amount > cfg.recipient_window_cap {
            next_ledger = next_ledger.max(rec_win.window_start.saturating_add(cfg.window_ledgers));
        }

        let active_glob = if now >= glob_win.window_start.saturating_add(cfg.window_ledgers) {
            0
        } else {
            glob_win.window_outflow
        };
        if active_glob + amount > cfg.global_epoch_cap {
            next_ledger = next_ledger.max(glob_win.window_start.saturating_add(cfg.window_ledgers));
        }

        Ok(VelocityPreview {
            accepted: next_ledger == now
                && amount <= cfg.recipient_window_cap
                && amount <= cfg.global_epoch_cap,
            next_eligible_ledger: next_ledger,
        })
    }

    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        let mut stored = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            stored.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &caller,
            &env.current_contract_address(),
            &total_amount,
        );

        let grant_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));
        env.storage().persistent().set(
            &DataKey::Grant(grant_id),
            &Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
            },
        );
        Ok(grant_id)
    }

    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }
        for i in 0..milestone_index {
            if !grant.milestones.get(i).unwrap().confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }
        let mut ms = grant.milestones.get(milestone_index).unwrap();
        if ms.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }
        ms.confirmed = true;
        grant.milestones.set(milestone_index, ms.clone());
        grant.claimable += ms.amount;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        let token = Self::token(&env)?;
        Self::enforce_and_record_velocity(&env, &recipient, &token, amount)?;

        grant.claimable = 0;
        for i in 0..grant.milestones.len() {
            let mut ms = grant.milestones.get(i).unwrap();
            if ms.confirmed {
                ms.claimed = true;
                grant.milestones.set(i, ms);
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );
        Ok(amount)
    }

    fn enforce_and_record_velocity(
        env: &Env,
        recipient: &Address,
        token: &Address,
        amount: i128,
    ) -> Result<(), GrantsError> {
        let cfg = Self::velocity_config_internal(env)?;
        let now = env.ledger().sequence();

        let mut rec_win = Self::recipient_window(env, recipient, token, now);
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
            return Err(GrantsError::CooldownNotElapsed);
        }

        if now >= rec_win.window_start.saturating_add(cfg.window_ledgers) {
            rec_win.window_start = now;
            rec_win.window_outflow = 0;
        }
        if rec_win.window_outflow + amount > cfg.recipient_window_cap {
            env.events().publish(
                (symbol_short!("vel_hit"), recipient.clone(), token.clone()),
                (amount, symbol_short!("rec_cap")),
            );
            return Err(GrantsError::RecipientVelocityCapExceeded);
        }

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
            return Err(GrantsError::GlobalEpochCapExceeded);
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

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let gov: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)?;
        if caller != &gov {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    fn velocity_config_internal(env: &Env) -> Result<VelocityConfig, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::VelocityConfig)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn grants_velocity_cooldown_and_cap_enforcement() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.sequence_number = 500);

        let gov = Address::generate(&env);
        let grantee = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&gov, &1_000);

        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.set_velocity_config(
            &gov,
            &VelocityConfig {
                min_spacing_ledgers: 40,
                window_ledgers: 200,
                recipient_window_cap: 500,
                global_epoch_cap: 800,
            },
        );

        let gid = client.create_grant(&gov, &grantee, &700, &vec![&env, 300, 400]);
        client.confirm_milestone(&gov, &gid, &0);
        assert_eq!(client.claim(&grantee, &gid), 300);

        // Confirm milestone 1 (400): blocked by cooldown before ledger 540, and then by recipient cap (300+400 > 500) before ledger 700
        client.confirm_milestone(&gov, &gid, &1);
        assert_eq!(
            client.try_claim(&grantee, &gid),
            Err(Ok(GrantsError::CooldownNotElapsed))
        );
        env.ledger().with_mut(|li| li.sequence_number = 550);
        assert_eq!(
            client.try_claim(&grantee, &gid),
            Err(Ok(GrantsError::RecipientVelocityCapExceeded))
        );

        env.ledger().with_mut(|li| li.sequence_number = 700);
        assert_eq!(client.claim(&grantee, &gid), 400);
    }
}
