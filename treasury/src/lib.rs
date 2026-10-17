#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

const BPS_DENOM: i128 = 10_000;

/// Storage keys for the treasury contract, keeping operating and emergency
/// reserve namespaces strictly segregated.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Timelock,
    ReleaseDelayLedgers,
    ReserveBucket(Address),
    ReserveProposal(u64),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InvalidTargetRatio = 5,
    InsufficientOperatingBalance = 6,
    InsufficientReserveBalance = 7,
    ProposalNotFound = 8,
    TimelockDelayNotElapsed = 9,
    ProposalAlreadyExecuted = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveBucketState {
    pub balance: i128,
    pub declared_obligations: i128,
    pub target_ratio_bps: i128,
    pub last_release_amount: i128,
    pub last_release_purpose: Symbol,
    pub last_release_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveReleaseProposal {
    pub proposal_id: u64,
    pub token: Address,
    pub to: Address,
    pub amount: i128,
    pub purpose: Symbol,
    pub unlock_ledger: u32,
    pub executed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveStatusView {
    pub reserve_balance: i128,
    pub operating_balance: i128,
    pub total_custody_balance: i128,
    pub target_amount: i128,
    pub shortfall: i128,
    pub surplus: i128,
    pub last_release_amount: i128,
    pub last_release_purpose: Symbol,
    pub last_release_ledger: u32,
}

/// `RefractTreasury` is a token-custody vault with a segregated emergency
/// reserve bucket that cannot be touched by operating withdrawals.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an operating admin.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Timelock, &admin);
        env.storage()
            .instance()
            .set(&DataKey::ReleaseDelayLedgers, &10u32);
        Ok(())
    }

    /// Configure the timelock authority and required delay (in ledgers) for emergency reserve releases.
    pub fn configure_timelock(
        env: Env,
        caller: Address,
        timelock: Address,
        release_delay_ledgers: u32,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Timelock, &timelock);
        env.storage()
            .instance()
            .set(&DataKey::ReleaseDelayLedgers, &release_delay_ledgers);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    /// Deposit `amount` of `token` directly into the segregated emergency reserve bucket.
    pub fn deposit_reserve(
        env: Env,
        from: Address,
        token: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        from.require_auth();
        let _ = Self::read_admin(&env)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);

        let mut bucket = Self::read_bucket(&env, &token);
        bucket.balance += amount;
        env.storage()
            .persistent()
            .set(&DataKey::ReserveBucket(token.clone()), &bucket);

        env.events().publish(
            (symbol_short!("reserve"), symbol_short!("deposit")),
            (token, from, amount, bucket.balance),
        );

        Ok(bucket.balance)
    }

    /// Set the target reserve ratio (in basis points) and declared obligations via the timelock authority.
    pub fn set_reserve_target(
        env: Env,
        caller: Address,
        token: Address,
        declared_obligations: i128,
        target_ratio_bps: i128,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        let timelock = Self::read_timelock(&env)?;
        if caller != timelock {
            return Err(TreasuryError::Unauthorized);
        }
        if declared_obligations < 0 || !(0..=BPS_DENOM).contains(&target_ratio_bps) {
            return Err(TreasuryError::InvalidTargetRatio);
        }

        let mut bucket = Self::read_bucket(&env, &token);
        bucket.declared_obligations = declared_obligations;
        bucket.target_ratio_bps = target_ratio_bps;
        env.storage()
            .persistent()
            .set(&DataKey::ReserveBucket(token.clone()), &bucket);

        let target_amount = declared_obligations * target_ratio_bps / BPS_DENOM;
        env.events().publish(
            (symbol_short!("reserve"), symbol_short!("target")),
            (token, declared_obligations, target_ratio_bps, target_amount),
        );

        Ok(target_amount)
    }

    /// Withdraw `amount` of `token` from the operating balance only.
    /// Segregated emergency reserve funds are strictly protected and cannot be drawn here.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let client = token::Client::new(&env, &token);
        let total_custody = client.balance(&env.current_contract_address());
        let bucket = Self::read_bucket(&env, &token);
        let operating_balance = total_custody - bucket.balance;

        if amount > operating_balance {
            return Err(TreasuryError::InsufficientOperatingBalance);
        }

        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Queue a timelocked governance proposal to release funds from the emergency reserve with a purpose tag.
    pub fn queue_reserve_release(
        env: Env,
        caller: Address,
        proposal_id: u64,
        token: Address,
        to: Address,
        amount: i128,
        purpose: Symbol,
    ) -> Result<u32, TreasuryError> {
        caller.require_auth();
        let timelock = Self::read_timelock(&env)?;
        if caller != timelock {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let bucket = Self::read_bucket(&env, &token);
        if amount > bucket.balance {
            env.events().publish(
                (symbol_short!("reserve"), symbol_short!("rejected")),
                (proposal_id, token, amount, bucket.balance),
            );
            return Err(TreasuryError::InsufficientReserveBalance);
        }

        let delay: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ReleaseDelayLedgers)
            .unwrap_or(0);
        let unlock_ledger = env.ledger().sequence() + delay;

        let proposal = ReserveReleaseProposal {
            proposal_id,
            token: token.clone(),
            to,
            amount,
            purpose: purpose.clone(),
            unlock_ledger,
            executed: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ReserveProposal(proposal_id), &proposal);

        env.events().publish(
            (symbol_short!("reserve"), symbol_short!("queued")),
            (proposal_id, token, amount, purpose, unlock_ledger),
        );

        Ok(unlock_ledger)
    }

    /// Execute a queued emergency reserve release after the timelock delay has elapsed.
    pub fn execute_reserve_release(
        env: Env,
        caller: Address,
        proposal_id: u64,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        let timelock = Self::read_timelock(&env)?;
        if caller != timelock {
            return Err(TreasuryError::Unauthorized);
        }

        let mut proposal: ReserveReleaseProposal = env
            .storage()
            .persistent()
            .get(&DataKey::ReserveProposal(proposal_id))
            .ok_or(TreasuryError::ProposalNotFound)?;

        if proposal.executed {
            return Err(TreasuryError::ProposalAlreadyExecuted);
        }
        let current_ledger = env.ledger().sequence();
        if current_ledger < proposal.unlock_ledger {
            env.events().publish(
                (symbol_short!("reserve"), symbol_short!("rejected")),
                (proposal_id, current_ledger, proposal.unlock_ledger),
            );
            return Err(TreasuryError::TimelockDelayNotElapsed);
        }

        let mut bucket = Self::read_bucket(&env, &proposal.token);
        if proposal.amount > bucket.balance {
            env.events().publish(
                (symbol_short!("reserve"), symbol_short!("rejected")),
                (proposal_id, proposal.amount, bucket.balance),
            );
            return Err(TreasuryError::InsufficientReserveBalance);
        }

        bucket.balance -= proposal.amount;
        bucket.last_release_amount = proposal.amount;
        bucket.last_release_purpose = proposal.purpose.clone();
        bucket.last_release_ledger = current_ledger;
        env.storage()
            .persistent()
            .set(&DataKey::ReserveBucket(proposal.token.clone()), &bucket);

        proposal.executed = true;
        env.storage()
            .persistent()
            .set(&DataKey::ReserveProposal(proposal_id), &proposal);

        token::Client::new(&env, &proposal.token).transfer(
            &env.current_contract_address(),
            &proposal.to,
            &proposal.amount,
        );

        env.events().publish(
            (symbol_short!("reserve"), symbol_short!("released")),
            (
                proposal_id,
                proposal.token,
                proposal.to,
                proposal.amount,
                proposal.purpose,
            ),
        );

        Ok(proposal.amount)
    }

    /// Read-only view reporting reserve balance, operating balance, target, shortfall/surplus, and last release.
    pub fn reserve_status(env: Env, token: Address) -> Result<ReserveStatusView, TreasuryError> {
        let _ = Self::read_admin(&env)?;
        let total_custody =
            token::Client::new(&env, &token).balance(&env.current_contract_address());
        let bucket = Self::read_bucket(&env, &token);
        let operating_balance = total_custody - bucket.balance;
        let target_amount = bucket.declared_obligations * bucket.target_ratio_bps / BPS_DENOM;
        let shortfall = if target_amount > bucket.balance {
            target_amount - bucket.balance
        } else {
            0
        };
        let surplus = if bucket.balance > target_amount {
            bucket.balance - target_amount
        } else {
            0
        };

        Ok(ReserveStatusView {
            reserve_balance: bucket.balance,
            operating_balance,
            total_custody_balance: total_custody,
            target_amount,
            shortfall,
            surplus,
            last_release_amount: bucket.last_release_amount,
            last_release_purpose: bucket.last_release_purpose,
            last_release_ledger: bucket.last_release_ledger,
        })
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_timelock(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Timelock)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_bucket(env: &Env, token: &Address) -> ReserveBucketState {
        env.storage()
            .persistent()
            .get(&DataKey::ReserveBucket(token.clone()))
            .unwrap_or(ReserveBucketState {
                balance: 0,
                declared_obligations: 0,
                target_ratio_bps: 0,
                last_release_amount: 0,
                last_release_purpose: symbol_short!("none"),
                last_release_ledger: 0,
            })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn segregated_emergency_reserve_protects_balance_and_releases_via_timelock() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let timelock = Address::generate(&env);
        client.initialize(&admin);
        client.configure_timelock(&admin, &timelock, &20);

        let token_admin = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin);
        let token_id = token_contract.address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Operating custody deposit (600) + emergency reserve deposit (400) = 1000 total custody
        sac.mint(&contract_id, &600);
        let depositor = Address::generate(&env);
        sac.mint(&depositor, &400);
        client.deposit_reserve(&depositor, &token_id, &400);

        // Configure reserve target: 25% (2500 bps) of 2000 obligations = 500 target (100 shortfall)
        let target = client.set_reserve_target(&timelock, &token_id, &2_000, &2_500);
        assert_eq!(target, 500);

        let status = client.reserve_status(&token_id);
        assert_eq!(status.reserve_balance, 400);
        assert_eq!(status.operating_balance, 600);
        assert_eq!(
            status.reserve_balance + status.operating_balance,
            status.total_custody_balance
        );
        assert_eq!(status.shortfall, 100);
        assert_eq!(status.surplus, 0);

        // Operating withdrawal cannot touch segregated reserve funds (trying to withdraw 700 > 600 operating)
        let vendor = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&admin, &token_id, &700, &vendor),
            Err(Ok(TreasuryError::InsufficientOperatingBalance))
        );

        // Operating withdrawal within operating balance succeeds
        client.withdraw(&admin, &token_id, &250, &vendor);
        assert_eq!(token_client.balance(&vendor), 250);

        // Over-sized reserve release fails atomically
        let recipient = Address::generate(&env);
        assert_eq!(
            client.try_queue_reserve_release(
                &timelock,
                &1,
                &token_id,
                &recipient,
                &500,
                &symbol_short!("backstop")
            ),
            Err(Ok(TreasuryError::InsufficientReserveBalance))
        );

        // Queue valid reserve release of 150 and verify timelock delay enforcement
        env.ledger().with_mut(|li| li.sequence_number = 100);
        let unlock = client.queue_reserve_release(
            &timelock,
            &1,
            &token_id,
            &recipient,
            &150,
            &symbol_short!("backstop"),
        );
        assert_eq!(unlock, 120);

        assert_eq!(
            client.try_execute_reserve_release(&timelock, &1),
            Err(Ok(TreasuryError::TimelockDelayNotElapsed))
        );

        env.ledger().with_mut(|li| li.sequence_number = 120);
        let released = client.execute_reserve_release(&timelock, &1);
        assert_eq!(released, 150);
        assert_eq!(token_client.balance(&recipient), 150);

        let final_status = client.reserve_status(&token_id);
        assert_eq!(final_status.reserve_balance, 250);
        assert_eq!(final_status.operating_balance, 350);
        assert_eq!(
            final_status.reserve_balance + final_status.operating_balance,
            final_status.total_custody_balance
        );
        assert_eq!(final_status.last_release_amount, 150);
        assert_eq!(final_status.last_release_purpose, symbol_short!("backstop"));
    }
}
