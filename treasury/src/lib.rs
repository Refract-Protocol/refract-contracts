#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, IntoVal,
    Symbol,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
}

/// `RefractTreasury` is a token-custody vault with admin-gated withdrawals and
/// cross-contract disbursement helpers.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    /// Privileged: admin-only rotation of the treasury admin.
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        Ok(())
    }

    /// Privileged: withdraw `amount` of `token` to `to`. Only `admin` may call this.
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
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Cross-contract helper that attempts to invoke `award` on `bounty_contract`
    /// on behalf of `caller` so tests can verify that a sub-invocation cannot
    /// borrow `caller`'s authority unless `caller`'s auth tree explicitly
    /// authorizes the `bounty_contract.award(...)` sub-invocation!
    pub fn relay_bounty_award(
        env: Env,
        caller: Address,
        bounty_contract: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        let args = (caller, researcher, amount, report_hash).into_val(&env);
        let _: () = env.invoke_contract(&bounty_contract, &Symbol::new(&env, "award"), args);
        Ok(())
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    //! ## Privileged Entrypoints in `RefractTreasury` (#253)
    //!
    //! | Entrypoint           | Required Caller(s) | Auth Check              |
    //! |----------------------|--------------------|-------------------------|
    //! | `set_admin`          | `admin`            | `caller.require_auth()` |
    //! | `withdraw`           | `admin`            | `caller.require_auth()` |
    //! | `relay_bounty_award` | `admin` + sub-auth | `caller.require_auth()` |

    extern crate std;

    use super::*;
    use refract_bounty::{RefractBountyEscrow, RefractBountyEscrowClient};
    use soroban_sdk::{
        testutils::{
            Address as _, AuthorizedFunction, AuthorizedInvocation, MockAuth, MockAuthInvoke,
        },
        Env,
    };

    #[test]
    fn cross_contract_sub_invocation_cannot_borrow_ungranted_authority() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let researcher = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        env.mock_all_auths();

        let treasury_id = env.register_contract(None, RefractTreasury);
        let treasury = RefractTreasuryClient::new(&env, &treasury_id);
        treasury.initialize(&admin);

        let bounty_id = env.register_contract(None, RefractBountyEscrow);
        let bounty = RefractBountyEscrowClient::new(&env, &bounty_id);
        bounty.initialize(&admin, &token_id);

        token::StellarAssetClient::new(&env, &token_id).mint(&treasury_id, &5_000);
        treasury.withdraw(&admin, &token_id, &3_000, &admin);
        bounty.fund(&admin, &3_000);

        let report_hash = BytesN::from_array(&env, &[0xAB; 32]);

        // 1. Negative test: admin signs ONLY the top-level `treasury.relay_bounty_award`
        //    call WITHOUT authorizing the nested `bounty.award` sub-invocation.
        //    Soroban's auth-tree check MUST reject the sub-invocation!
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &treasury_id,
                fn_name: "relay_bounty_award",
                args: (
                    admin.clone(),
                    bounty_id.clone(),
                    researcher.clone(),
                    1_000i128,
                    report_hash.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(treasury
            .try_relay_bounty_award(&admin, &bounty_id, &researcher, &1_000, &report_hash)
            .is_err());

        // 2. Negative test: stranger cannot withdraw from treasury.
        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &treasury_id,
                fn_name: "withdraw",
                args: (
                    stranger.clone(),
                    token_id.clone(),
                    500i128,
                    stranger.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            treasury.try_withdraw(&stranger, &token_id, &500, &stranger),
            Err(Ok(TreasuryError::Unauthorized))
        );

        // 3. Positive test: admin explicitly authorizes the full cross-contract auth tree
        //    (`treasury.relay_bounty_award` -> `bounty.award`), and we assert `env.auths()`.
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &treasury_id,
                fn_name: "relay_bounty_award",
                args: (
                    admin.clone(),
                    bounty_id.clone(),
                    researcher.clone(),
                    1_000i128,
                    report_hash.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &bounty_id,
                    fn_name: "award",
                    args: (
                        admin.clone(),
                        researcher.clone(),
                        1_000i128,
                        report_hash.clone(),
                    )
                        .into_val(&env),
                    sub_invokes: &[],
                }],
            },
        }]);
        treasury.relay_bounty_award(&admin, &bounty_id, &researcher, &1_000, &report_hash);
        assert_eq!(
            env.auths(),
            std::vec![(
                admin.clone(),
                AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        treasury_id.clone(),
                        Symbol::new(&env, "relay_bounty_award"),
                        (
                            admin.clone(),
                            bounty_id.clone(),
                            researcher.clone(),
                            1_000i128,
                            report_hash.clone(),
                        )
                            .into_val(&env),
                    )),
                    sub_invocations: std::vec![AuthorizedInvocation {
                        function: AuthorizedFunction::Contract((
                            bounty_id.clone(),
                            Symbol::new(&env, "award"),
                            (
                                admin.clone(),
                                researcher.clone(),
                                1_000i128,
                                report_hash.clone(),
                            )
                                .into_val(&env),
                        )),
                        sub_invocations: std::vec![],
                    }],
                }
            )]
        );
        assert_eq!(bounty.balance(), 2_000);
    }
}
