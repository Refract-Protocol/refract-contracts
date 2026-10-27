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
    ReentrancyLock,
    /// Whether the contract is paused (instance, #211).
    Paused,
    /// On-chain storage schema version (instance, #212).
    SchemaVersion,
    /// Pauser role holder (persistent, #211).
    PauserRole(Address),
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
    ReentrancyDetected = 21,
    /// The contract is paused (#211).
    Paused = 22,
}

#[contract]
pub struct RefractGrants;

impl RefractGrants {
    /// Current on-chain storage schema version for the grants contract.
    pub const SCHEMA_VERSION: u32 = 1;
}

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
        Self::require_not_paused(&env)?;
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
        Self::require_not_paused(&env)?;
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
        Self::acquire_reentrancy_guard(&env)?;
        let mut grant = match Self::grant(&env, grant_id) {
            Ok(g) => g,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };
        if grant.recipient != recipient {
            Self::release_reentrancy_guard(&env);
            return Err(GrantsError::NotRecipient);
        }
        let gross = grant.claimable;
        if gross <= 0 {
            Self::release_reentrancy_guard(&env);
            return Err(GrantsError::NothingToClaim);
        }
        let quote = match Self::quote_payout(env.clone(), gross) {
            Ok(q) => q,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };
        let token_address = match Self::token(&env) {
            Ok(t) => t,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };

        // Effects: zero claimable, mark milestones claimed, and record accrued fees
        // BEFORE invoking external token.transfer (strict CEI).
        if quote.fee > 0 {
            let fees: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::AccruedFees(token_address.clone()))
                .unwrap_or(0);
            let updated = match fees.checked_add(quote.fee) {
                Some(u) => u,
                None => {
                    Self::release_reentrancy_guard(&env);
                    return Err(GrantsError::ArithmeticError);
                }
            };
            env.storage()
                .persistent()
                .set(&DataKey::AccruedFees(token_address.clone()), &updated);
        }

        grant.claimable = 0;
        for index in 0..grant.milestones.len() {
            let mut milestone = match grant.milestones.get(index) {
                Some(m) => m,
                None => {
                    Self::release_reentrancy_guard(&env);
                    return Err(GrantsError::InvalidMilestoneIndex);
                }
            };
            if milestone.confirmed {
                milestone.claimed = true;
                grant.milestones.set(index, milestone);
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        // Interactions: outbound token transfer while reentrancy lock is held.
        if quote.net > 0 {
            token::Client::new(&env, &token_address).transfer(
                &env.current_contract_address(),
                &recipient,
                &quote.net,
            );
        }
        Self::release_reentrancy_guard(&env);
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
        Self::acquire_reentrancy_guard(&env)?;
        let expected_token = match Self::token(&env) {
            Ok(t) => t,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };
        if token != expected_token {
            Self::release_reentrancy_guard(&env);
            return Err(GrantsError::InvalidToken);
        }
        let fees = Self::accrued_fees(env.clone(), token.clone());
        if fees == 0 {
            Self::release_reentrancy_guard(&env);
            return Ok(0);
        }
        let treasury: Address = match env.storage().instance().get(&DataKey::Treasury) {
            Some(t) => t,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(GrantsError::TreasuryNotConfigured);
            }
        };
        // Effects: zero accrued fees BEFORE invoking outbound token.transfer (strict CEI).
        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token.clone()), &0i128);

        // Interactions: outbound token transfer while reentrancy lock is held.
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &treasury,
            &fees,
        );
        Self::release_reentrancy_guard(&env);
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

    fn acquire_reentrancy_guard(env: &Env) -> Result<(), GrantsError> {
        if env.storage().temporary().has(&DataKey::ReentrancyLock) {
            return Err(GrantsError::ReentrancyDetected);
        }
        env.storage().temporary().set(&DataKey::ReentrancyLock, &true);
        Ok(())
    }

    fn release_reentrancy_guard(env: &Env) {
        env.storage().temporary().remove(&DataKey::ReentrancyLock);
    }

    // ─── #211: Coordinated pause facility ─────────────────────────────────

    /// Grant the pauser role to `account`. Governance-only.
    pub fn grant_pauser_role(
        env: Env,
        caller: Address,
        account: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage()
            .persistent()
            .set(&DataKey::PauserRole(account.clone()), &true);
        env.events().publish((symbol_short!("PROLE_G"),), account);
        Ok(())
    }

    /// Revoke the pauser role from `account`. Governance-only.
    pub fn revoke_pauser_role(
        env: Env,
        caller: Address,
        account: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage()
            .persistent()
            .remove(&DataKey::PauserRole(account.clone()));
        env.events().publish((symbol_short!("PROLE_R"),), account);
        Ok(())
    }

    /// Pause the grants contract. Callable by the pauser role or governance.
    /// While paused, `create_grant` and `confirm_milestone` are blocked.
    /// `claim` (outflow) remains callable so confirmed-but-unclaimed funds
    /// can be settled by grantees.
    pub fn pause(env: Env, caller: Address) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance_or_pauser(&env, &caller)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("GR_PAUS"),), caller);
        Ok(())
    }

    /// Unpause the grants contract. Governance-only (pauser can freeze but not
    /// unilaterally resume, matching the pool guardian asymmetry).
    pub fn unpause(env: Env, caller: Address) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("GR_UNPS"),), caller);
        Ok(())
    }

    /// Whether the grants contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ─── #212: Timelock-gated upgrade + state migration ───────────────────

    /// Replace the contract wasm. Governance (timelock) only.
    /// Must be queued through the timelock to enforce the mandatory upgrade delay.
    pub fn upgrade(
        env: Env,
        caller: Address,
        new_wasm_hash: soroban_sdk::BytesN<32>,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("GR_UPGR"),), new_wasm_hash);
        Ok(())
    }

    /// Migrate on-chain storage from a previous schema to `SCHEMA_VERSION`.
    /// Governance (timelock) only. Idempotent: calling it when already at the
    /// current version returns the current version unchanged.
    pub fn migrate(env: Env, caller: Address) -> Result<u32, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        let stored: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(0);
        if stored >= Self::SCHEMA_VERSION {
            return Ok(stored);
        }
        // ── Schema 0 → 1 ──────────────────────────────────────────────────
        // Version 1 is the initial schema. Ensure instance keys introduced
        // in v1 carry sensible defaults on any pre-v1 deployment.
        if stored < 1 {
            if !env.storage().instance().has(&DataKey::NextGrantId) {
                env.storage().instance().set(&DataKey::NextGrantId, &0u64);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &Self::SCHEMA_VERSION);
        env.events()
            .publish((symbol_short!("GR_MIGR"),), Self::SCHEMA_VERSION);
        Ok(Self::SCHEMA_VERSION)
    }

    /// The schema version currently stored on-chain (0 if never migrated).
    pub fn schema_version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(0)
    }

    fn require_not_paused(env: &Env) -> Result<(), GrantsError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(GrantsError::Paused);
        }
        Ok(())
    }

    fn require_governance_or_pauser(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let governance = Self::governance(env)?;
        if *caller == governance {
            return Ok(());
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::PauserRole(caller.clone()))
        {
            return Ok(());
        }
        Err(GrantsError::Unauthorized)
    }
}

#[cfg(test)]
mod reentrancy_test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[contract]
    pub struct HostileGrantsToken;

    #[contractimpl]
    impl HostileGrantsToken {
        pub fn set_target(env: Env, grants: Address, recipient: Address, grant_id: u64) {
            env.storage().instance().set(&"grants", &grants);
            env.storage().instance().set(&"recipient", &recipient);
            env.storage().instance().set(&"grant_id", &grant_id);
        }

        pub fn last_reentry_err(env: Env) -> Option<GrantsError> {
            env.storage().instance().get(&"last_err")
        }

        pub fn transfer(env: Env, _from: Address, _to: Address, _amount: i128) {
            if env.storage().temporary().has(&"in_hook") {
                return;
            }
            env.storage().temporary().set(&"in_hook", &true);

            let grants: Address = env.storage().instance().get(&"grants").unwrap();
            let recipient: Address = env.storage().instance().get(&"recipient").unwrap();
            let grant_id: u64 = env.storage().instance().get(&"grant_id").unwrap();
            let client = RefractGrantsClient::new(&env, &grants);

            if let Err(Ok(err)) = client.try_claim(&recipient, &grant_id) {
                env.storage().instance().set(&"last_err", &err);
            }

            env.storage().temporary().remove(&"in_hook");
        }
    }

    #[test]
    fn reentrant_grant_claim_during_token_transfer_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let recipient = Address::generate(&env);
        let hostile_token_id = env.register_contract(None, HostileGrantsToken);
        let hostile_token = HostileGrantsTokenClient::new(&env, &hostile_token_id);

        let grants_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &grants_id);
        client.initialize(&governance, &hostile_token_id);

        let milestones = Vec::from_array(&env, [200i128, 300i128]);
        let grant_id = client.create_grant(&governance, &recipient, &500, &milestones);
        client.confirm_milestone(&governance, &grant_id, &0);

        hostile_token.set_target(&grants_id, &recipient, &grant_id);
        let claimed = client.claim(&recipient, &grant_id);
        assert_eq!(claimed, 200);
        assert_eq!(
            hostile_token.last_reentry_err(),
            Some(GrantsError::ReentrancyDetected)
        );
        assert_eq!(client.get_grant(&grant_id).unwrap().claimable, 0);
    }
}

#[cfg(test)]
#[path = "proptest_invariants.rs"]
mod proptest_invariants;
