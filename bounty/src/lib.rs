#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

/// Lifecycle status of a pull-based escrow payout record.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PayoutStatus {
    /// Funds are reserved in escrow awaiting recipient `claim`.
    Pending = 0,
    /// Funds have been claimed and transferred to the recipient.
    Claimed = 1,
    /// Claim deadline passed and reserved funds were swept back to available escrow.
    Swept = 2,
}

/// Per-recipient pull-based payout escrow record.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutRecord {
    pub payout_id: u64,
    pub recipient: Address,
    pub asset: Address,
    pub amount: i128,
    pub claim_deadline: u64,
    pub report_hash: BytesN<32>,
    pub status: PayoutStatus,
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
    /// Running total of funds currently reserved for pending pull-based payouts.
    ReservedBalance,
    /// Monotonic payout identifier counter.
    NextPayoutId,
    /// Per-recipient pull-based payout record.
    Payout(u64),
    /// Reentrancy lock for claim/sweep paths.
    ReentrancyLock,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
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
    PayoutNotFound = 7,
    NotRecipient = 8,
    AlreadyClaimed = 9,
    AlreadySwept = 10,
    DeadlineNotReached = 11,
    InvalidDeadline = 12,
    ReentrancyDetected = 13,
}

/// Governance-controlled bug bounty escrow supporting pull-based per-recipient
/// payout records (`create_payout`, `claim`, `sweep_expired`).
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
            .set(&DataKey::ReservedBalance, &0i128);
        env.storage().instance().set(&DataKey::NextPayoutId, &0u64);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Create an escrowed pull-based payout record and reserve funds from the
    /// available balance. Governance/timelock-gated. Emits `payout_created`.
    pub fn create_payout(
        env: Env,
        caller: Address,
        recipient: Address,
        amount: i128,
        claim_deadline: u64,
        report_hash: BytesN<32>,
    ) -> Result<u64, BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let now = env.ledger().timestamp();
        if claim_deadline <= now {
            return Err(BountyError::InvalidDeadline);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        let avail = Self::read_available_balance(&env)?;
        if amount > avail {
            return Err(BountyError::InsufficientBalance);
        }

        let reserved = Self::read_reserved_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::ReservedBalance, &(reserved + amount));

        let payout_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextPayoutId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextPayoutId, &(payout_id + 1));

        let asset = Self::read_token(&env)?;
        let record = PayoutRecord {
            payout_id,
            recipient: recipient.clone(),
            asset: asset.clone(),
            amount,
            claim_deadline,
            report_hash: report_hash.clone(),
            status: PayoutStatus::Pending,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

        let award = AwardRecord {
            researcher: recipient.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: now,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &award);

        env.events().publish(
            (symbol_short!("pay_creat"), payout_id),
            (recipient, asset, amount, claim_deadline),
        );

        Ok(payout_id)
    }

    /// Pull-based claim callable only by the recorded recipient.
    /// Allowed after `claim_deadline` until `sweep_expired` is executed; once
    /// swept, `AlreadySwept` is returned. Emits `payout_claimed`.
    pub fn claim(env: Env, caller: Address, payout_id: u64) -> Result<i128, BountyError> {
        caller.require_auth();
        Self::enter_non_reentrant(&env)?;

        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
            .ok_or(BountyError::PayoutNotFound)?;

        if caller != record.recipient {
            Self::exit_non_reentrant(&env);
            return Err(BountyError::NotRecipient);
        }
        match record.status {
            PayoutStatus::Claimed => {
                Self::exit_non_reentrant(&env);
                return Err(BountyError::AlreadyClaimed);
            }
            PayoutStatus::Swept => {
                Self::exit_non_reentrant(&env);
                return Err(BountyError::AlreadySwept);
            }
            PayoutStatus::Pending => {}
        }

        // Update state before external token transfer (Checks-Effects-Interactions).
        // If the token transfer panics/fails in Soroban, the entire host invocation
        // rolls back, leaving the payout record Pending and intact.
        record.status = PayoutStatus::Claimed;
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

        let reserved = Self::read_reserved_balance(&env)?;
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::ReservedBalance, &(reserved - record.amount));
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - record.amount));

        token::Client::new(&env, &record.asset).transfer(
            &env.current_contract_address(),
            &record.recipient,
            &record.amount,
        );

        env.events().publish(
            (symbol_short!("pay_claim"), payout_id),
            (record.recipient, record.asset, record.amount),
        );

        Self::exit_non_reentrant(&env);
        Ok(record.amount)
    }

    /// Permissionless, idempotent sweep of an expired unclaimed payout after
    /// `claim_deadline`, returning reserved funds to the available pool balance.
    /// Emits `payout_expired_swept` on transition from `Pending` to `Swept`.
    pub fn sweep_expired(env: Env, payout_id: u64) -> Result<i128, BountyError> {
        Self::enter_non_reentrant(&env)?;

        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
            .ok_or(BountyError::PayoutNotFound)?;

        match record.status {
            PayoutStatus::Swept => {
                // Idempotent no-op when already swept.
                Self::exit_non_reentrant(&env);
                return Ok(0);
            }
            PayoutStatus::Claimed => {
                Self::exit_non_reentrant(&env);
                return Err(BountyError::AlreadyClaimed);
            }
            PayoutStatus::Pending => {}
        }

        if env.ledger().timestamp() <= record.claim_deadline {
            Self::exit_non_reentrant(&env);
            return Err(BountyError::DeadlineNotReached);
        }

        record.status = PayoutStatus::Swept;
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

        let reserved = Self::read_reserved_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::ReservedBalance, &(reserved - record.amount));

        env.events().publish(
            (symbol_short!("pay_swept"), payout_id),
            (record.recipient, record.asset, record.amount),
        );

        Self::exit_non_reentrant(&env);
        Ok(record.amount)
    }

    /// Legacy push award entrypoint preserved for backward compatibility;
    /// checks `available_balance` so reserved pull payouts cannot be double-spent.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
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
        let avail = Self::read_available_balance(&env)?;
        if amount > avail {
            return Err(BountyError::InsufficientBalance);
        }

        let balance = Self::read_balance(&env)?;
        let token = Self::read_token(&env)?;
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

    /// Total funds held in escrow (including reserved funds).
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// Total funds reserved for pending pull-based payouts.
    pub fn reserved_balance(env: Env) -> Result<i128, BountyError> {
        Self::read_reserved_balance(&env)
    }

    /// Uncommitted funds available for new payouts (`balance - reserved_balance`).
    pub fn available_balance(env: Env) -> Result<i128, BountyError> {
        Self::read_available_balance(&env)
    }

    /// Read a payout record by id.
    pub fn get_payout(env: Env, payout_id: u64) -> Option<PayoutRecord> {
        env.storage().persistent().get(&DataKey::Payout(payout_id))
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn enter_non_reentrant(env: &Env) -> Result<(), BountyError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::ReentrancyLock)
            .unwrap_or(false)
        {
            return Err(BountyError::ReentrancyDetected);
        }
        env.storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &true);
        Ok(())
    }

    fn exit_non_reentrant(env: &Env) {
        env.storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &false);
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_reserved_balance(env: &Env) -> Result<i128, BountyError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::NotInitialized);
        }
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::ReservedBalance)
            .unwrap_or(0))
    }

    fn read_available_balance(env: &Env) -> Result<i128, BountyError> {
        let bal = Self::read_balance(env)?;
        let res = Self::read_reserved_balance(env)?;
        Ok(bal - res)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    #[test]
    fn pull_payout_lifecycle_claim_sweep_and_reservation_accounting() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.timestamp = 1_000);

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher1 = Address::generate(&env);
        let researcher2 = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        sac.mint(&funder, &1_000);
        client.fund(&funder, &1_000);
        assert_eq!(client.balance(), 1_000);
        assert_eq!(client.available_balance(), 1_000);

        let hash1 = BytesN::from_array(&env, &[1u8; 32]);
        let payout_1 = client.create_payout(&governance, &researcher1, &600, &2_000, &hash1);
        assert_eq!(payout_1, 0);
        assert_eq!(client.balance(), 1_000);
        assert_eq!(client.reserved_balance(), 600);
        assert_eq!(client.available_balance(), 400);

        // Cannot over-commit reserved funds.
        let hash_over = BytesN::from_array(&env, &[9u8; 32]);
        assert!(client
            .try_create_payout(&governance, &researcher2, &500, &2_000, &hash_over)
            .is_err());

        // Non-recipient cannot claim.
        assert_eq!(
            client.try_claim(&stranger, &payout_1),
            Err(Ok(BountyError::NotRecipient))
        );

        // Sweep before deadline is rejected.
        assert_eq!(
            client.try_sweep_expired(&payout_1),
            Err(Ok(BountyError::DeadlineNotReached))
        );

        // Recipient claims payout_1.
        assert_eq!(client.claim(&researcher1, &payout_1), 600);
        assert_eq!(token_client.balance(&researcher1), 600);
        assert_eq!(client.balance(), 400);
        assert_eq!(client.reserved_balance(), 0);
        assert_eq!(client.available_balance(), 400);

        // Double-claim is rejected.
        assert_eq!(
            client.try_claim(&researcher1, &payout_1),
            Err(Ok(BountyError::AlreadyClaimed))
        );

        // Create second payout and let it expire, then sweep.
        let hash2 = BytesN::from_array(&env, &[2u8; 32]);
        let payout_2 = client.create_payout(&governance, &researcher2, &300, &2_000, &hash2);
        assert_eq!(client.available_balance(), 100);

        env.ledger().with_mut(|li| li.timestamp = 2_001);
        assert_eq!(client.sweep_expired(&payout_2), 300);
        assert_eq!(client.available_balance(), 400);
        assert_eq!(client.reserved_balance(), 0);

        // Repeating sweep_expired is idempotent (returns 0).
        assert_eq!(client.sweep_expired(&payout_2), 0);

        // Claiming after sweep is rejected with AlreadySwept.
        assert_eq!(
            client.try_claim(&researcher2, &payout_2),
            Err(Ok(BountyError::AlreadySwept))
        );
    }

    #[test]
    fn reentrancy_lock_blocks_nested_claim_or_sweep() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.timestamp = 100);

        let governance = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        sac.mint(&governance, &500);
        client.fund(&governance, &500);
        let hash = BytesN::from_array(&env, &[7u8; 32]);
        let pid = client.create_payout(&governance, &recipient, &200, &500, &hash);

        env.as_contract(&contract_id, || {
            env.storage()
                .instance()
                .set(&DataKey::ReentrancyLock, &true);
        });

        assert_eq!(
            client.try_claim(&recipient, &pid),
            Err(Ok(BountyError::ReentrancyDetected))
        );
        assert_eq!(
            client.try_sweep_expired(&pid),
            Err(Ok(BountyError::ReentrancyDetected))
        );
    }
}
