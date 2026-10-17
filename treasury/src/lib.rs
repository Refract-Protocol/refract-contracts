#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    RequestNotFound = 5,
    InvalidStateTransition = 6,
    SelfApprovalForbidden = 7,
    SelfExecutionForbidden = 8,
    DuplicateApproval = 9,
    BudgetExceeded = 10,
    RequestExpired = 11,
    TimelockNotReady = 12,
    InsufficientVaultBalance = 13,
    InvalidPurposeHash = 14,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    ApprovalThreshold,
    TimelockDelay,
    Operator(Address),
    Approver(Address),
    AssetBudgetCap(Address),
    AssetSpent(Address),
    NextRequestId,
    PaymentRequest(u64),
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PaymentRequestStatus {
    Submitted = 0,
    Approved = 1,
    Queued = 2,
    Executed = 3,
    Rejected = 4,
    Cancelled = 5,
    Expired = 6,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentRequest {
    pub id: u64,
    pub requester: Address,
    pub payee: Address,
    pub asset: Address,
    pub amount: i128,
    pub purpose_hash: BytesN<32>,
    pub approvers: Vec<Address>,
    pub status: PaymentRequestStatus,
    pub submitted_ledger: u32,
    pub expires_at_ledger: u32,
    pub has_timelock_action: bool,
    pub timelock_action_id: u64,
    pub eta_ledger: u32,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to manage roles and budgets.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::ApprovalThreshold, &1u32);
        env.storage().instance().set(&DataKey::TimelockDelay, &5u32);
        env.storage().instance().set(&DataKey::NextRequestId, &0u64);
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

    pub fn set_operator(
        env: Env,
        caller: Address,
        operator: Address,
        enabled: bool,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        env.storage()
            .persistent()
            .set(&DataKey::Operator(operator), &enabled);
        Ok(())
    }

    pub fn set_approver(
        env: Env,
        caller: Address,
        approver: Address,
        enabled: bool,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        env.storage()
            .persistent()
            .set(&DataKey::Approver(approver), &enabled);
        Ok(())
    }

    pub fn set_workflow_params(
        env: Env,
        caller: Address,
        approval_threshold: u32,
        timelock_delay_ledgers: u32,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        if approval_threshold == 0 || timelock_delay_ledgers == 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::ApprovalThreshold, &approval_threshold);
        env.storage()
            .instance()
            .set(&DataKey::TimelockDelay, &timelock_delay_ledgers);
        Ok(())
    }

    pub fn set_asset_budget(
        env: Env,
        caller: Address,
        asset: Address,
        budget_cap: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_admin(&env, &caller)?;
        if budget_cap < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::AssetBudgetCap(asset), &budget_cap);
        Ok(())
    }

    /// Submit a structured payment request. Callable by an authorized operator or admin.
    pub fn submit_payment_request(
        env: Env,
        requester: Address,
        payee: Address,
        asset: Address,
        amount: i128,
        purpose_hash: BytesN<32>,
        ttl_ledgers: u32,
    ) -> Result<u64, TreasuryError> {
        requester.require_auth();
        let is_op: bool = env
            .storage()
            .persistent()
            .get(&DataKey::Operator(requester.clone()))
            .unwrap_or(false);
        let admin = Self::read_admin(&env)?;
        if !is_op && requester != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 || ttl_ledgers == 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if purpose_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(TreasuryError::InvalidPurposeHash);
        }

        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextRequestId)
            .unwrap_or(0u64)
            + 1;
        env.storage()
            .instance()
            .set(&DataKey::NextRequestId, &next_id);

        let now = env.ledger().sequence();
        let req = PaymentRequest {
            id: next_id,
            requester: requester.clone(),
            payee,
            asset,
            amount,
            purpose_hash,
            approvers: Vec::new(&env),
            status: PaymentRequestStatus::Submitted,
            submitted_ledger: now,
            expires_at_ledger: now + ttl_ledgers,
            has_timelock_action: false,
            timelock_action_id: 0,
            eta_ledger: 0,
        };
        env.storage()
            .persistent()
            .set(&DataKey::PaymentRequest(next_id), &req);
        env.events()
            .publish((symbol_short!("pay_sub"), requester), next_id);
        Ok(next_id)
    }

    /// Approver reviews and approves a `Submitted` payment request.
    /// Role separation prevents the requester from approving their own request.
    pub fn approve_payment_request(
        env: Env,
        approver: Address,
        request_id: u64,
    ) -> Result<PaymentRequestStatus, TreasuryError> {
        approver.require_auth();
        let is_app: bool = env
            .storage()
            .persistent()
            .get(&DataKey::Approver(approver.clone()))
            .unwrap_or(false);
        if !is_app {
            return Err(TreasuryError::Unauthorized);
        }

        let mut req = Self::read_request(&env, request_id)?;
        if req.status != PaymentRequestStatus::Submitted {
            return Err(TreasuryError::InvalidStateTransition);
        }
        if env.ledger().sequence() > req.expires_at_ledger {
            req.status = PaymentRequestStatus::Expired;
            env.storage()
                .persistent()
                .set(&DataKey::PaymentRequest(request_id), &req);
            return Err(TreasuryError::RequestExpired);
        }
        if req.requester == approver {
            return Err(TreasuryError::SelfApprovalForbidden);
        }
        for existing in req.approvers.iter() {
            if existing == approver {
                return Err(TreasuryError::DuplicateApproval);
            }
        }

        req.approvers.push_back(approver.clone());
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ApprovalThreshold)
            .unwrap_or(1u32);
        if req.approvers.len() >= threshold {
            // Check per-asset budget before marking Approved
            let cap: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::AssetBudgetCap(req.asset.clone()))
                .unwrap_or(0);
            let spent: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::AssetSpent(req.asset.clone()))
                .unwrap_or(0);
            if spent + req.amount > cap {
                return Err(TreasuryError::BudgetExceeded);
            }
            req.status = PaymentRequestStatus::Approved;
        }

        env.storage()
            .persistent()
            .set(&DataKey::PaymentRequest(request_id), &req);
        env.events().publish(
            (symbol_short!("pay_app"), approver),
            (request_id, req.status),
        );
        Ok(req.status)
    }

    /// Queue an `Approved` payment request into the timelock execution queue.
    pub fn queue_payment_request(
        env: Env,
        caller: Address,
        request_id: u64,
        timelock_action_id: u64,
    ) -> Result<PaymentRequest, TreasuryError> {
        caller.require_auth();
        let is_app: bool = env
            .storage()
            .persistent()
            .get(&DataKey::Approver(caller.clone()))
            .unwrap_or(false);
        let admin = Self::read_admin(&env)?;
        if !is_app && caller != admin {
            return Err(TreasuryError::Unauthorized);
        }

        let mut req = Self::read_request(&env, request_id)?;
        if req.status != PaymentRequestStatus::Approved {
            return Err(TreasuryError::InvalidStateTransition);
        }
        if env.ledger().sequence() > req.expires_at_ledger {
            req.status = PaymentRequestStatus::Expired;
            env.storage()
                .persistent()
                .set(&DataKey::PaymentRequest(request_id), &req);
            return Err(TreasuryError::RequestExpired);
        }

        let cap: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::AssetBudgetCap(req.asset.clone()))
            .unwrap_or(0);
        let spent: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::AssetSpent(req.asset.clone()))
            .unwrap_or(0);
        if spent + req.amount > cap {
            return Err(TreasuryError::BudgetExceeded);
        }

        let delay: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TimelockDelay)
            .unwrap_or(5u32);
        req.status = PaymentRequestStatus::Queued;
        req.has_timelock_action = true;
        req.timelock_action_id = timelock_action_id;
        req.eta_ledger = env.ledger().sequence() + delay;

        env.storage()
            .persistent()
            .set(&DataKey::PaymentRequest(request_id), &req);
        env.events().publish(
            (symbol_short!("pay_que"), caller),
            (request_id, timelock_action_id, req.eta_ledger),
        );
        Ok(req)
    }

    /// Execute a `Queued` payment request once its timelock delay has elapsed.
    /// Role separation prevents the original requester from executing their own request.
    pub fn execute_queued_payment(
        env: Env,
        executor: Address,
        request_id: u64,
    ) -> Result<PaymentRequest, TreasuryError> {
        executor.require_auth();
        let mut req = Self::read_request(&env, request_id)?;
        if req.status != PaymentRequestStatus::Queued {
            return Err(TreasuryError::InvalidStateTransition);
        }
        if req.requester == executor {
            return Err(TreasuryError::SelfExecutionForbidden);
        }
        if env.ledger().sequence() < req.eta_ledger {
            return Err(TreasuryError::TimelockNotReady);
        }

        let cap: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::AssetBudgetCap(req.asset.clone()))
            .unwrap_or(0);
        let spent: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::AssetSpent(req.asset.clone()))
            .unwrap_or(0);
        if spent + req.amount > cap {
            return Err(TreasuryError::BudgetExceeded);
        }

        let token_client = token::Client::new(&env, &req.asset);
        let vault_bal = token_client.balance(&env.current_contract_address());
        if vault_bal < req.amount {
            return Err(TreasuryError::InsufficientVaultBalance);
        }

        token_client.transfer(&env.current_contract_address(), &req.payee, &req.amount);
        env.storage().persistent().set(
            &DataKey::AssetSpent(req.asset.clone()),
            &(spent + req.amount),
        );

        req.status = PaymentRequestStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::PaymentRequest(request_id), &req);
        env.events().publish(
            (symbol_short!("pay_exe"), executor),
            (request_id, req.amount),
        );
        Ok(req)
    }

    pub fn reject_payment_request(
        env: Env,
        caller: Address,
        request_id: u64,
    ) -> Result<PaymentRequest, TreasuryError> {
        caller.require_auth();
        let is_app: bool = env
            .storage()
            .persistent()
            .get(&DataKey::Approver(caller.clone()))
            .unwrap_or(false);
        let admin = Self::read_admin(&env)?;
        if !is_app && caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        let mut req = Self::read_request(&env, request_id)?;
        if req.status != PaymentRequestStatus::Submitted
            && req.status != PaymentRequestStatus::Approved
        {
            return Err(TreasuryError::InvalidStateTransition);
        }
        req.status = PaymentRequestStatus::Rejected;
        env.storage()
            .persistent()
            .set(&DataKey::PaymentRequest(request_id), &req);
        env.events()
            .publish((symbol_short!("pay_rej"), caller), request_id);
        Ok(req)
    }

    pub fn cancel_payment_request(
        env: Env,
        caller: Address,
        request_id: u64,
    ) -> Result<PaymentRequest, TreasuryError> {
        caller.require_auth();
        let mut req = Self::read_request(&env, request_id)?;
        let admin = Self::read_admin(&env)?;
        if caller != req.requester && caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if req.status == PaymentRequestStatus::Executed
            || req.status == PaymentRequestStatus::Rejected
            || req.status == PaymentRequestStatus::Cancelled
        {
            return Err(TreasuryError::InvalidStateTransition);
        }
        req.status = PaymentRequestStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::PaymentRequest(request_id), &req);
        env.events()
            .publish((symbol_short!("pay_can"), caller), request_id);
        Ok(req)
    }

    pub fn get_payment_request(env: Env, request_id: u64) -> Option<PaymentRequest> {
        env.storage()
            .persistent()
            .get(&DataKey::PaymentRequest(request_id))
    }

    pub fn list_open_requests(env: Env) -> Vec<PaymentRequest> {
        let max_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextRequestId)
            .unwrap_or(0u64);
        let mut out = Vec::new(&env);
        for id in 1..=max_id {
            if let Some(req) = env
                .storage()
                .persistent()
                .get::<_, PaymentRequest>(&DataKey::PaymentRequest(id))
            {
                if req.status == PaymentRequestStatus::Submitted
                    || req.status == PaymentRequestStatus::Approved
                    || req.status == PaymentRequestStatus::Queued
                {
                    out.push_back(req);
                }
            }
        }
        out
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        let admin = Self::read_admin(env)?;
        if *caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        Ok(())
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_request(env: &Env, request_id: u64) -> Result<PaymentRequest, TreasuryError> {
        env.storage()
            .persistent()
            .get(&DataKey::PaymentRequest(request_id))
            .ok_or(TreasuryError::RequestNotFound)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn payment_request_workflow_enforces_role_separation_budget_caps_and_timelock_execution() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 10;
        });

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let operator = Address::generate(&env);
        let approver1 = Address::generate(&env);
        let approver2 = Address::generate(&env);
        let payee = Address::generate(&env);

        client.initialize(&admin);
        client.set_operator(&admin, &operator, &true);
        client.set_approver(&admin, &operator, &true); // operator also has approver bit to test self-approval rejection
        client.set_approver(&admin, &approver1, &true);
        client.set_approver(&admin, &approver2, &true);
        client.set_workflow_params(&admin, &2, &5);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        StellarAssetClient::new(&env, &token_id).mint(&contract_id, &1_000);
        client.set_asset_budget(&admin, &token_id, &600);

        let purpose = BytesN::from_array(&env, &[0x42u8; 32]);
        let req_id =
            client.submit_payment_request(&operator, &payee, &token_id, &400, &purpose, &20);
        assert_eq!(req_id, 1);

        // Self-approval is strictly rejected
        assert_eq!(
            client.try_approve_payment_request(&operator, &req_id),
            Err(Ok(TreasuryError::SelfApprovalForbidden))
        );

        // First independent approval -> still Submitted (threshold = 2)
        assert_eq!(
            client.approve_payment_request(&approver1, &req_id),
            PaymentRequestStatus::Submitted
        );
        // Second independent approval -> transitions to Approved
        assert_eq!(
            client.approve_payment_request(&approver2, &req_id),
            PaymentRequestStatus::Approved
        );

        // Queue into timelock (eta = 10 + 5 = 15)
        let queued = client.queue_payment_request(&approver1, &req_id, &77);
        assert_eq!(queued.status, PaymentRequestStatus::Queued);
        assert_eq!(queued.eta_ledger, 15);

        // Early execution before eta fails; self-execution by requester also fails
        assert_eq!(
            client.try_execute_queued_payment(&approver1, &req_id),
            Err(Ok(TreasuryError::TimelockNotReady))
        );
        env.ledger().with_mut(|li| {
            li.sequence_number = 15;
        });
        assert_eq!(
            client.try_execute_queued_payment(&operator, &req_id),
            Err(Ok(TreasuryError::SelfExecutionForbidden))
        );

        // Independent executor executes after timelock ETA -> reconciles treasury balance
        let executed = client.execute_queued_payment(&approver1, &req_id);
        assert_eq!(executed.status, PaymentRequestStatus::Executed);
        assert_eq!(token_client.balance(&payee), 400);
        assert_eq!(token_client.balance(&contract_id), 600);

        // Second request for 300 exceeds remaining budget (600 - 400 = 200) -> rejected on 2nd approval
        let req2 = client.submit_payment_request(&operator, &payee, &token_id, &300, &purpose, &20);
        client.approve_payment_request(&approver1, &req2);
        assert_eq!(
            client.try_approve_payment_request(&approver2, &req2),
            Err(Ok(TreasuryError::BudgetExceeded))
        );
    }
}
