#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
}

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
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
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

    /// Privileged: governance-only rotation of the timelock/governance authority.
    pub fn set_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        Ok(())
    }

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

    /// Privileged: governance/timelock-only payout authorization.
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

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

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
    //! ## Privileged Entrypoints in `RefractBountyEscrow` (#253)
    //!
    //! | Entrypoint       | Required Caller(s) | Auth Check              |
    //! |------------------|--------------------|-------------------------|
    //! | `set_governance` | `governance`       | `caller.require_auth()` |
    //! | `award`          | `governance`       | `caller.require_auth()` |
    //! | `fund`           | `from`             | `from.require_auth()`   |

    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, MockAuth, MockAuthInvoke},
        Env, IntoVal,
    };

    #[test]
    fn bounty_explicit_auth_and_negative_authorization_coverage() {
        let env = Env::default();
        let governance = Address::generate(&env);
        let new_governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        env.mock_all_auths();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &5_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);
        client.fund(&funder, &3_000);

        let report_hash = BytesN::from_array(&env, &[0x11; 32]);

        // Missing require_auth signature must fail even when caller == governance.
        env.mock_auths(&[]);
        assert!(client
            .try_award(&governance, &researcher, &1_000, &report_hash)
            .is_err());
        assert!(client
            .try_set_governance(&governance, &new_governance)
            .is_err());

        // Stranger with valid signature must fail with Unauthorized.
        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "award",
                args: (
                    stranger.clone(),
                    researcher.clone(),
                    1_000i128,
                    report_hash.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_award(&stranger, &researcher, &1_000, &report_hash),
            Err(Ok(BountyError::Unauthorized))
        );

        // Rotate governance and verify old governance is rejected.
        env.mock_auths(&[MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_governance",
                args: (governance.clone(), new_governance.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_governance(&governance, &new_governance);

        env.mock_auths(&[MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "award",
                args: (
                    governance.clone(),
                    researcher.clone(),
                    1_000i128,
                    report_hash.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_award(&governance, &researcher, &1_000, &report_hash),
            Err(Ok(BountyError::Unauthorized))
        );
    }
}
