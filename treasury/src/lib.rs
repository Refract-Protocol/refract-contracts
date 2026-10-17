//! # RefractTreasury
//!
//! Protocol fee and reserve vault with scoped, expiring keeper delegations
//! (`delegate`, `revoke`, `require_scoped_auth`) for least-privilege automation.

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

pub const SCOPE_EXEC_DUE_ESCROW: u32 = 1 << 0;
pub const SCOPE_EXEC_TIMELOCK: u32 = 1 << 1;
pub const SCOPE_EXTEND_TTL: u32 = 1 << 2;
pub const SCOPE_REFRESH_RATE: u32 = 1 << 3;
pub const ALL_VALID_SCOPES: u32 =
    SCOPE_EXEC_DUE_ESCROW | SCOPE_EXEC_TIMELOCK | SCOPE_EXTEND_TTL | SCOPE_REFRESH_RATE;

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
    InvalidScope = 7,
    InvalidDelegationParams = 8,
    DelegationNotFound = 9,
    DelegationExpired = 10,
    DelegationExhausted = 11,
    ScopeNotPermitted = 12,
    PrivilegeEscalationForbidden = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    Balance,
    Delegation(Address),
    DelegateList,
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
pub struct KeeperDelegation {
    pub grantor: Address,
    pub grantee: Address,
    pub scope: u32,
    pub expires_at_ledger: u32,
    pub remaining_calls: u32,
    pub active: bool,
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
            token,
            max_single_withdrawal,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    /// Grants a keeper a narrowly-scoped, expiring, call-bounded delegation.
    /// Only direct governance may grant delegations (delegates cannot amplify or re-delegate).
    pub fn delegate(
        env: Env,
        caller: Address,
        grantee: Address,
        scope: u32,
        expires_at_ledger: u32,
        max_calls: u32,
    ) -> Result<KeeperDelegation, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        caller.require_auth();
        if caller != cfg.governance {
            return Err(TreasuryError::PrivilegeEscalationForbidden);
        }
        if scope == 0 || (scope & !ALL_VALID_SCOPES) != 0 {
            return Err(TreasuryError::InvalidScope);
        }
        if max_calls == 0 || expires_at_ledger <= env.ledger().sequence() {
            return Err(TreasuryError::InvalidDelegationParams);
        }

        let record = KeeperDelegation {
            grantor: cfg.governance,
            grantee: grantee.clone(),
            scope,
            expires_at_ledger,
            remaining_calls: max_calls,
            active: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Delegation(grantee.clone()), &record);

        let mut list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegateList)
            .unwrap_or_else(|| Vec::new(&env));
        if !list.contains(&grantee) {
            list.push_back(grantee.clone());
            env.storage().instance().set(&DataKey::DelegateList, &list);
        }

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("del_grnt")),
            (grantee, scope, expires_at_ledger, max_calls),
        );
        Ok(record)
    }

    /// Revokes a keeper's delegation immediately.
    pub fn revoke(env: Env, caller: Address, grantee: Address) -> Result<(), TreasuryError> {
        let cfg = Self::read_config(&env)?;
        caller.require_auth();
        if caller != cfg.governance {
            return Err(TreasuryError::PrivilegeEscalationForbidden);
        }
        if let Some(mut record) = env
            .storage()
            .persistent()
            .get::<DataKey, KeeperDelegation>(&DataKey::Delegation(grantee.clone()))
        {
            record.active = false;
            record.remaining_calls = 0;
            env.storage()
                .persistent()
                .set(&DataKey::Delegation(grantee.clone()), &record);
        }
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("del_rev")),
            (grantee,),
        );
        Ok(())
    }

    /// Guarded automation entrypoint: extends storage TTL (requires `SCOPE_EXTEND_TTL`).
    pub fn automated_extend_ttl(
        env: Env,
        caller: Address,
        threshold: u32,
        extend_to: u32,
    ) -> Result<(), TreasuryError> {
        Self::require_scoped_auth(&env, &caller, SCOPE_EXTEND_TTL)?;
        env.storage().instance().extend_ttl(threshold, extend_to);
        Ok(())
    }

    /// Guarded automation entrypoint: settles a due escrow (requires `SCOPE_EXEC_DUE_ESCROW`).
    pub fn automated_settle_due_escrow(
        env: Env,
        caller: Address,
        escrow_id: u64,
    ) -> Result<u64, TreasuryError> {
        Self::require_scoped_auth(&env, &caller, SCOPE_EXEC_DUE_ESCROW)?;
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("esc_stl")),
            (caller, escrow_id),
        );
        Ok(escrow_id)
    }

    /// Guarded automation entrypoint: executes a due timelock action (requires `SCOPE_EXEC_TIMELOCK`).
    pub fn automated_exec_timelock(
        env: Env,
        caller: Address,
        action_id: u64,
    ) -> Result<u64, TreasuryError> {
        Self::require_scoped_auth(&env, &caller, SCOPE_EXEC_TIMELOCK)?;
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("tl_exec")),
            (caller, action_id),
        );
        Ok(action_id)
    }

    /// Guarded automation entrypoint: triggers rate refresh (requires `SCOPE_REFRESH_RATE`).
    pub fn automated_refresh_rate(env: Env, caller: Address) -> Result<u64, TreasuryError> {
        Self::require_scoped_auth(&env, &caller, SCOPE_REFRESH_RATE)?;
        let ts = env.ledger().timestamp();
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("rt_refr")),
            (caller, ts),
        );
        Ok(ts)
    }

    /// Admin-only withdrawal; rejects any delegate attempting privilege escalation.
    pub fn withdraw(
        env: Env,
        caller: Address,
        recipient: Address,
        amount: i128,
        reason: Symbol,
    ) -> Result<i128, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        caller.require_auth();
        if caller != cfg.governance {
            return Err(TreasuryError::PrivilegeEscalationForbidden);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if amount > cfg.max_single_withdrawal {
            return Err(TreasuryError::CapExceeded);
        }
        let bal = Self::read_balance(&env)?;
        if amount > bal {
            return Err(TreasuryError::InsufficientBalance);
        }
        token::Client::new(&env, &cfg.token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );
        let next = bal - amount;
        env.storage().instance().set(&DataKey::Balance, &next);
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("withdraw")),
            (recipient, amount, reason),
        );
        Ok(next)
    }

    pub fn delegation(env: Env, grantee: Address) -> Option<KeeperDelegation> {
        env.storage()
            .persistent()
            .get(&DataKey::Delegation(grantee))
    }

    pub fn active_delegations(env: Env) -> Vec<KeeperDelegation> {
        let list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegateList)
            .unwrap_or_else(|| Vec::new(&env));
        let now = env.ledger().sequence();
        let mut out = Vec::new(&env);
        for addr in list.iter() {
            if let Some(d) = env
                .storage()
                .persistent()
                .get::<DataKey, KeeperDelegation>(&DataKey::Delegation(addr))
            {
                if d.active && d.remaining_calls > 0 && now <= d.expires_at_ledger {
                    out.push_back(d);
                }
            }
        }
        out
    }

    fn require_scoped_auth(
        env: &Env,
        caller: &Address,
        required_scope: u32,
    ) -> Result<(), TreasuryError> {
        let cfg = Self::read_config(env)?;
        caller.require_auth();
        if *caller == cfg.governance {
            return Ok(());
        }

        let mut record: KeeperDelegation = env
            .storage()
            .persistent()
            .get(&DataKey::Delegation(caller.clone()))
            .ok_or(TreasuryError::DelegationNotFound)?;

        if !record.active {
            return Err(TreasuryError::DelegationNotFound);
        }

        let now = env.ledger().sequence();
        if now > record.expires_at_ledger {
            env.events().publish(
                (symbol_short!("treasury"), symbol_short!("del_exp")),
                (caller.clone(), required_scope, now),
            );
            return Err(TreasuryError::DelegationExpired);
        }

        if record.remaining_calls == 0 {
            return Err(TreasuryError::DelegationExhausted);
        }

        if (record.scope & required_scope) != required_scope {
            return Err(TreasuryError::ScopeNotPermitted);
        }

        record.remaining_calls -= 1;
        if record.remaining_calls == 0 {
            record.active = false;
            env.events().publish(
                (symbol_short!("treasury"), symbol_short!("del_exh")),
                (caller.clone(), record.scope),
            );
        }
        env.storage()
            .persistent()
            .set(&DataKey::Delegation(caller.clone()), &record);

        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("del_use")),
            (caller.clone(), required_scope, record.remaining_calls),
        );
        Ok(())
    }

    fn read_config(env: &Env) -> Result<Config, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, TreasuryError> {
        let _ = Self::read_config(env)?;
        Ok(env.storage().instance().get(&DataKey::Balance).unwrap_or(0))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    #[test]
    fn scoped_keeper_delegation_enforces_scope_max_calls_expiry_and_no_escalation() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let keeper = Address::generate(&env);
        let token_id = Address::generate(&env);

        let cid = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &cid);

        client.initialize(&gov, &token_id, &5_000);

        // Grant keeper SCOPE_EXEC_DUE_ESCROW | SCOPE_EXTEND_TTL with max_calls = 2, expires_at = 10
        client.delegate(
            &gov,
            &keeper,
            &(SCOPE_EXEC_DUE_ESCROW | SCOPE_EXTEND_TTL),
            &10,
            &2,
        );
        assert_eq!(client.active_delegations().len(), 1);

        // Out-of-scope capability (SCOPE_EXEC_TIMELOCK) is rejected without consuming max_calls
        assert_eq!(
            client.try_automated_exec_timelock(&keeper, &1),
            Err(Ok(TreasuryError::ScopeNotPermitted))
        );
        assert_eq!(client.delegation(&keeper).unwrap().remaining_calls, 2);

        // Privilege escalation (withdraw or re-delegate) is forbidden
        assert_eq!(
            client.try_withdraw(&keeper, &keeper, &100, &symbol_short!("hack")),
            Err(Ok(TreasuryError::PrivilegeEscalationForbidden))
        );
        assert_eq!(
            client.try_delegate(&keeper, &keeper, &SCOPE_EXEC_TIMELOCK, &20, &5),
            Err(Ok(TreasuryError::PrivilegeEscalationForbidden))
        );

        // Two valid calls succeed and exhaust the delegation
        assert_eq!(client.automated_settle_due_escrow(&keeper, &42), 42);
        client.automated_extend_ttl(&keeper, &10, &100);
        assert_eq!(client.delegation(&keeper).unwrap().remaining_calls, 0);
        assert_eq!(
            client.try_automated_settle_due_escrow(&keeper, &43),
            Err(Ok(TreasuryError::DelegationNotFound))
        );

        // Re-grant with expires_at = 10 and test exact ledger expiry boundary (ledger 11 fails)
        client.delegate(&gov, &keeper, &SCOPE_REFRESH_RATE, &10, &5);
        env.ledger().with_mut(|l| l.sequence_number = 10);
        client.automated_refresh_rate(&keeper);
        env.ledger().with_mut(|l| l.sequence_number = 11);
        assert_eq!(
            client.try_automated_refresh_rate(&keeper),
            Err(Ok(TreasuryError::DelegationExpired))
        );
    }
}
