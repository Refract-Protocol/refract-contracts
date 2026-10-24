#[path = "../../shared/fee.rs"]
mod fee;
#[path = "../../shared/ttl.rs"]
mod ttl;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol, Vec,
};

/// Screening status for a grant recipient (issue #244).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ScreeningStatus {
    /// Governance-approved; may receive grant funds.
    Allowed = 0,
    /// Explicitly blocked; may NOT receive grant funds.
    Blocked = 1,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnchainMilestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnchainGrant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<OnchainMilestone>,
    pub claimable: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutQuote {
    pub gross: i128,
    pub net: i128,
    pub fee: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    ParameterRegistry,
    Treasury,
    Grant(u64),
    AccruedFees(Address),
    /// Screening status for a grant recipient (issue #244).
    RecipientStatus(Address),
}

#[contracttype]
#[derive(Clone)]
pub enum BumpKey {
    Instance,
    Grant(u64),
    AccruedFees(Address),
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
    InvalidFeeBps = 14,
    InvalidToken = 15,
    TreasuryNotConfigured = 16,
    ArithmeticError = 17,
    BatchTooLarge = 18,
    KeyNotBumpable = 19,
    /// The grant recipient has not been screened and allowlisted by governance
    /// (issue #244).
    RecipientNotAllowed = 20,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        governance.require_auth();
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token), &0i128);
        Ok(())
    }

    pub fn set_fee_config(
        env: Env,
        caller: Address,
        treasury: Address,
        parameter_registry: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(GrantsError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage()
            .instance()
            .set(&DataKey::ParameterRegistry, &parameter_registry);
        Ok(())
    }

    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestone_amounts: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        Self::require_governance(&env, &caller)?;
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestone_amounts.is_empty() {
            return Err(GrantsError::NoMilestones);
        }
        // Reject grants to recipients not screened by governance (issue #244).
        if !Self::is_recipient_allowed_inner(&env, &recipient) {
            return Err(GrantsError::RecipientNotAllowed);
        }

        let mut sum = 0i128;
        let mut milestones = Vec::new(&env);
        for amount in milestone_amounts.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            milestones.push_back(OnchainMilestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: u64 = env.storage().instance().get(&DataKey::NextGrantId).unwrap_or(0);
        let next_id = grant_id
            .checked_add(1)
            .ok_or(GrantsError::ArithmeticError)?;
        env.storage().instance().set(&DataKey::NextGrantId, &next_id);
        env.storage().persistent().set(
            &DataKey::Grant(grant_id),
            &OnchainGrant {
                recipient,
                total_amount,
                milestones,
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
        Self::require_governance(&env, &caller)?;
        let mut grant = Self::grant(&env, grant_id)?;
        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }
        for index in 0..milestone_index {
            if !grant
                .milestones
                .get(index)
                .ok_or(GrantsError::InvalidMilestoneIndex)?
                .confirmed
            {
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
        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::ArithmeticError)?;
        grant.milestones.set(milestone_index, milestone);
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let mut grant = Self::grant(&env, grant_id)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let gross = grant.claimable;
        if gross <= 0 {
            return Err(GrantsError::NothingToClaim);
        }
        let quote = Self::quote_payout(env.clone(), gross)?;
        let token_address = Self::token(&env)?;
        if quote.net > 0 {
            token::Client::new(&env, &token_address).transfer(
                &env.current_contract_address(),
                &recipient,
                &quote.net,
            );
        }
        if quote.fee > 0 {
            let fees: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::AccruedFees(token_address.clone()))
                .unwrap_or(0);
            let updated = fees
                .checked_add(quote.fee)
                .ok_or(GrantsError::ArithmeticError)?;
            env.storage()
                .persistent()
                .set(&DataKey::AccruedFees(token_address), &updated);
        }

        grant.claimable = 0;
        for index in 0..grant.milestones.len() {
            let mut milestone = grant
                .milestones
                .get(index)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if milestone.confirmed {
                milestone.claimed = true;
                grant.milestones.set(index, milestone);
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(quote.net)
    }

    pub fn quote_payout(env: Env, gross: i128) -> Result<PayoutQuote, GrantsError> {
        if gross < 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let fee_bps = Self::fee_bps(&env)?;
        let (net, fee) = fee::assess_fee(gross, fee_bps).ok_or(GrantsError::ArithmeticError)?;
        Ok(PayoutQuote { gross, net, fee })
    }

    pub fn accrued_fees(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AccruedFees(token))
            .unwrap_or(0)
    }

    fn fee_bps(env: &Env) -> Result<u32, GrantsError> {
        let Some(registry) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ParameterRegistry)
        else {
            return Ok(0);
        };
        let fee_bps: u32 = env.invoke_contract(
            &registry,
            &Symbol::new(env, "fee_bps"),
            Vec::new(env),
        );
        if fee_bps > fee::MAX_FEE_BPS {
            return Err(GrantsError::InvalidFeeBps);
        }
        Ok(fee_bps)
    }

    pub fn sweep_fees(env: Env, token: Address) -> Result<i128, GrantsError> {
        if token != Self::token(&env)? {
            return Err(GrantsError::InvalidToken);
        }
        let fees = Self::accrued_fees(env.clone(), token.clone());
        if fees == 0 {
            return Ok(0);
        }
        let treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(GrantsError::TreasuryNotConfigured)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &treasury,
            &fees,
        );
        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token), &0i128);
        Ok(fees)
    }

    pub fn bump(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, GrantsError> {
        if keys.len() > ttl::MAX_BUMP_BATCH {
            return Err(GrantsError::BatchTooLarge);
        }
        let extend_to = ttl::clamp_extend(requested_extend);
        for key in keys.iter() {
            match key {
                BumpKey::Instance => env
                    .storage()
                    .instance()
                    .extend_ttl(ttl::MIN_TTL_THRESHOLD, extend_to),
                BumpKey::Grant(grant_id) => {
                    let storage_key = DataKey::Grant(grant_id);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(GrantsError::KeyNotBumpable);
                    }
                    env.storage()
                        .persistent()
                        .extend_ttl(&storage_key, ttl::MIN_TTL_THRESHOLD, extend_to);
                }
                BumpKey::AccruedFees(token) => {
                    let storage_key = DataKey::AccruedFees(token);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(GrantsError::KeyNotBumpable);
                    }
                    env.storage()
                        .persistent()
                        .extend_ttl(&storage_key, ttl::MIN_TTL_THRESHOLD, extend_to);
                }
            }
        }
        Ok(extend_to)
    }

    pub fn get_grant(env: Env, grant_id: u64) -> Option<OnchainGrant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    // ─── Recipient allowlist (issue #244) ─────────────────────────────────

    /// Allow `recipient` to receive grants. Governance/timelock-gated.
    pub fn allow_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), GrantsError> {
        Self::require_governance(&env, &caller)?;
        env.storage().persistent().set(
            &DataKey::RecipientStatus(recipient.clone()),
            &ScreeningStatus::Allowed,
        );
        env.events()
            .publish((symbol_short!("al_allow"),), recipient);
        Ok(())
    }

    /// Block `recipient` from receiving grants. Governance/timelock-gated.
    pub fn block_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), GrantsError> {
        Self::require_governance(&env, &caller)?;
        env.storage().persistent().set(
            &DataKey::RecipientStatus(recipient.clone()),
            &ScreeningStatus::Blocked,
        );
        env.events()
            .publish((symbol_short!("al_block"),), recipient);
        Ok(())
    }

    /// Remove the screening record for `recipient`. Governance/timelock-gated.
    pub fn remove_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), GrantsError> {
        Self::require_governance(&env, &caller)?;
        env.storage()
            .persistent()
            .remove(&DataKey::RecipientStatus(recipient.clone()));
        env.events()
            .publish((symbol_short!("al_remove"),), recipient);
        Ok(())
    }

    /// Return the screening status of `recipient`, if any.
    pub fn recipient_status(env: Env, recipient: Address) -> Option<ScreeningStatus> {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientStatus(recipient))
    }

    /// Return `true` iff `recipient` is currently `Allowed`.
    pub fn is_recipient_allowed(env: Env, recipient: Address) -> bool {
        Self::is_recipient_allowed_inner(&env, &recipient)
    }

    fn is_recipient_allowed_inner(env: &Env, recipient: &Address) -> bool {
        matches!(
            env.storage()
                .persistent()
                .get::<DataKey, ScreeningStatus>(&DataKey::RecipientStatus(recipient.clone())),
            Some(ScreeningStatus::Allowed)
        )
    }

    fn governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        caller.require_auth();
        if *caller != Self::governance(env)? {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn grant(env: &Env, grant_id: u64) -> Result<OnchainGrant, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)
    }

    fn token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}