//! # RefractGrants (#209)
//!
//! Milestone-based community grants escrow, linear/cliff vesting, and governance clawback contract.

#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{contract, contracterror, contractimpl, symbol_short, token, Address, Env, Vec};
use storage::{DataKey, Grant, Milestone, MilestoneStatus, MAX_MILESTONES};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    GrantNotFound = 4,
    NoMilestones = 5,
    TooManyMilestones = 6,
    MilestoneSumMismatch = 7,
    InvalidMilestoneAmount = 8,
    InvalidTotalAmount = 9,
    InvalidMilestoneIndex = 10,
    MilestoneOutOfOrder = 11,
    MilestoneInvalidState = 12,
    VestingCliffNotReached = 13,
    NothingToClawback = 14,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize `RefractGrants` with the `governance` (timelock) authority, `treasury` address,
    /// and escrow `token` contract.
    pub fn initialize(
        env: Env,
        governance: Address,
        treasury: Address,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    /// Return the configured governance/timelock address.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::read_governance(&env)
    }

    /// Return the configured treasury address.
    pub fn treasury(env: Env) -> Result<Address, GrantsError> {
        Self::read_treasury(&env)
    }

    /// Create a new grant funded from `treasury` with `total_amount` split across `milestone_amounts`
    /// and optional per-milestone `cliff_offsets_secs` relative to the creation ledger timestamp.
    pub fn create_grant(
        env: Env,
        caller: Address,
        grantee: Address,
        total_amount: i128,
        milestone_amounts: Vec<i128>,
        cliff_offsets_secs: Option<Vec<u64>>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let count = milestone_amounts.len();
        if count == 0 {
            return Err(GrantsError::NoMilestones);
        }
        if count > MAX_MILESTONES {
            return Err(GrantsError::TooManyMilestones);
        }
        if let Some(ref cliffs) = cliff_offsets_secs {
            if cliffs.len() != count {
                return Err(GrantsError::MilestoneSumMismatch);
            }
        }

        let now = env.ledger().timestamp();
        let mut sum: i128 = 0;
        let mut stored_milestones = Vec::new(&env);

        for idx in 0..count {
            let amt = milestone_amounts.get(idx).unwrap_or(0);
            if amt <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amt)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            let cliff_offset = cliff_offsets_secs
                .as_ref()
                .and_then(|c| c.get(idx))
                .unwrap_or(0);
            stored_milestones.push_back(Milestone {
                amount: amt,
                vesting_cliff: now.saturating_add(cliff_offset),
                status: MilestoneStatus::Pending,
            });
        }

        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let treasury = Self::read_treasury(&env)?;
        treasury.require_auth();
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &treasury,
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

        let grant = Grant {
            id: grant_id,
            grantee: grantee.clone(),
            total: total_amount,
            released: 0,
            clawed_back: 0,
            milestones: stored_milestones,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        env.events().publish(
            (symbol_short!("grt_crt"), grant_id),
            (grantee, total_amount, count),
        );
        Ok(grant_id)
    }

    /// Governance/approver-gated confirmation of milestone `index`, transitioning `Pending -> Approved`.
    /// Milestones must be approved strictly in order (`0..index` must already be `Approved` or `Released`).
    pub fn approve_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        for earlier_idx in 0..index {
            let earlier = grant.milestones.get(earlier_idx).unwrap();
            if earlier.status == MilestoneStatus::Pending
                || earlier.status == MilestoneStatus::Rejected
            {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut m = grant.milestones.get(index).unwrap();
        if m.status != MilestoneStatus::Pending {
            return Err(GrantsError::MilestoneInvalidState);
        }

        m.status = MilestoneStatus::Approved;
        grant.milestones.set(index, m.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        env.events()
            .publish((symbol_short!("grt_app"), grant_id, index), m.amount);
        Ok(())
    }

    /// Release an `Approved` milestone (`index`) to `grant.grantee` once `now >= vesting_cliff`.
    /// Transitions `Approved -> Released` and transfers the tranche from escrow to `grantee`.
    pub fn release_milestone(env: Env, grant_id: u64, index: u32) -> Result<i128, GrantsError> {
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order release: every earlier milestone must already be Released.
        for earlier_idx in 0..index {
            let earlier = grant.milestones.get(earlier_idx).unwrap();
            if earlier.status != MilestoneStatus::Released {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut m = grant.milestones.get(index).unwrap();
        if m.status != MilestoneStatus::Approved {
            return Err(GrantsError::MilestoneInvalidState);
        }
        if env.ledger().timestamp() < m.vesting_cliff {
            return Err(GrantsError::VestingCliffNotReached);
        }

        m.status = MilestoneStatus::Released;
        grant.milestones.set(index, m.clone());
        grant.released = grant
            .released
            .checked_add(m.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &grant.grantee,
            &m.amount,
        );

        env.events().publish(
            (symbol_short!("grt_rel"), grant_id, index),
            (grant.grantee, m.amount),
        );
        Ok(m.amount)
    }

    /// Governance/timelock-gated clawback returning all unreleased funds (`Pending` or `Approved`)
    /// for `grant_id` back to the `treasury`, marking those milestones `Rejected`.
    /// Guarantees `grant.released + grant.clawed_back == grant.total`.
    pub fn clawback(env: Env, caller: Address, grant_id: u64) -> Result<i128, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        let mut clawback_amount: i128 = 0;
        let len = grant.milestones.len();
        for idx in 0..len {
            let mut m = grant.milestones.get(idx).unwrap();
            if m.status == MilestoneStatus::Pending || m.status == MilestoneStatus::Approved {
                clawback_amount = clawback_amount
                    .checked_add(m.amount)
                    .ok_or(GrantsError::MilestoneSumMismatch)?;
                m.status = MilestoneStatus::Rejected;
                grant.milestones.set(idx, m);
            }
        }

        if clawback_amount <= 0 {
            return Err(GrantsError::NothingToClawback);
        }

        grant.clawed_back = grant
            .clawed_back
            .checked_add(clawback_amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        let treasury = Self::read_treasury(&env)?;
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &treasury,
            &clawback_amount,
        );

        env.events().publish(
            (symbol_short!("grt_clw"), grant_id),
            (treasury, clawback_amount),
        );
        Ok(clawback_amount)
    }

    /// Return the stored `Grant` record for `grant_id`, if any.
    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    fn read_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_treasury(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let gov = Self::read_governance(env)?;
        if caller != &gov {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        vec,
    };

    fn setup() -> (Env, RefractGrantsClient<'static>, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
            li.min_temp_entry_ttl = 100_000;
            li.min_persistent_entry_ttl = 100_000;
            li.max_entry_ttl = 1_000_000;
        });
        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        let governance = Address::generate(&env);
        let treasury = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&treasury, &10_000_0000000);
        client.initialize(&governance, &treasury, &token_id);
        (env, client, governance, treasury, token_id)
    }

    #[test]
    fn happy_path_multi_milestone_out_of_order_and_double_release_rejection() {
        let (env, client, governance, _treasury, token_id) = setup();
        let grantee = Address::generate(&env);

        let id = client.create_grant(
            &governance,
            &grantee,
            &600_0000000,
            &vec![&env, 100_0000000, 200_0000000, 300_0000000],
            &None,
        );

        // Out-of-order approval (index 1 before index 0) is rejected.
        assert_eq!(
            client.try_approve_milestone(&governance, &id, &1),
            Err(Ok(GrantsError::MilestoneOutOfOrder))
        );

        client.approve_milestone(&governance, &id, &0);
        client.approve_milestone(&governance, &id, &1);

        // Out-of-order release (releasing index 1 before index 0 is released) is rejected.
        assert_eq!(
            client.try_release_milestone(&id, &1),
            Err(Ok(GrantsError::MilestoneOutOfOrder))
        );

        assert_eq!(client.release_milestone(&id, &0), 100_0000000);
        // Double release of index 0 is rejected.
        assert_eq!(
            client.try_release_milestone(&id, &0),
            Err(Ok(GrantsError::MilestoneInvalidState))
        );

        assert_eq!(client.release_milestone(&id, &1), 200_0000000);
        client.approve_milestone(&governance, &id, &2);
        assert_eq!(client.release_milestone(&id, &2), 300_0000000);

        let grant = client.get_grant(&id).unwrap();
        assert_eq!(grant.released, 600_0000000);
        assert_eq!(grant.clawed_back, 0);
        assert_eq!(
            token::Client::new(&env, &token_id).balance(&grantee),
            600_0000000
        );
    }

    #[test]
    fn vesting_cliff_boundary_and_partial_release_clawback_conservation() {
        let (env, client, governance, treasury, token_id) = setup();
        let grantee = Address::generate(&env);
        let token_client = token::Client::new(&env, &token_id);
        let treasury_pre = token_client.balance(&treasury);

        let id = client.create_grant(
            &governance,
            &grantee,
            &1_000_0000000,
            &vec![&env, 250_0000000, 350_0000000, 400_0000000],
            &Some(vec![&env, 1_000u64, 2_000u64, 3_000u64]),
        );
        assert_eq!(
            token_client.balance(&treasury),
            treasury_pre - 1_000_0000000
        );

        client.approve_milestone(&governance, &id, &0);
        client.approve_milestone(&governance, &id, &1);

        // 1 second before milestone 0's cliff (1_700_000_999 < 1_700_001_000) -> VestingCliffNotReached.
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_999;
        });
        assert_eq!(
            client.try_release_milestone(&id, &0),
            Err(Ok(GrantsError::VestingCliffNotReached))
        );

        // At cliff -> milestone 0 releases 250_0000000.
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_001_000;
        });
        assert_eq!(client.release_milestone(&id, &0), 250_0000000);

        // Clawback after partial release: milestone 1 (Approved, 350) + milestone 2 (Pending, 400) = 750 returned.
        let clawed = client.clawback(&governance, &id);
        assert_eq!(clawed, 750_0000000);

        let grant = client.get_grant(&id).unwrap();
        // Exact conservation: released + clawed_back == total.
        assert_eq!(grant.released + grant.clawed_back, grant.total);
        assert_eq!(token_client.balance(&grantee), 250_0000000);
        assert_eq!(
            token_client.balance(&treasury),
            treasury_pre - 1_000_0000000 + 750_0000000
        );

        // Second clawback fails with NothingToClawback.
        assert_eq!(
            client.try_clawback(&governance, &id),
            Err(Ok(GrantsError::NothingToClawback))
        );
    }
}
