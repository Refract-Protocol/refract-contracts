#![no_std]

#[path = "../../shared/fee.rs"]
mod fee;

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol};

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    FeeBps,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ParamsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidFeeBps = 4,
}

#[contract]
pub struct RefractParams;

#[contractimpl]
impl RefractParams {
    pub fn initialize(env: Env, admin: Address) -> Result<(), ParamsError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ParamsError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::FeeBps, &0u32);
        Ok(())
    }

    pub fn set_fee_bps(
        env: Env,
        caller: Address,
        fee_bps: u32,
    ) -> Result<(), ParamsError> {
        caller.require_auth();
        if caller != Self::admin(&env)? {
            return Err(ParamsError::Unauthorized);
        }
        if fee_bps > fee::MAX_FEE_BPS {
            return Err(ParamsError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.events()
            .publish((Symbol::new(&env, "fee_bps_set"),), (fee_bps,));
        Ok(())
    }

    pub fn fee_bps(env: Env) -> Result<u32, ParamsError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(ParamsError::NotInitialized);
        }
        Ok(env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0))
    }

    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }
}