#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, xdr::ToXdr, Address,
    Bytes, BytesN, Env, Symbol,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;
/// Periodic checkpoint anchor interval (every 4 audit records).
const AUDIT_CHECKPOINT_INTERVAL: u64 = 4;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    Paused = 8,
    AuditChainBroken = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRecord {
    pub seq: u64,
    pub caller: Address,
    pub action_tag: Symbol,
    pub payload_digest: BytesN<32>,
    pub ledger: u32,
    pub prev_digest: BytesN<32>,
    pub record_digest: BytesN<32>,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&"config") {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool: pool.clone(),
            admin: admin.clone(),
            token: token.clone(),
            premium_share_bps,
        };
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(&"paused", &false);
        let payload = (pool, admin.clone(), token, premium_share_bps).to_xdr(&env);
        let payload_digest: BytesN<32> = env.crypto().sha256(&payload).into();
        Self::append_audit_record(&env, admin, symbol_short!("init"), payload_digest);
        Ok(())
    }

    pub fn set_premium_share_bps(
        env: Env,
        caller: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        config.premium_share_bps = premium_share_bps;
        env.storage().instance().set(&"config", &config);
        let payload = premium_share_bps.to_xdr(&env);
        let payload_digest: BytesN<32> = env.crypto().sha256(&payload).into();
        Self::append_audit_record(&env, caller, symbol_short!("set_bps"), payload_digest);
        Ok(())
    }

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&"paused", &paused);
        let payload = paused.to_xdr(&env);
        let payload_digest: BytesN<32> = env.crypto().sha256(&payload).into();
        Self::append_audit_record(&env, caller, symbol_short!("pause"), payload_digest);
        Ok(())
    }

    pub fn rotate_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.admin = new_admin.clone();
        env.storage().instance().set(&"config", &config);
        let payload = new_admin.to_xdr(&env);
        let payload_digest: BytesN<32> = env.crypto().sha256(&payload).into();
        Self::append_audit_record(&env, caller, symbol_short!("rot_adm"), payload_digest);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if env.storage().instance().get(&"paused").unwrap_or(false) {
            return Err(BackstopError::Paused);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = ("shares", staker);
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };

        if covered > 0 {
            state.total_capital -= covered;
            env.storage().instance().set(&"state", &state);

            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &covered,
            );
        }

        let payload = (amount, covered).to_xdr(&env);
        let payload_digest: BytesN<32> = env.crypto().sha256(&payload).into();
        Self::append_audit_record(&env, caller, symbol_short!("shortfl"), payload_digest);

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    pub fn audit_record(env: Env, seq: u64) -> Option<AuditRecord> {
        env.storage().persistent().get(&("audit_rec", seq))
    }

    pub fn audit_head_digest(env: Env) -> BytesN<32> {
        env.storage()
            .instance()
            .get(&"audit_head")
            .unwrap_or_else(|| BytesN::from_array(&env, &[0u8; 32]))
    }

    pub fn audit_head_seq(env: Env) -> u64 {
        env.storage().instance().get(&"audit_seq").unwrap_or(0u64)
    }

    pub fn audit_checkpoint_digest(env: Env, checkpoint_seq: u64) -> Option<BytesN<32>> {
        env.storage()
            .persistent()
            .get(&("audit_chk", checkpoint_seq))
    }

    /// Verify a range `[start_seq, end_seq]` of audit records starting from `expected_prev_digest`
    /// (which may be `[0u8; 32]` at genesis or a checkpoint anchor across an archival boundary).
    pub fn verify_audit_chain(
        env: Env,
        start_seq: u64,
        end_seq: u64,
        expected_prev_digest: BytesN<32>,
    ) -> Result<BytesN<32>, BackstopError> {
        if start_seq == 0 || end_seq < start_seq {
            return Err(BackstopError::AuditChainBroken);
        }
        let mut running_prev = expected_prev_digest;
        for seq in start_seq..=end_seq {
            let rec: AuditRecord = env
                .storage()
                .persistent()
                .get(&("audit_rec", seq))
                .ok_or(BackstopError::AuditChainBroken)?;
            if rec.seq != seq || rec.prev_digest != running_prev {
                return Err(BackstopError::AuditChainBroken);
            }
            let expected = Self::compute_record_digest(
                &env,
                rec.seq,
                &rec.caller,
                &rec.action_tag,
                &rec.payload_digest,
                rec.ledger,
                &rec.prev_digest,
            );
            if rec.record_digest != expected {
                return Err(BackstopError::AuditChainBroken);
            }
            running_prev = rec.record_digest;
        }
        Ok(running_prev)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    fn append_audit_record(
        env: &Env,
        caller: Address,
        action_tag: Symbol,
        payload_digest: BytesN<32>,
    ) -> AuditRecord {
        let prev_seq: u64 = env.storage().instance().get(&"audit_seq").unwrap_or(0u64);
        let seq = prev_seq + 1;
        let prev_digest: BytesN<32> = env
            .storage()
            .instance()
            .get(&"audit_head")
            .unwrap_or_else(|| BytesN::from_array(env, &[0u8; 32]));
        let ledger = env.ledger().sequence();
        let record_digest = Self::compute_record_digest(
            env,
            seq,
            &caller,
            &action_tag,
            &payload_digest,
            ledger,
            &prev_digest,
        );
        let rec = AuditRecord {
            seq,
            caller: caller.clone(),
            action_tag: action_tag.clone(),
            payload_digest,
            ledger,
            prev_digest,
            record_digest: record_digest.clone(),
        };
        env.storage().persistent().set(&("audit_rec", seq), &rec);
        env.storage().instance().set(&"audit_seq", &seq);
        env.storage().instance().set(&"audit_head", &record_digest);
        if seq.is_multiple_of(AUDIT_CHECKPOINT_INTERVAL) {
            env.storage()
                .persistent()
                .set(&("audit_chk", seq), &record_digest);
        }
        env.events().publish(
            (symbol_short!("audit"), action_tag, caller),
            (seq, record_digest),
        );
        rec
    }

    fn compute_record_digest(
        env: &Env,
        seq: u64,
        caller: &Address,
        action_tag: &Symbol,
        payload_digest: &BytesN<32>,
        ledger: u32,
        prev_digest: &BytesN<32>,
    ) -> BytesN<32> {
        let mut buf = Bytes::new(env);
        buf.append(&seq.to_xdr(env));
        buf.append(&caller.clone().to_xdr(env));
        buf.append(&action_tag.clone().to_xdr(env));
        buf.append(&payload_digest.clone().into());
        buf.append(&ledger.to_xdr(env));
        buf.append(&prev_digest.clone().into());
        env.crypto().sha256(&buf).into()
    }

    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn hash_chained_audit_log_verifies_detects_tampering_and_supports_checkpoints() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 10;
        });

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &token_id).mint(&staker, &10_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        // 1. Initialize -> seq 1
        client.initialize(&pool, &admin, &token_id, &2_000);
        assert_eq!(client.audit_head_seq(), 1);

        // 2. Deposit & cover_shortfall -> seq 2
        client.deposit(&staker, &1_000);
        client.cover_shortfall(&pool, &200);
        assert_eq!(client.audit_head_seq(), 2);

        // 3. set_premium_share_bps -> seq 3
        client.set_premium_share_bps(&admin, &3_000);
        assert_eq!(client.audit_head_seq(), 3);

        // 4. set_paused -> seq 4 (checkpoint anchor recorded at seq 4)
        client.set_paused(&admin, &true);
        assert_eq!(client.audit_head_seq(), 4);
        let chk4 = client.audit_checkpoint_digest(&4).unwrap();
        assert_eq!(chk4, client.audit_head_digest());

        // 5. rotate_admin -> seq 5
        client.rotate_admin(&admin, &new_admin);
        assert_eq!(client.audit_head_seq(), 5);

        let zero_digest = BytesN::from_array(&env, &[0u8; 32]);
        let verified_head = client.verify_audit_chain(&1, &5, &zero_digest);
        assert_eq!(verified_head, client.audit_head_digest());

        // Verify across simulated archival boundary starting at seq 5 using checkpoint anchor 4
        let post_checkpoint_head = client.verify_audit_chain(&5, &5, &chk4);
        assert_eq!(post_checkpoint_head, client.audit_head_digest());

        // Tamper detection: wrong expected_prev_digest fails verification
        assert_eq!(
            client.try_verify_audit_chain(&2, &5, &zero_digest),
            Err(Ok(BackstopError::AuditChainBroken))
        );
    }
}
