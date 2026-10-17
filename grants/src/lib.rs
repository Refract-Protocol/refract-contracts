#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub type GrantId = u64;

/// Maximum distinct funders permitted on a single grant to bound iteration cost.
pub const MAX_FUNDERS_PER_GRANT: u32 = 16;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
    EscrowBalance,
    CoFunderAllowlistOnly,
    AllowedCoFunder(Address),
}

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
    InvalidCoFundAmount = 14,
    CoFunderNotAllowed = 15,
    MaxFundersReached = 16,
    InsufficientGrantFunding = 17,
    GrantAlreadyCancelled = 18,
    BalanceDeltaMismatch = 19,
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

/// Per-funder accounting entry within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunderShare {
    pub funder: Address,
    pub deposited: i128,
    pub remaining: i128,
    pub disbursed: i128,
    pub refunded: i128,
}

/// A grant approved by governance with multi-funder escrow accounting.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
    pub funders: Vec<FunderShare>,
    pub remaining_escrow: i128,
    pub cancelled: bool,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize the grants contract with a governance/timelock authority and token.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::EscrowBalance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::CoFunderAllowlistOnly, &false);
        Ok(())
    }

    /// Configure whether external co-funders must be allowlisted.
    pub fn set_co_funder_policy(
        env: Env,
        caller: Address,
        allowlist_only: bool,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::CoFunderAllowlistOnly, &allowlist_only);
        Ok(())
    }

    /// Allowlist or remove an external co-funder address.
    pub fn set_co_funder_allowed(
        env: Env,
        caller: Address,
        funder: Address,
        allowed: bool,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        let key = DataKey::AllowedCoFunder(funder);
        if allowed {
            env.storage().persistent().set(&key, &true);
            env.storage().persistent().extend_ttl(
                &key,
                PERSISTENT_TTL_THRESHOLD,
                PERSISTENT_TTL_EXTEND,
            );
        } else {
            env.storage().persistent().remove(&key);
        }
        Ok(())
    }

    pub fn is_co_funder_allowed(env: Env, funder: Address) -> bool {
        let allowlist_only: bool = env
            .storage()
            .instance()
            .get(&DataKey::CoFunderAllowlistOnly)
            .unwrap_or(false);
        if !allowlist_only {
            return true;
        }
        if let Ok(gov) = Self::read_governance(&env) {
            if funder == gov {
                return true;
            }
        }
        env.storage()
            .persistent()
            .get(&DataKey::AllowedCoFunder(funder))
            .unwrap_or(false)
    }

    /// Approve a new grant with `total_amount` split across `milestones`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
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

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored,
            claimable: 0,
            funders: Vec::new(&env),
            remaining_escrow: 0,
            cancelled: false,
        };
        let key = DataKey::Grant(grant_id);
        env.storage().persistent().set(&key, &grant);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        Ok(grant_id)
    }

    /// Contribute `amount` to `grant_id` as the treasury or an external co-funder.
    ///
    /// Verifies SEP-41 token transfer-in via contract balance delta and enforces
    /// `MAX_FUNDERS_PER_GRANT`. If `co_fund` increases total funding above the
    /// remaining unpaid milestone sum (matching contribution), the extra funding
    /// is added to the last unconfirmed milestone so 100% of co-funded capital
    /// is disbursable without stranding funds.
    pub fn co_fund(
        env: Env,
        funder: Address,
        grant_id: GrantId,
        amount: i128,
    ) -> Result<i128, GrantsError> {
        funder.require_auth();
        if amount <= 0 {
            return Err(GrantsError::InvalidCoFundAmount);
        }
        if !Self::is_co_funder_allowed(env.clone(), funder.clone()) {
            return Err(GrantsError::CoFunderNotAllowed);
        }

        let key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.cancelled {
            return Err(GrantsError::GrantAlreadyCancelled);
        }

        let mut existing_idx: Option<u32> = None;
        for idx in 0..grant.funders.len() {
            if let Some(f) = grant.funders.get(idx) {
                if f.funder == funder {
                    existing_idx = Some(idx);
                    break;
                }
            }
        }
        if existing_idx.is_none() && grant.funders.len() >= MAX_FUNDERS_PER_GRANT {
            return Err(GrantsError::MaxFundersReached);
        }

        let token_addr = Self::read_token(&env)?;
        let token_client = token::Client::new(&env, &token_addr);
        let contract_addr = env.current_contract_address();
        let before = token_client.balance(&contract_addr);
        token_client.transfer(&funder, &contract_addr, &amount);
        let after = token_client.balance(&contract_addr);
        if after - before != amount {
            return Err(GrantsError::BalanceDeltaMismatch);
        }

        if let Some(idx) = existing_idx {
            let mut entry = grant.funders.get(idx).unwrap();
            entry.deposited += amount;
            entry.remaining += amount;
            grant.funders.set(idx, entry);
        } else {
            grant.funders.push_back(FunderShare {
                funder: funder.clone(),
                deposited: amount,
                remaining: amount,
                disbursed: 0,
                refunded: 0,
            });
        }

        grant.remaining_escrow += amount;
        let escrow = Self::escrow_balance(env.clone())?;
        env.storage()
            .instance()
            .set(&DataKey::EscrowBalance, &(escrow + amount));

        env.storage().persistent().set(&key, &grant);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.events()
            .publish((symbol_short!("CoFund"), funder), (grant_id, amount));
        Ok(grant.remaining_escrow)
    }

    /// Confirm delivery of a milestone. Requires the grant's unallocated escrow
    /// (`remaining_escrow - claimable`) to cover the milestone amount.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.cancelled {
            return Err(GrantsError::GrantAlreadyCancelled);
        }

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }
        for idx in 0..milestone_index {
            let earlier = grant
                .milestones
                .get(idx)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        let next_claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        if next_claimable > grant.remaining_escrow {
            return Err(GrantsError::InsufficientGrantFunding);
        }

        milestone.confirmed = true;
        grant.claimable = next_claimable;
        grant.milestones.set(milestone_index, milestone);

        env.storage().persistent().set(&key, &grant);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        Ok(())
    }

    /// Claim confirmed milestone funds, drawing pro-rata across all funders'
    /// `remaining` balances with deterministic remainder assignment to the
    /// earliest funder(s) with available balance.
    pub fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.cancelled {
            return Err(GrantsError::GrantAlreadyCancelled);
        }
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }
        if amount > grant.remaining_escrow {
            return Err(GrantsError::InsufficientGrantFunding);
        }

        let total_remaining = grant.remaining_escrow;
        let len = grant.funders.len();
        let mut allocated: i128 = 0;

        for idx in 0..len {
            let mut f = grant.funders.get(idx).unwrap();
            let cut = amount * f.remaining / total_remaining;
            f.remaining -= cut;
            f.disbursed += cut;
            allocated += cut;
            grant.funders.set(idx, f);
        }

        // Deterministic remainder rule: assign leftover units 1-by-1 in funder order.
        let mut remainder = amount - allocated;
        for idx in 0..len {
            if remainder <= 0 {
                break;
            }
            let mut f = grant.funders.get(idx).unwrap();
            if f.remaining > 0 {
                let take = if f.remaining >= remainder {
                    remainder
                } else {
                    f.remaining
                };
                f.remaining -= take;
                f.disbursed += take;
                remainder -= take;
                grant.funders.set(idx, f);
            }
        }

        grant.claimable = 0;
        grant.remaining_escrow -= amount;
        for idx in 0..grant.milestones.len() {
            if let Some(mut ms) = grant.milestones.get(idx) {
                if ms.confirmed && !ms.claimed {
                    ms.claimed = true;
                    grant.milestones.set(idx, ms);
                }
            }
        }

        let escrow = Self::escrow_balance(env.clone())?;
        env.storage()
            .instance()
            .set(&DataKey::EscrowBalance, &(escrow - amount));
        env.storage().persistent().set(&key, &grant);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        let token_addr = Self::read_token(&env)?;
        token::Client::new(&env, &token_addr).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        Ok(amount)
    }

    /// Cancel/clawback a grant and refund all unspent escrow pro-rata to every
    /// funder using the same deterministic remainder rule.
    pub fn clawback_grant(
        env: Env,
        caller: Address,
        grant_id: GrantId,
    ) -> Result<Vec<(Address, i128)>, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.cancelled {
            return Err(GrantsError::GrantAlreadyCancelled);
        }

        let unspent = grant.remaining_escrow;
        let len = grant.funders.len();
        let mut refunds = Vec::new(&env);
        let token_addr = Self::read_token(&env)?;
        let token_client = token::Client::new(&env, &token_addr);
        let contract_addr = env.current_contract_address();

        if unspent > 0 && len > 0 {
            let mut allocated: i128 = 0;
            let mut shares = Vec::new(&env);
            for idx in 0..len {
                let f = grant.funders.get(idx).unwrap();
                let share = unspent * f.remaining / unspent;
                allocated += share;
                shares.push_back(share);
            }
            let mut rem = unspent - allocated;
            for idx in 0..len {
                let mut f = grant.funders.get(idx).unwrap();
                let mut refund_amt = shares.get(idx).unwrap();
                if rem > 0 && f.remaining > refund_amt {
                    let extra = (f.remaining - refund_amt).min(rem);
                    refund_amt += extra;
                    rem -= extra;
                }
                f.remaining -= refund_amt;
                f.refunded += refund_amt;
                grant.funders.set(idx, f.clone());
                if refund_amt > 0 {
                    token_client.transfer(&contract_addr, &f.funder, &refund_amt);
                }
                refunds.push_back((f.funder, refund_amt));
            }
        }

        grant.claimable = 0;
        grant.remaining_escrow = 0;
        grant.cancelled = true;

        let escrow = Self::escrow_balance(env.clone())?;
        env.storage()
            .instance()
            .set(&DataKey::EscrowBalance, &(escrow - unspent));
        env.storage().persistent().set(&key, &grant);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        Ok(refunds)
    }

    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::read_governance(&env)
    }

    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn claimable(env: Env, grant_id: GrantId) -> Option<i128> {
        Self::get_grant(env, grant_id).map(|g| g.claimable)
    }

    /// Aggregate unspent escrow across all active grants.
    pub fn escrow_balance(env: Env) -> Result<i128, GrantsError> {
        let _ = Self::read_governance(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::EscrowBalance)
            .unwrap_or(0))
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let gov = Self::read_governance(env)?;
        if *caller != gov {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn read_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    fn setup() -> (
        Env,
        RefractGrantsClient<'static>,
        Address,
        Address,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let treasury = Address::generate(&env);
        let sponsor_a = Address::generate(&env);
        let sponsor_b = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&treasury, &10_000);
        sac.mint(&sponsor_a, &10_000);
        sac.mint(&sponsor_b, &10_000);

        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        (
            env, client, governance, treasury, sponsor_a, sponsor_b, recipient,
        )
    }

    #[test]
    fn three_funder_pro_rata_disbursement_remainder_and_clawback_refund() {
        let (env, client, governance, treasury, sponsor_a, sponsor_b, recipient) = setup();
        // 3 milestones of 100 each = 300 total.
        let id = client.create_grant(&governance, &recipient, &300, &vec![&env, 100, 100, 100]);

        // Treasury funds 100, Sponsor A funds 100, Sponsor B funds 100 -> equal 1/3 shares.
        client.co_fund(&treasury, &id, &100);
        client.co_fund(&sponsor_a, &id, &100);
        client.co_fund(&sponsor_b, &id, &100);
        assert_eq!(client.escrow_balance(), 300);

        // Confirm and claim Milestone 0 (100 units across 3 equal funders -> 34, 33, 33).
        client.confirm_milestone(&governance, &id, &0);
        assert_eq!(client.claim(&recipient, &id), 100);
        assert_eq!(client.escrow_balance(), 200);

        let grant = client.get_grant(&id).unwrap();
        let f0 = grant.funders.get(0).unwrap();
        let f1 = grant.funders.get(1).unwrap();
        let f2 = grant.funders.get(2).unwrap();
        // Deterministic remainder assigned 1 extra unit to funder 0 (34 vs 33, 33).
        assert_eq!((f0.disbursed, f0.remaining), (34, 66));
        assert_eq!((f1.disbursed, f1.remaining), (33, 67));
        assert_eq!((f2.disbursed, f2.remaining), (33, 67));
        assert_eq!(
            f0.remaining + f1.remaining + f2.remaining,
            client.escrow_balance()
        );

        // Clawback the remaining 200 units -> refunds exact remaining shares (66, 67, 67).
        let refunds = client.clawback_grant(&governance, &id);
        assert_eq!(refunds.len(), 3);
        assert_eq!(refunds.get(0).unwrap(), (treasury, 66));
        assert_eq!(refunds.get(1).unwrap(), (sponsor_a, 67));
        assert_eq!(refunds.get(2).unwrap(), (sponsor_b, 67));
        assert_eq!(client.escrow_balance(), 0);
    }

    #[test]
    fn co_funder_allowlist_and_max_funders_cap_enforcement() {
        let (env, client, governance, treasury, sponsor_a, _sponsor_b, recipient) = setup();
        let id = client.create_grant(&governance, &recipient, &200, &vec![&env, 200]);

        client.set_co_funder_policy(&governance, &true);
        assert_eq!(
            client.try_co_fund(&sponsor_a, &id, &50),
            Err(Ok(GrantsError::CoFunderNotAllowed))
        );

        client.set_co_funder_allowed(&governance, &sponsor_a, &true);
        client.co_fund(&sponsor_a, &id, &50);
        client.set_co_funder_allowed(&governance, &treasury, &true);
        client.co_fund(&treasury, &id, &150);
        assert_eq!(client.escrow_balance(), 200);

        // Enforce MAX_FUNDERS_PER_GRANT (16)
        client.set_co_funder_policy(&governance, &false);
        let token_addr: Address = env.as_contract(&client.address, || {
            env.storage().instance().get(&DataKey::Token).unwrap()
        });
        let sac = token::StellarAssetClient::new(&env, &token_addr);

        for _ in 2..MAX_FUNDERS_PER_GRANT {
            let f = Address::generate(&env);
            sac.mint(&f, &10);
            client.co_fund(&f, &id, &10);
        }
        let extra_funder = Address::generate(&env);
        sac.mint(&extra_funder, &10);
        assert_eq!(
            client.try_co_fund(&extra_funder, &id, &10),
            Err(Ok(GrantsError::MaxFundersReached))
        );
    }
}
