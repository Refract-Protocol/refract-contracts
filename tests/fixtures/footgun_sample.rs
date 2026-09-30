// Deliberate Footgun Fixture for Static Analysis Gate Testing
#![no_std]
use soroban_sdk::{contractimpl, Env, Address};

pub struct VulnerableContract;

#[contractimpl]
impl VulnerableContract {
    // VIOLATION 1: unwrap() in contractimpl
    pub fn risky_operation(env: Env, val: Option<u32>) -> u32 {
        val.unwrap()
    }

    // VIOLATION 2: expect() in contractimpl
    pub fn risky_expect(env: Env, val: Result<u32, ()>) -> u32 {
        val.expect("should not panic")
    }

    // VIOLATION 3: State-mutating function missing require_auth
    pub fn update_admin_unauthorized(env: Env, new_admin: Address) {
        // Mutates state without requiring caller auth
        env.storage().instance().set(&1, &new_admin);
    }
}
