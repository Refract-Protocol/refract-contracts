#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

extern crate alloc;

use alloc::vec::Vec as StdVec;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Vec,
};

/// Largest-remainder integer allocation over `weights`, guaranteeing
/// `sum(outputs) == total` (with deterministic tie-breaking by lowest index).
pub fn allocate_split_largest_remainder(
    total: i128,
    weights: &[i128],
) -> Result<StdVec<i128>, BountyError> {
    if total <= 0 || weights.is_empty() {
        return Err(BountyError::InvalidAmount);
    }
    let mut weight_sum: u128 = 0;
    for &w in weights {
        if w < 0 {
            return Err(BountyError::InvalidAmount);
        }
        weight_sum = weight_sum
            .checked_add(w as u128)
            .ok_or(BountyError::InvalidAmount)?;
    }
    if weight_sum == 0 {
        return Err(BountyError::InvalidAmount);
    }

    let total_u = total as u128;
    let mut outputs = alloc::vec![0i128; weights.len()];
    let mut remainders: StdVec<(u128, usize)> = StdVec::with_capacity(weights.len());
    let mut floor_sum: i128 = 0;

    for (idx, &w) in weights.iter().enumerate() {
        let prod = total_u
            .checked_mul(w as u128)
            .ok_or(BountyError::InvalidAmount)?;
        let q = (prod / weight_sum) as i128;
        let rem = prod % weight_sum;
        outputs[idx] = q;
        floor_sum += q;
        remainders.push((rem, idx));
    }

    remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut residual = total - floor_sum;
    for (_, idx) in remainders {
        if residual <= 0 {
            break;
        }
        outputs[idx] += 1;
        residual -= 1;
    }

    debug_assert_eq!(outputs.iter().copied().sum::<i128>(), total);
    Ok(outputs)
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
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
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
}

/// Governance-controlled bug bounty escrow with deterministic largest-remainder
/// multi-recipient bounty splits (`award_split`).
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

    /// Award a single bounty to a security researcher.
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
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

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

    /// Split a bounty award of `total_amount` across multiple co-reporters
    /// according to `weights` using largest-remainder allocation so that
    /// `sum(payouts) == total_amount` with zero stranded dust.
    pub fn award_split(
        env: Env,
        caller: Address,
        researchers: Vec<Address>,
        weights: Vec<i128>,
        total_amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<Vec<i128>, BountyError> {
        caller.require_auth();
        if caller != Self::read_governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if researchers.is_empty() || researchers.len() != weights.len() {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if total_amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let mut w_slice = StdVec::with_capacity(weights.len() as usize);
        for w in weights.iter() {
            w_slice.push(w);
        }
        let payouts = allocate_split_largest_remainder(total_amount, &w_slice)?;

        let token = Self::read_token(&env)?;
        let token_client = token::Client::new(&env, &token);
        let mut out = Vec::new(&env);
        for (idx, amount) in payouts.into_iter().enumerate() {
            let r = researchers.get(idx as u32).unwrap();
            if amount > 0 {
                token_client.transfer(&env.current_contract_address(), &r, &amount);
            }
            out.push_back(amount);
        }

        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - total_amount));
        let lead = researchers.get(0).unwrap();
        env.storage().persistent().set(
            &DataKey::Award(report_hash.clone()),
            &AwardRecord {
                researcher: lead,
                amount: total_amount,
                report_hash,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(out)
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn multi_recipient_award_split_conserves_exact_total() {
        let env = Env::default();
        env.mock_all_auths();
        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        let r3 = Address::generate(&env);
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

        let mut researchers = Vec::new(&env);
        researchers.push_back(r1.clone());
        researchers.push_back(r2.clone());
        researchers.push_back(r3.clone());

        let mut weights = Vec::new(&env);
        weights.push_back(1);
        weights.push_back(1);
        weights.push_back(1);

        let hash = BytesN::from_array(&env, &[3u8; 32]);
        let split = client.award_split(&governance, &researchers, &weights, &1_000, &hash);
        assert_eq!(split.get(0), Some(334));
        assert_eq!(split.get(1), Some(333));
        assert_eq!(split.get(2), Some(333));
        assert_eq!(
            token_client.balance(&r1) + token_client.balance(&r2) + token_client.balance(&r3),
            1_000
        );
        assert_eq!(client.balance(), 0);
    }
}
