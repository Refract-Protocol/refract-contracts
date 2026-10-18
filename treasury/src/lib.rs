#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantRefundRecord {
    pub grant_id: u64,
    pub token: Address,
    pub refunded_amount: i128,
    pub reason_hash: BytesN<32>,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    DuplicateRefund = 4,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    GrantsContract,
    GrantRefund(u64),
    TotalGrantRefunds(Address),
}

/// `RefractTreasury` receives and audits pro-rata grant termination refunds
/// routed via approved timelock/governance actions (#280).
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin (timelock/governance).
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

    /// Receive an unearned grant refund upon early grant termination or downward
    /// scope amendment, recording a permanent audit record.
    pub fn receive_grant_refund(
        env: Env,
        caller: Address,
        from_escrow: Address,
        grant_id: u64,
        token: Address,
        amount: i128,
        reason_hash: BytesN<32>,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        from_escrow.require_auth();
        if caller != Self::admin(env.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::GrantRefund(grant_id))
        {
            return Err(TreasuryError::DuplicateRefund);
        }

        token::Client::new(&env, &token).transfer(
            &from_escrow,
            &env.current_contract_address(),
            &amount,
        );

        let record = GrantRefundRecord {
            grant_id,
            token: token.clone(),
            refunded_amount: amount,
            reason_hash: reason_hash.clone(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::GrantRefund(grant_id), &record);

        let prev: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalGrantRefunds(token.clone()))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalGrantRefunds(token.clone()), &(prev + amount));

        env.events().publish(
            (symbol_short!("grt_rfnd"), grant_id),
            (token, amount, reason_hash),
        );
        Ok(())
    }

    /// Query the permanent refund audit record for `grant_id`.
    pub fn get_grant_refund(env: Env, grant_id: u64) -> Option<GrantRefundRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::GrantRefund(grant_id))
    }

    /// Query cumulative grant termination refunds received for `token`.
    pub fn total_grant_refunds(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalGrantRefunds(token))
            .unwrap_or(0)
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
    fn receive_grant_refund_records_audit_and_conserves_funds() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let grants_escrow = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&grants_escrow, &500);
        let reason = BytesN::from_array(&env, &[4u8; 32]);
        client.receive_grant_refund(&admin, &grants_escrow, &11, &token_id, &500, &reason);

        assert_eq!(token_client.balance(&contract_id), 500);
        assert_eq!(client.total_grant_refunds(&token_id), 500);
        assert_eq!(
            client.get_grant_refund(&11),
            Some(GrantRefundRecord {
                grant_id: 11,
                token: token_id,
                refunded_amount: 500,
                reason_hash: reason,
            })
        );
    }
}
