#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

extern crate alloc;

use alloc::vec::Vec as StdVec;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AllocationError {
    NegativeTotal = 1,
    NegativeWeight = 2,
    EmptyWeights = 3,
    ArithmeticOverflow = 4,
}

/// Deterministic largest-remainder integer allocation over `weights`.
///
/// Guarantees exact conservation:
/// - If `sum(weights) > 0`: `sum(allocations) == total` and `dust == 0`.
///   Remainders `(total * w_i) % sum(weights)` are distributed 1 unit at a time
///   in descending remainder order with deterministic tie-breaking by lowest
///   recipient index.
/// - If `sum(weights) == 0`: every output is `0` and `dust == total` so the
///   caller can route `dust` to the protocol dust account in `RefractTreasury`.
pub fn allocate_largest_remainder(
    total: i128,
    weights: &[i128],
) -> Result<(StdVec<i128>, i128), AllocationError> {
    if total < 0 {
        return Err(AllocationError::NegativeTotal);
    }
    if weights.is_empty() {
        return Err(AllocationError::EmptyWeights);
    }

    let mut weight_sum: u128 = 0;
    for &w in weights {
        if w < 0 {
            return Err(AllocationError::NegativeWeight);
        }
        weight_sum = weight_sum
            .checked_add(w as u128)
            .ok_or(AllocationError::ArithmeticOverflow)?;
    }

    if total == 0 {
        return Ok((alloc::vec![0; weights.len()], 0));
    }
    if weight_sum == 0 {
        return Ok((alloc::vec![0; weights.len()], total));
    }

    let total_u = total as u128;
    let mut outputs = alloc::vec![0i128; weights.len()];
    let mut remainders: StdVec<(u128, usize)> = StdVec::with_capacity(weights.len());
    let mut floor_sum: i128 = 0;

    for (idx, &w) in weights.iter().enumerate() {
        let prod = total_u
            .checked_mul(w as u128)
            .ok_or(AllocationError::ArithmeticOverflow)?;
        let q = (prod / weight_sum) as i128;
        let rem = prod % weight_sum;
        outputs[idx] = q;
        floor_sum = floor_sum
            .checked_add(q)
            .ok_or(AllocationError::ArithmeticOverflow)?;
        remainders.push((rem, idx));
    }

    // Sort by descending remainder, then ascending index for deterministic tie-breaking.
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
    Ok((outputs, 0))
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    /// Protocol dust account balance per token asset.
    DustAccount(Address),
}

/// `RefractTreasury` custodies protocol funds and maintains a dedicated protocol
/// dust account (`route_dust`, `dust_balance`, `sweep_dust`) for residual
/// integer division remainders.
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

    /// Route residual rounding dust of `token` into the treasury's dedicated
    /// protocol dust account.
    pub fn route_dust(env: Env, from: Address, token: Address, amount: i128) {
        from.require_auth();
        if amount < 0 {
            panic!("dust amount cannot be negative");
        }
        if amount == 0 {
            return;
        }
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let prev = Self::dust_balance(env.clone(), token.clone());
        env.storage()
            .instance()
            .set(&DataKey::DustAccount(token.clone()), &(prev + amount));
        env.events()
            .publish((symbol_short!("dust_in"), token), (from, amount));
    }

    /// View the accumulated dust balance for `token`.
    pub fn dust_balance(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::DustAccount(token))
            .unwrap_or(0)
    }

    /// Admin-gated sweep of accumulated protocol dust to `to`.
    pub fn sweep_dust(env: Env, caller: Address, token: Address, to: Address) -> i128 {
        caller.require_auth();
        if caller != Self::admin(env.clone()) {
            panic!("unauthorized");
        }
        let dust = Self::dust_balance(env.clone(), token.clone());
        if dust <= 0 {
            return 0;
        }
        env.storage()
            .instance()
            .set(&DataKey::DustAccount(token.clone()), &0i128);
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &dust);
        env.events()
            .publish((symbol_short!("dust_swp"), token), (to, dust));
        dust
    }

    /// Deterministic largest-remainder split helper exposed on-chain.
    pub fn allocate_split(
        env: Env,
        total: i128,
        weights: Vec<i128>,
    ) -> Result<(Vec<i128>, i128), AllocationError> {
        let mut w_slice = StdVec::with_capacity(weights.len() as usize);
        for w in weights.iter() {
            w_slice.push(w);
        }
        let (allocs, dust) = allocate_largest_remainder(total, &w_slice)?;
        let mut out = Vec::new(&env);
        for a in allocs {
            out.push_back(a);
        }
        Ok((out, dust))
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_and_dust_routing_and_largest_remainder_conservation() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let router = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        sac.mint(&router, &15);

        client.route_dust(&router, &token_id, &15);
        assert_eq!(client.dust_balance(&token_id), 15);

        let to = Address::generate(&env);
        assert_eq!(client.sweep_dust(&admin, &token_id, &to), 15);
        assert_eq!(client.dust_balance(&token_id), 0);
        assert_eq!(token_client.balance(&to), 15);

        // Boundary & conservation checks:
        // 1. More recipients than total units (dust dominates: total=3, 5 equal weights -> [1,1,1,0,0])
        let (out, dust) = allocate_largest_remainder(3, &[10, 10, 10, 10, 10]).unwrap();
        assert_eq!(out, alloc::vec![1, 1, 1, 0, 0]);
        assert_eq!(dust, 0);
        assert_eq!(out.iter().sum::<i128>() + dust, 3);

        // 2. All zero weights -> all outputs 0, dust == total.
        let (out_z, dust_z) = allocate_largest_remainder(777, &[0, 0, 0]).unwrap();
        assert_eq!(out_z, alloc::vec![0, 0, 0]);
        assert_eq!(dust_z, 777);
        assert_eq!(out_z.iter().sum::<i128>() + dust_z, 777);
    }
}
