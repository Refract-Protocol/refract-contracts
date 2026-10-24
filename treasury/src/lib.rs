//! # RefractTreasury
//!
//! Token-custody vault and payment-request execution engine.
//!
//! ## Cross-Contract Call Graph & Reentrancy Safety (#241)
//!
//! - **Inbound calls**: Admin/timelock invokes `withdraw`, `approve_payment_request`,
//!   `reject_payment_request`, and `execute_payment_request`; requesters invoke
//!   `submit_payment_request` and `cancel_payment_request`.
//! - **Outbound calls**: `token::Client::transfer` on the requested asset.
//! - **CEI & Reentrancy Invariant**: `execute_payment_request` acquires a
//!   temporary-storage reentrancy lock (`DataKey::ReentrancyLock`), transitions
//!   the request to [`PaymentStatus::Executed`] in persistent storage *before*
//!   invoking `token.transfer`, and releases the lock afterward. Any re-entrant
//!   callback into `execute_payment_request` while the lock is held fails
//!   immediately with [`TreasuryError::ReentrancyDetected`].

#![no_std]

extern crate alloc;

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

pub mod payments;

use payments::{PaymentRequest, PaymentStatus, TreasuryError};

pub mod challenge;

pub mod reserve;

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    /// Next payment request id to hand out.
    NextRequestId,
    /// A payment request, keyed by its id.
    Request(u64),
    /// Temporary-storage reentrancy lock for outbound token transfers.
    ReentrancyLock,
}

/// `RefractTreasury` is a deliberately minimal token-custody vault.
///
/// Funds are deposited via plain token transfers to the contract address
/// (no `deposit` entrypoint is required). Withdrawals are admin-gated.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    /// Submit a payment request. The purpose text is kept off-chain; only its
    /// hash is stored. Returns the new request id.
    pub fn submit_payment_request(
        env: Env,
        requester: Address,
        payee: Address,
        token: Address,
        amount: i128,
        purpose_hash: BytesN<32>,
    ) -> Result<u64, TreasuryError> {
        requester.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextRequestId)
            .unwrap_or(0);
        let request = PaymentRequest {
            id,
            requester: requester.clone(),
            payee: payee.clone(),
            token,
            amount,
            purpose_hash,
            status: PaymentStatus::Submitted,
            approver: None,
            created_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Request(id), &request);
        env.storage().instance().set(&DataKey::NextRequestId, &(id + 1));
        env.events()
            .publish((symbol_short!("pay_subm"), id), (requester, payee, amount));
        Ok(id)
    }

    /// Approve a submitted request. Only the admin may approve, and never
    /// their own request.
    pub fn approve_payment_request(env: Env, approver: Address, id: u64) -> Result<(), TreasuryError> {
        approver.require_auth();
        Self::require_admin(&env, &approver)?;
        let mut request = Self::load_request(&env, id)?;
        if request.requester == approver {
            return Err(TreasuryError::SelfApproval);
        }
        if request.status != PaymentStatus::Submitted {
            return Err(TreasuryError::InvalidState);
        }
        request.status = PaymentStatus::Approved;
        request.approver = Some(approver.clone());
        Self::save_request(&env, &request);
        env.events().publish((symbol_short!("pay_appr"), id), approver);
        Ok(())
    }

    /// Reject a submitted request. Only the admin may reject.
    pub fn reject_payment_request(env: Env, approver: Address, id: u64) -> Result<(), TreasuryError> {
        approver.require_auth();
        Self::require_admin(&env, &approver)?;
        let mut request = Self::load_request(&env, id)?;
        if request.status != PaymentStatus::Submitted {
            return Err(TreasuryError::InvalidState);
        }
        request.status = PaymentStatus::Rejected;
        Self::save_request(&env, &request);
        env.events().publish((symbol_short!("pay_rejt"), id), approver);
        Ok(())
    }

    /// Cancel a submitted request. Only the original requester may cancel.
    pub fn cancel_payment_request(env: Env, requester: Address, id: u64) -> Result<(), TreasuryError> {
        requester.require_auth();
        let mut request = Self::load_request(&env, id)?;
        if request.requester != requester {
            return Err(TreasuryError::NotRequester);
        }
        if request.status != PaymentStatus::Submitted {
            return Err(TreasuryError::InvalidState);
        }
        request.status = PaymentStatus::Cancelled;
        Self::save_request(&env, &request);
        env.events().publish((symbol_short!("pay_cncl"), id), requester);
        Ok(())
    }

    /// Pay out an approved request. Only the admin may execute, and never a
    /// request they submitted themselves. State is committed prior to the
    /// outbound transfer (CEI) under a temporary-storage reentrancy guard.
    pub fn execute_payment_request(env: Env, caller: Address, id: u64) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        Self::acquire_reentrancy_guard(&env)?;
        let mut request = match Self::load_request(&env, id) {
            Ok(req) => req,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };
        if request.requester == caller {
            Self::release_reentrancy_guard(&env);
            return Err(TreasuryError::SelfApproval);
        }
        if request.status != PaymentStatus::Approved {
            Self::release_reentrancy_guard(&env);
            return Err(TreasuryError::InvalidState);
        }
        // Effects: transition to Executed and persist BEFORE outbound token transfer (CEI).
        request.status = PaymentStatus::Executed;
        Self::save_request(&env, &request);

        // Interactions: outbound token transfer while reentrancy lock is held.
        let client = token::Client::new(&env, &request.token);
        client.transfer(&env.current_contract_address(), &request.payee, &request.amount);
        Self::release_reentrancy_guard(&env);
        env.events().publish((symbol_short!("pay_exec"), id), request.amount);
        Ok(())
    }

    /// Look up a payment request by id.
    pub fn get_payment_request(env: Env, id: u64) -> Option<PaymentRequest> {
        env.storage().persistent().get(&DataKey::Request(id))
    }

    fn acquire_reentrancy_guard(env: &Env) -> Result<(), TreasuryError> {
        if env.storage().temporary().has(&DataKey::ReentrancyLock) {
            return Err(TreasuryError::ReentrancyDetected);
        }
        env.storage().temporary().set(&DataKey::ReentrancyLock, &true);
        Ok(())
    }

    fn release_reentrancy_guard(env: &Env) {
        env.storage().temporary().remove(&DataKey::ReentrancyLock);
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotAdmin)?;
        if *caller != admin {
            return Err(TreasuryError::NotAdmin);
        }
        Ok(())
    }

    fn load_request(env: &Env, id: u64) -> Result<PaymentRequest, TreasuryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Request(id))
            .ok_or(TreasuryError::RequestNotFound)
    }

    fn save_request(env: &Env, request: &PaymentRequest) {
        env.storage()
            .persistent()
            .set(&DataKey::Request(request.id), request);
    }
}

#[cfg(test)]
mod payments_test;

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract(token_admin.clone());
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }
}
