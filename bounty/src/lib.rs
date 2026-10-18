#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, IntoVal,
    Symbol, Val, Vec,
};

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ContractRole {
    Pool = 0,
    Oracle = 1,
    Policy = 2,
    Governance = 3,
    Timelock = 4,
    Treasury = 5,
    Backstop = 6,
    Bounty = 7,
    Grants = 8,
    VeToken = 9,
    Multisig = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleRotationRecord {
    pub role: ContractRole,
    pub old_address: Option<Address>,
    pub new_address: Address,
    pub rotated_at: u64,
}

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Optional external trusted contract registry.
    Registry,
    /// Whether initial deployment bootstrap has completed.
    BootstrapCompleted,
    /// Whether runtime allowlist verification is active.
    EnforceAllowlist,
    /// Role -> Address mapping.
    RoleAddress(ContractRole),
    /// Address -> Role mapping.
    AddressRole(Address),
    /// Historical audit trail of role rotations.
    RotationHistory(ContractRole),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
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
    UntrustedContract = 7,
    DuplicateRoleAddress = 8,
    BootstrapCompleted = 9,
}

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address. There is
/// deliberately no single admin key with payout authority: a compromised admin
/// key authorizing arbitrary "bounty" payouts is a direct fund-drain vector.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::BootstrapCompleted, &false);
        env.storage()
            .instance()
            .set(&DataKey::EnforceAllowlist, &false);
        Ok(())
    }

    /// Configure an external trusted contract registry (e.g. `RefractTimelock`).
    pub fn set_registry(env: Env, caller: Address, registry: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Registry, &registry);
        env.storage()
            .instance()
            .set(&DataKey::EnforceAllowlist, &true);
        Ok(())
    }

    /// Register a trusted contract role during deployment bootstrap.
    pub fn bootstrap_register_role(
        env: Env,
        caller: Address,
        role: ContractRole,
        contract_address: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::BootstrapCompleted)
            .unwrap_or(false)
        {
            return Err(BountyError::BootstrapCompleted);
        }
        Self::apply_role_update(&env, role, contract_address, true)
    }

    /// Permanently lock bootstrap mode and enable runtime allowlist checks.
    pub fn complete_bootstrap(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::BootstrapCompleted)
            .unwrap_or(false)
        {
            return Err(BountyError::BootstrapCompleted);
        }
        env.storage()
            .instance()
            .set(&DataKey::BootstrapCompleted, &true);
        env.storage()
            .instance()
            .set(&DataKey::EnforceAllowlist, &true);
        Ok(())
    }

    /// Rotate the contract address for `role` via the registered `Timelock`
    /// (or `governance` if `Timelock` was not separately registered).
    pub fn rotate_role_address(
        env: Env,
        caller: Address,
        role: ContractRole,
        new_address: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let boot_done: bool = env
            .storage()
            .instance()
            .get(&DataKey::BootstrapCompleted)
            .unwrap_or(false);
        if boot_done {
            let timelock: Option<Address> = env
                .storage()
                .persistent()
                .get(&DataKey::RoleAddress(ContractRole::Timelock));
            if Some(caller.clone()) != timelock {
                return Err(BountyError::Unauthorized);
            }
        } else {
            let governance = Self::governance_internal(&env)?;
            if caller != governance {
                return Err(BountyError::Unauthorized);
            }
        }
        Self::apply_role_update(&env, role, new_address.clone(), false)?;
        if role == ContractRole::Governance || role == ContractRole::Timelock {
            env.storage()
                .instance()
                .set(&DataKey::Governance, &new_address);
        }
        Ok(())
    }

    pub fn resolve_role(env: Env, role: ContractRole) -> Option<Address> {
        if let Some(reg) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Registry)
        {
            let mut args: Vec<Val> = Vec::new(&env);
            args.push_back(role.into_val(&env));
            if let Ok(Ok(resolved)) = env
                .try_invoke_contract::<Option<Address>, soroban_sdk::InvokeError>(
                    &reg,
                    &Symbol::new(&env, "resolve_role"),
                    args,
                )
            {
                if resolved.is_some() {
                    return resolved;
                }
            }
        }
        env.storage().persistent().get(&DataKey::RoleAddress(role))
    }

    pub fn rotation_history(env: Env, role: ContractRole) -> Vec<RoleRotationRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::RotationHistory(role))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token_internal(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance_internal(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Top up the reward pool strictly from the registered `Treasury` contract.
    /// Rejects calls from any address not currently registered as
    /// `ContractRole::Treasury`.
    pub fn fund_from_trusted_treasury(
        env: Env,
        treasury_caller: Address,
        amount: i128,
    ) -> Result<(), BountyError> {
        let active_treasury = Self::resolve_role(env.clone(), ContractRole::Treasury)
            .ok_or(BountyError::UntrustedContract)?;
        if treasury_caller != active_treasury {
            return Err(BountyError::UntrustedContract);
        }
        Self::fund(env, treasury_caller, amount)
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// When runtime allowlist enforcement is active, verifies `caller` against
    /// the active `Governance` or `Timelock` role in the trusted registry.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let enforce: bool = env
            .storage()
            .instance()
            .get(&DataKey::EnforceAllowlist)
            .unwrap_or(false);
        if enforce {
            let trusted_gov = Self::resolve_role(env.clone(), ContractRole::Governance);
            let trusted_tl = Self::resolve_role(env.clone(), ContractRole::Timelock);
            if Some(caller.clone()) != trusted_gov && Some(caller.clone()) != trusted_tl {
                return Err(BountyError::UntrustedContract);
            }
        } else {
            let governance = Self::governance_internal(&env)?;
            if caller != governance {
                return Err(BountyError::Unauthorized);
            }
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

        let token = Self::token_internal(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance_internal(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance_internal(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn governance_internal(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token_internal(env: &Env) -> Result<Address, BountyError> {
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

    fn apply_role_update(
        env: &Env,
        role: ContractRole,
        new_address: Address,
        is_bootstrap: bool,
    ) -> Result<(), BountyError> {
        if env
            .storage()
            .persistent()
            .has(&DataKey::AddressRole(new_address.clone()))
        {
            return Err(BountyError::DuplicateRoleAddress);
        }
        let old_address: Option<Address> =
            env.storage().persistent().get(&DataKey::RoleAddress(role));
        if let Some(ref prev) = old_address {
            env.storage()
                .persistent()
                .remove(&DataKey::AddressRole(prev.clone()));
        }
        env.storage()
            .persistent()
            .set(&DataKey::RoleAddress(role), &new_address);
        env.storage()
            .persistent()
            .set(&DataKey::AddressRole(new_address.clone()), &role);

        let now = env.ledger().timestamp();
        let mut history: Vec<RoleRotationRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::RotationHistory(role))
            .unwrap_or_else(|| Vec::new(env));
        history.push_back(RoleRotationRecord {
            role,
            old_address: old_address.clone(),
            new_address: new_address.clone(),
            rotated_at: now,
        });
        env.storage()
            .persistent()
            .set(&DataKey::RotationHistory(role), &history);

        let topic = if is_bootstrap {
            Symbol::new(env, "reg_boot")
        } else {
            Symbol::new(env, "role_rot")
        };
        env.events()
            .publish((topic, role), (old_address, new_address, now));
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};

    #[test]
    fn bounty_registry_allowlist_and_role_rotation() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let gov_v1 = Address::generate(&env);
        let timelock = Address::generate(&env);
        let treasury_v1 = Address::generate(&env);
        let treasury_v2 = Address::generate(&env);
        let researcher = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&treasury_v1, &5_000);
        sac.mint(&treasury_v2, &5_000);

        client.initialize(&gov_v1, &token_id);

        env.ledger().with_mut(|l| l.timestamp = 500);
        client.bootstrap_register_role(&gov_v1, &ContractRole::Governance, &gov_v1);
        client.bootstrap_register_role(&gov_v1, &ContractRole::Timelock, &timelock);
        client.bootstrap_register_role(&gov_v1, &ContractRole::Treasury, &treasury_v1);

        assert_eq!(
            client.try_bootstrap_register_role(&gov_v1, &ContractRole::Pool, &treasury_v1),
            Err(Ok(BountyError::DuplicateRoleAddress))
        );

        client.complete_bootstrap(&gov_v1);
        assert_eq!(
            client.try_bootstrap_register_role(&gov_v1, &ContractRole::Pool, &treasury_v2),
            Err(Ok(BountyError::BootstrapCompleted))
        );

        // Trusted treasury_v1 can fund via fund_from_trusted_treasury; untrusted treasury_v2 is rejected.
        client.fund_from_trusted_treasury(&treasury_v1, &2_000);
        assert_eq!(
            client.try_fund_from_trusted_treasury(&treasury_v2, &1_000),
            Err(Ok(BountyError::UntrustedContract))
        );

        // Rotate Treasury role via Timelock; direct gov_v1 attempt is rejected.
        assert_eq!(
            client.try_rotate_role_address(&gov_v1, &ContractRole::Treasury, &treasury_v2),
            Err(Ok(BountyError::Unauthorized))
        );
        env.ledger().with_mut(|l| l.timestamp = 1_500);
        client.rotate_role_address(&timelock, &ContractRole::Treasury, &treasury_v2);

        // Retired treasury_v1 is now rejected; treasury_v2 succeeds.
        assert_eq!(
            client.try_fund_from_trusted_treasury(&treasury_v1, &500),
            Err(Ok(BountyError::UntrustedContract))
        );
        client.fund_from_trusted_treasury(&treasury_v2, &1_000);
        assert_eq!(client.balance(), 3_000);

        // Award by trusted timelock succeeds; random caller is rejected with UntrustedContract.
        let report_hash = BytesN::from_array(&env, &[7u8; 32]);
        assert_eq!(
            client.try_award(&treasury_v2, &researcher, &500, &report_hash),
            Err(Ok(BountyError::UntrustedContract))
        );
        client.award(&timelock, &researcher, &500, &report_hash);
        assert_eq!(client.balance(), 2_500);

        let history = client.rotation_history(&ContractRole::Treasury);
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(1).unwrap().old_address, Some(treasury_v1));
        assert_eq!(history.get(1).unwrap().new_address, treasury_v2);
    }
}
