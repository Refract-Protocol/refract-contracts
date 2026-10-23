//! Test-only mock SEP-41 token that can be told to fail transfers to
//! specific recipients in each of the ways a real asset can.
//!
//! Failure modes reuse the Stellar Asset Contract error codes, so
//! [`crate::payout::try_transfer`] classifies them exactly as it would a real
//! SAC failure. `Blocked` uses a code the SAC never returns, the way a custom
//! token's blocklist would.

#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env,
};

/// How the mock token treats transfers *to* a given recipient.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum MockMode {
    Normal = 0,
    Frozen = 1,
    Blocked = 2,
    MissingTrustline = 3,
    Unauthorized = 4,
    /// Traps with a plain panic instead of a contract error.
    Trap = 5,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MockTokenError {
    Unauthorized = 4,
    Balance = 10,
    Deauthorized = 11,
    TrustlineMissing = 13,
    /// Not a SAC code: stands in for a custom token's recipient blocklist.
    Blocklisted = 100,
}

#[contracttype]
enum MockKey {
    Balance(Address),
    Mode(Address),
}

/// Mock SEP-41 token. Only `transfer` / `balance` / `mint` / `set_mode` are
/// implemented; that is all the payout paths use.
#[contract]
pub struct MockToken;

#[contractimpl]
impl MockToken {
    pub fn mint(env: Env, to: Address, amount: i128) {
        let bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&MockKey::Balance(to), &(bal + amount));
    }

    pub fn set_mode(env: Env, addr: Address, mode: MockMode) {
        env.storage().persistent().set(&MockKey::Mode(addr), &mode);
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&MockKey::Balance(id))
            .unwrap_or(0)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        let mode: MockMode = env
            .storage()
            .persistent()
            .get(&MockKey::Mode(to.clone()))
            .unwrap_or(MockMode::Normal);
        match mode {
            MockMode::Normal => {}
            MockMode::Frozen => panic_with_error!(&env, MockTokenError::Deauthorized),
            MockMode::Blocked => panic_with_error!(&env, MockTokenError::Blocklisted),
            MockMode::MissingTrustline => {
                panic_with_error!(&env, MockTokenError::TrustlineMissing)
            }
            MockMode::Unauthorized => panic_with_error!(&env, MockTokenError::Unauthorized),
            MockMode::Trap => panic!("mock token trap"),
        }
        let from_bal = Self::balance(env.clone(), from.clone());
        if from_bal < amount {
            panic_with_error!(&env, MockTokenError::Balance);
        }
        env.storage()
            .persistent()
            .set(&MockKey::Balance(from), &(from_bal - amount));
        let to_bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&MockKey::Balance(to), &(to_bal + amount));
    }
}
