//! # RefractGrants
//!
//! Milestone-based ecosystem grant disbursement contract with category-level
//! line-item budget plans, drawdown enforcement, reviewer-approved category
//! reallocations, and variance reporting.

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

const MAX_CATEGORIES: u32 = 16;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidConfig = 3,
    InvalidGrant = 4,
    GrantNotFound = 5,
    MilestoneNotFound = 6,
    MilestoneAlreadyReleased = 7,
    GrantNotActive = 8,
    DuplicateApproval = 9,
    LineItemMismatch = 10,
    CategoryNotFound = 11,
    CategoryOverspend = 12,
    DuplicateCategory = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    NextGrantId,
    Grant(u64),
    LineItems(u64),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub reviewers: Vec<Address>,
    pub approval_threshold: u32,
    pub token: Address,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GrantStatus {
    Active = 0,
    Completed = 1,
    Cancelled = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub description: Symbol,
    pub category: Symbol,
    pub amount: i128,
    pub approvals: Vec<Address>,
    pub released: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub id: u64,
    pub recipient: Address,
    pub total_amount: i128,
    pub released_amount: i128,
    pub milestones: Vec<Milestone>,
    pub status: GrantStatus,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryLineItem {
    pub category: Symbol,
    pub initial_allocated: i128,
    pub current_allocated: i128,
    pub disbursed: i128,
    pub remaining: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryVariance {
    pub category: Symbol,
    pub initial_allocated: i128,
    pub current_allocated: i128,
    pub disbursed: i128,
    pub remaining: i128,
    pub reallocation_delta: i128,
    pub unspent_variance: i128,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(
        env: Env,
        governance: Address,
        reviewers: Vec<Address>,
        approval_threshold: u32,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(GrantsError::AlreadyInitialized);
        }
        governance.require_auth();
        if reviewers.is_empty() || approval_threshold == 0 || approval_threshold > reviewers.len() {
            return Err(GrantsError::InvalidConfig);
        }
        let cfg = Config {
            governance,
            reviewers,
            approval_threshold,
            token,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    /// Creates a grant with a single default `"general"` category line item.
    pub fn create_grant(
        env: Env,
        recipient: Address,
        milestones: Vec<(Symbol, i128)>,
    ) -> Result<u64, GrantsError> {
        let general = symbol_short!("general");
        let mut categorized = Vec::new(&env);
        let mut total = 0i128;
        for (desc, amt) in milestones.iter() {
            if amt <= 0 {
                return Err(GrantsError::InvalidGrant);
            }
            total = total.checked_add(amt).ok_or(GrantsError::InvalidGrant)?;
            categorized.push_back((desc, general.clone(), amt));
        }
        let mut line_items = Vec::new(&env);
        if total > 0 {
            line_items.push_back((general, total));
        }
        Self::create_grant_with_line_items(env, recipient, categorized, line_items)
    }

    /// Creates a grant with an explicit category line-item budget plan (`category -> amount`)
    /// that must sum exactly to the total milestone amount.
    pub fn create_grant_with_line_items(
        env: Env,
        recipient: Address,
        milestones: Vec<(Symbol, Symbol, i128)>,
        line_items: Vec<(Symbol, i128)>,
    ) -> Result<u64, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if milestones.is_empty() || line_items.is_empty() || line_items.len() > MAX_CATEGORIES {
            return Err(GrantsError::InvalidGrant);
        }

        let mut built_items: Vec<CategoryLineItem> = Vec::new(&env);
        let mut plan_sum: i128 = 0;
        for (cat, amount) in line_items.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidGrant);
            }
            for existing in built_items.iter() {
                if existing.category == cat {
                    return Err(GrantsError::DuplicateCategory);
                }
            }
            plan_sum = plan_sum
                .checked_add(amount)
                .ok_or(GrantsError::InvalidGrant)?;
            built_items.push_back(CategoryLineItem {
                category: cat,
                initial_allocated: amount,
                current_allocated: amount,
                disbursed: 0,
                remaining: amount,
            });
        }

        let mut total: i128 = 0;
        let mut built_milestones: Vec<Milestone> = Vec::new(&env);
        for (desc, category, amount) in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidGrant);
            }
            if !built_items.iter().any(|li| li.category == category) {
                return Err(GrantsError::CategoryNotFound);
            }
            total = total.checked_add(amount).ok_or(GrantsError::InvalidGrant)?;
            built_milestones.push_back(Milestone {
                description: desc,
                category,
                amount,
                approvals: Vec::new(&env),
                released: false,
            });
        }

        if plan_sum != total {
            return Err(GrantsError::LineItemMismatch);
        }

        token::Client::new(&env, &cfg.token).transfer(
            &cfg.governance,
            &env.current_contract_address(),
            &total,
        );

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        let grant = Grant {
            id,
            recipient: recipient.clone(),
            total_amount: total,
            released_amount: 0,
            milestones: built_milestones,
            status: GrantStatus::Active,
        };
        env.storage().persistent().set(&DataKey::Grant(id), &grant);
        env.storage()
            .persistent()
            .set(&DataKey::LineItems(id), &built_items);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(id + 1));

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("create")),
            (id, recipient, total),
        );
        Ok(id)
    }

    /// Reviewer-approved reallocation moving remaining budget from one category to another.
    pub fn reallocate_category(
        env: Env,
        reviewer: Address,
        grant_id: u64,
        from_category: Symbol,
        to_category: Symbol,
        amount: i128,
    ) -> Result<Vec<CategoryLineItem>, GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::InvalidConfig);
        }
        reviewer.require_auth();

        if amount <= 0 || from_category == to_category {
            return Err(GrantsError::InvalidGrant);
        }

        let grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }

        let mut items: Vec<CategoryLineItem> = env
            .storage()
            .persistent()
            .get(&DataKey::LineItems(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        let mut from_idx: Option<u32> = None;
        let mut to_idx: Option<u32> = None;
        for (idx, item) in items.iter().enumerate() {
            if item.category == from_category {
                from_idx = Some(idx as u32);
            }
            if item.category == to_category {
                to_idx = Some(idx as u32);
            }
        }

        let f_idx = from_idx.ok_or(GrantsError::CategoryNotFound)?;
        let t_idx = to_idx.ok_or(GrantsError::CategoryNotFound)?;

        let mut from_item = items.get(f_idx).ok_or(GrantsError::CategoryNotFound)?;
        let mut to_item = items.get(t_idx).ok_or(GrantsError::CategoryNotFound)?;

        if from_item.remaining < amount {
            return Err(GrantsError::CategoryOverspend);
        }

        from_item.current_allocated -= amount;
        from_item.remaining -= amount;
        to_item.current_allocated += amount;
        to_item.remaining += amount;

        items.set(f_idx, from_item);
        items.set(t_idx, to_item);

        env.storage()
            .persistent()
            .set(&DataKey::LineItems(grant_id), &items);

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("realloc")),
            (grant_id, reviewer, from_category, to_category, amount),
        );
        Ok(items)
    }

    pub fn approve_milestone(
        env: Env,
        reviewer: Address,
        grant_id: u64,
        milestone_idx: u32,
    ) -> Result<bool, GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::InvalidConfig);
        }
        reviewer.require_auth();

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        let mut ms = grant
            .milestones
            .get(milestone_idx)
            .ok_or(GrantsError::MilestoneNotFound)?;
        if ms.released {
            return Err(GrantsError::MilestoneAlreadyReleased);
        }
        if ms.approvals.contains(&reviewer) {
            return Err(GrantsError::DuplicateApproval);
        }

        ms.approvals.push_back(reviewer);
        let reached = ms.approvals.len() >= cfg.approval_threshold;
        let mut released_now = false;
        if reached {
            Self::debit_category_line_item(&env, grant_id, &ms.category, ms.amount)?;
            ms.released = true;
            grant.released_amount += ms.amount;
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &grant.recipient,
                &ms.amount,
            );
            released_now = true;
            env.events().publish(
                (symbol_short!("grants"), symbol_short!("release")),
                (grant_id, milestone_idx, ms.category.clone(), ms.amount),
            );
        }
        grant.milestones.set(milestone_idx, ms);

        let all_done = grant.milestones.iter().all(|m| m.released);
        if all_done {
            grant.status = GrantStatus::Completed;
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(released_now)
    }

    pub fn cancel_grant(env: Env, grant_id: u64) -> Result<i128, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        let refund = grant.total_amount - grant.released_amount;
        grant.status = GrantStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        if refund > 0 {
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &cfg.governance,
                &refund,
            );
        }
        env.events().publish(
            (symbol_short!("grants"), symbol_short!("cancel")),
            (grant_id, refund),
        );
        Ok(refund)
    }

    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn grant_line_items(env: Env, grant_id: u64) -> Vec<CategoryLineItem> {
        env.storage()
            .persistent()
            .get(&DataKey::LineItems(grant_id))
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn grant_variance_report(env: Env, grant_id: u64) -> Vec<CategoryVariance> {
        let items = Self::grant_line_items(env.clone(), grant_id);
        let mut report = Vec::new(&env);
        for item in items.iter() {
            report.push_back(CategoryVariance {
                category: item.category,
                initial_allocated: item.initial_allocated,
                current_allocated: item.current_allocated,
                disbursed: item.disbursed,
                remaining: item.remaining,
                reallocation_delta: item.current_allocated - item.initial_allocated,
                unspent_variance: item.initial_allocated - item.disbursed,
            });
        }
        report
    }

    fn debit_category_line_item(
        env: &Env,
        grant_id: u64,
        category: &Symbol,
        amount: i128,
    ) -> Result<(), GrantsError> {
        let mut items: Vec<CategoryLineItem> = env
            .storage()
            .persistent()
            .get(&DataKey::LineItems(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        let mut target_idx: Option<u32> = None;
        for (idx, item) in items.iter().enumerate() {
            if item.category == *category {
                target_idx = Some(idx as u32);
                break;
            }
        }
        let idx = target_idx.ok_or(GrantsError::CategoryNotFound)?;
        let mut item = items.get(idx).ok_or(GrantsError::CategoryNotFound)?;
        if item.remaining < amount {
            return Err(GrantsError::CategoryOverspend);
        }
        item.disbursed += amount;
        item.remaining -= amount;
        items.set(idx, item);
        env.storage()
            .persistent()
            .set(&DataKey::LineItems(grant_id), &items);
        Ok(())
    }

    fn read_config(env: &Env) -> Result<Config, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec};

    #[test]
    fn line_item_budgets_enforce_caps_reallocate_and_report_variance() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let r1 = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&gov, &20_000);

        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &cid);

        client.initialize(&gov, &vec![&env, r1.clone()], &1, &token_id);

        let dev = symbol_short!("dev");
        let audit = symbol_short!("audit");

        // Mismatched line-item sum is rejected
        assert_eq!(
            client.try_create_grant_with_line_items(
                &recipient,
                &vec![
                    &env,
                    (symbol_short!("m1"), dev.clone(), 1_000),
                    (symbol_short!("m2"), audit.clone(), 1_000),
                ],
                &vec![&env, (dev.clone(), 1_000), (audit.clone(), 500)],
            ),
            Err(Ok(GrantsError::LineItemMismatch))
        );

        // Create grant where m2 requests 1_200 from `audit` which initially only has 800
        // (`dev` has 1_200, `audit` has 800 -> sum = 2_000 == m1(800) + m2(1_200))
        let gid = client.create_grant_with_line_items(
            &recipient,
            &vec![
                &env,
                (symbol_short!("m1"), dev.clone(), 800),
                (symbol_short!("m2"), audit.clone(), 1_200),
            ],
            &vec![&env, (dev.clone(), 1_200), (audit.clone(), 800)],
        );

        // Approve m1 (800 dev) -> succeeds; dev remaining = 400
        assert!(client.approve_milestone(&r1, &gid, &0));

        // Approve m2 (1_200 audit) -> fails with CategoryOverspend because audit only has 800
        assert_eq!(
            client.try_approve_milestone(&r1, &gid, &1),
            Err(Ok(GrantsError::CategoryOverspend))
        );

        // Reviewer reallocates 400 from dev to audit
        client.reallocate_category(&r1, &gid, &dev, &audit, &400);

        // Invariant: sum of category remainders == grant remainder (1_200)
        let items = client.grant_line_items(&gid);
        let rem_sum: i128 = items.iter().map(|i| i.remaining).sum();
        let grant = client.get_grant(&gid).unwrap();
        assert_eq!(rem_sum, grant.total_amount - grant.released_amount);

        // Now m2 releases cleanly
        assert!(client.approve_milestone(&r1, &gid, &1));

        let variance = client.grant_variance_report(&gid);
        assert_eq!(variance.get(0).unwrap().reallocation_delta, -400);
        assert_eq!(variance.get(1).unwrap().reallocation_delta, 400);
        assert_eq!(variance.get(1).unwrap().disbursed, 1_200);
    }
}
