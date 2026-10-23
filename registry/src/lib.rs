//! # RefractContractRegistry — closes #284
//!
//! An on-chain registry of **trusted contract IDs** for every Refract protocol
//! component.  Other contracts call [`RefractContractRegistry::is_trusted`]
//! (or the helper [`RefractContractRegistry::require_trusted`]) before
//! forwarding funds or administrative authority to a given address, ensuring
//! that a misconfigured or malicious contract can never be substituted at
//! runtime.
//!
//! ## Contract roles
//!
//! | `ContractRole` variant | Purpose                                  |
//! |------------------------|------------------------------------------|
//! | `Pool`                 | Primary insurance pool                   |
//! | `Oracle`               | Price / event oracle                     |
//! | `Policy`               | Policy registry mirror                   |
//! | `Governance`           | Governor / voting contract               |
//! | `Timelock`             | Execution timelock                       |
//! | `Treasury`             | Protocol fee vault                       |
//! | `Backstop`             | Second-loss reinsurance tranche          |
//! | `Bounty`               | Bug-bounty escrow                        |
//! | `Grants`               | Milestone-based grants disbursement      |
//! | `VeToken`              | Vote-escrowed governance token           |
//! | `Multisig`             | M-of-N threshold multisig                |
//!
//! ## Authorization model
//!
//! * Only the **admin** may register, replace, or remove a contract address.
//! * The admin is typically the protocol's timelock or multisig — set at
//!   [`RefractContractRegistry::initialize`] and rotatable via the two-step
//!   [`RefractContractRegistry::propose_admin`] /
//!   [`RefractContractRegistry::accept_admin`] pattern.
//! * Any caller may call [`RefractContractRegistry::is_trusted`] or
//!   [`RefractContractRegistry::get`] to read the current allowlist.
//!
//! ## Usage pattern in downstream contracts
//!
//! ```ignore
//! // Inside pool::process_claim or backstop::cover_shortfall:
//! let registry_addr: Address = env.storage().instance().get(&DataKey::Registry).ok_or(PoolError::NotInitialized)?;
//! let registry = refract_registry::RegistryClient::new(&env, &registry_addr);
//! registry.require_trusted(&caller_contract_id, &ContractRole::Pool);
//! ```
//!
//! Alternatively, call `is_trusted` and return a typed error without
//! cross-contract panics.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol,
};

// ── Role enum ─────────────────────────────────────────────────────────────────

/// Canonical role labels for every protocol component.
///
/// This is a `contracttype` so it can be used as a storage key and passed
/// across contract boundaries without encoding overhead.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ContractRole {
    Pool        = 0,
    Oracle      = 1,
    Policy      = 2,
    Governance  = 3,
    Timelock    = 4,
    Treasury    = 5,
    Backstop    = 6,
    Bounty      = 7,
    Grants      = 8,
    VeToken     = 9,
    Multisig    = 10,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    /// Contract has already been initialized.
    AlreadyInitialized  = 1,
    /// Contract has not yet been initialized.
    NotInitialized      = 2,
    /// Caller is not the admin.
    Unauthorized        = 3,
    /// No address registered for the requested role.
    RoleNotFound        = 4,
    /// The supplied address is not the registered trusted address for that role.
    Untrusted           = 5,
    /// There is no pending admin to accept.
    NoPendingAdmin      = 6,
    /// The zero address was supplied where a real contract address is required.
    ZeroAddress         = 7,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Current administrator address (instance).
    Admin,
    /// Pending admin awaiting `accept_admin` confirmation (instance).
    PendingAdmin,
    /// Registered contract address for a given role (persistent).
    Entry(ContractRole),
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractContractRegistry;

#[contractimpl]
impl RefractContractRegistry {
    // ─── Initialization ──────────────────────────────────────────────────

    /// Deploy the registry.  `admin` is the sole address authorized to
    /// mutate the allowlist going forward.
    pub fn initialize(env: Env, admin: Address) -> Result<(), RegistryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(RegistryError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.events()
            .publish((Symbol::new(&env, "reg_init"),), (admin,));
        Ok(())
    }

    // ─── Admin views ─────────────────────────────────────────────────────

    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    // ─── Two-step admin rotation ─────────────────────────────────────────

    /// Current admin nominates a successor.  The successor must call
    /// `accept_admin` to complete the rotation.
    pub fn propose_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((Symbol::new(&env, "reg_propose_admin"),), (new_admin,));
        Ok(())
    }

    /// The pending admin accepts the nomination, completing the rotation.
    pub fn accept_admin(env: Env, caller: Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(RegistryError::NoPendingAdmin)?;
        if caller != pending {
            return Err(RegistryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((Symbol::new(&env, "reg_admin_accepted"),), (pending,));
        Ok(())
    }

    // ─── Allowlist mutations ──────────────────────────────────────────────

    /// Register (or replace) the trusted address for `role`.
    ///
    /// Replacing an entry atomically revokes the old address and registers
    /// the new one in a single admin-gated call, so there is no window where
    /// both addresses are simultaneously trusted.
    pub fn set(
        env: Env,
        caller: Address,
        role: ContractRole,
        contract_addr: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        // Emit the old address (if any) in the event so monitors can detect
        // unexpected replacements.
        let old: Option<Address> = env.storage().persistent().get(&DataKey::Entry(role));
        env.storage()
            .persistent()
            .set(&DataKey::Entry(role), &contract_addr);
        env.events().publish(
            (Symbol::new(&env, "reg_set"), role as u32),
            (contract_addr, old),
        );
        Ok(())
    }

    /// Remove the trusted address for `role`.  After removal,
    /// `is_trusted` and `require_trusted` will return false / error for
    /// any address queried against that role.
    pub fn remove(
        env: Env,
        caller: Address,
        role: ContractRole,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        let old: Option<Address> = env.storage().persistent().get(&DataKey::Entry(role));
        env.storage().persistent().remove(&DataKey::Entry(role));
        env.events().publish(
            (Symbol::new(&env, "reg_removed"), role as u32),
            (old,),
        );
        Ok(())
    }

    // ─── Allowlist queries ────────────────────────────────────────────────

    /// Return the registered address for `role`, or `None` if not set.
    pub fn get(env: Env, role: ContractRole) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Entry(role))
    }

    /// Return `true` iff `addr` is the currently registered trusted address
    /// for `role`.
    pub fn is_trusted(env: Env, addr: Address, role: ContractRole) -> bool {
        match env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::Entry(role))
        {
            Some(registered) => registered == addr,
            None => false,
        }
    }

    /// Require that `addr` is the registered trusted address for `role`.
    ///
    /// Returns `Ok(())` if the check passes, `Err(RegistryError::Untrusted)`
    /// or `Err(RegistryError::RoleNotFound)` otherwise.  Downstream
    /// contracts call this instead of `is_trusted` so they get a typed
    /// error they can surface to callers rather than silently discarding
    /// the operation.
    pub fn require_trusted(
        env: Env,
        addr: Address,
        role: ContractRole,
    ) -> Result<(), RegistryError> {
        let registered: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Entry(role))
            .ok_or(RegistryError::RoleNotFound)?;
        if registered != addr {
            return Err(RegistryError::Untrusted);
        }
        Ok(())
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        if *caller != admin {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    fn setup(env: &Env) -> (Address, Address) {
        let admin = Address::generate(env);
        let cid = env.register(RefractContractRegistry, ());
        env.as_contract(&cid, || {
            RefractContractRegistry::initialize(env.clone(), admin.clone()).unwrap()
        });
        (cid, admin)
    }

    #[test]
    fn initialize_is_idempotent() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let err = env.as_contract(&cid, || {
            RefractContractRegistry::initialize(env.clone(), admin.clone())
        });
        assert_eq!(err, Err(RegistryError::AlreadyInitialized));
    }

    #[test]
    fn set_and_get_entry() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let pool = Address::generate(&env);

        env.as_contract(&cid, || {
            RefractContractRegistry::set(
                env.clone(),
                admin.clone(),
                ContractRole::Pool,
                pool.clone(),
            )
            .unwrap()
        });

        let got = env.as_contract(&cid, || {
            RefractContractRegistry::get(env.clone(), ContractRole::Pool)
        });
        assert_eq!(got, Some(pool.clone()));
    }

    #[test]
    fn is_trusted_returns_correct_result() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let pool = Address::generate(&env);
        let fake = Address::generate(&env);

        env.as_contract(&cid, || {
            RefractContractRegistry::set(
                env.clone(),
                admin.clone(),
                ContractRole::Pool,
                pool.clone(),
            )
            .unwrap()
        });

        assert!(env.as_contract(&cid, || {
            RefractContractRegistry::is_trusted(
                env.clone(),
                pool.clone(),
                ContractRole::Pool,
            )
        }));

        assert!(!env.as_contract(&cid, || {
            RefractContractRegistry::is_trusted(
                env.clone(),
                fake.clone(),
                ContractRole::Pool,
            )
        }));
    }

    #[test]
    fn require_trusted_errors_on_untrusted() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let real = Address::generate(&env);
        let fake = Address::generate(&env);

        env.as_contract(&cid, || {
            RefractContractRegistry::set(
                env.clone(),
                admin.clone(),
                ContractRole::Backstop,
                real.clone(),
            )
            .unwrap()
        });

        assert_eq!(
            env.as_contract(&cid, || {
                RefractContractRegistry::require_trusted(
                    env.clone(),
                    fake.clone(),
                    ContractRole::Backstop,
                )
            }),
            Err(RegistryError::Untrusted)
        );

        assert_eq!(
            env.as_contract(&cid, || {
                RefractContractRegistry::require_trusted(
                    env.clone(),
                    real.clone(),
                    ContractRole::Backstop,
                )
            }),
            Ok(())
        );
    }

    #[test]
    fn require_trusted_errors_on_unregistered_role() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, _admin) = setup(&env);
        let any = Address::generate(&env);

        assert_eq!(
            env.as_contract(&cid, || {
                RefractContractRegistry::require_trusted(
                    env.clone(),
                    any,
                    ContractRole::Oracle,
                )
            }),
            Err(RegistryError::RoleNotFound)
        );
    }

    #[test]
    fn set_requires_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, _admin) = setup(&env);
        let intruder = Address::generate(&env);
        let target = Address::generate(&env);

        assert_eq!(
            env.as_contract(&cid, || {
                RefractContractRegistry::set(
                    env.clone(),
                    intruder,
                    ContractRole::Treasury,
                    target,
                )
            }),
            Err(RegistryError::Unauthorized)
        );
    }

    #[test]
    fn remove_clears_entry() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let oracle = Address::generate(&env);

        env.as_contract(&cid, || {
            RefractContractRegistry::set(
                env.clone(),
                admin.clone(),
                ContractRole::Oracle,
                oracle.clone(),
            )
            .unwrap()
        });
        env.as_contract(&cid, || {
            RefractContractRegistry::remove(env.clone(), admin.clone(), ContractRole::Oracle)
                .unwrap()
        });

        let got = env.as_contract(&cid, || {
            RefractContractRegistry::get(env.clone(), ContractRole::Oracle)
        });
        assert_eq!(got, None);
    }

    #[test]
    fn replacing_entry_atomically_revokes_old_address() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let old_pool = Address::generate(&env);
        let new_pool = Address::generate(&env);

        env.as_contract(&cid, || {
            RefractContractRegistry::set(
                env.clone(),
                admin.clone(),
                ContractRole::Pool,
                old_pool.clone(),
            )
            .unwrap()
        });
        env.as_contract(&cid, || {
            RefractContractRegistry::set(
                env.clone(),
                admin.clone(),
                ContractRole::Pool,
                new_pool.clone(),
            )
            .unwrap()
        });

        // Old address is no longer trusted.
        assert!(!env.as_contract(&cid, || {
            RefractContractRegistry::is_trusted(
                env.clone(),
                old_pool.clone(),
                ContractRole::Pool,
            )
        }));
        // New address is trusted.
        assert!(env.as_contract(&cid, || {
            RefractContractRegistry::is_trusted(
                env.clone(),
                new_pool.clone(),
                ContractRole::Pool,
            )
        }));
    }

    #[test]
    fn two_step_admin_rotation() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let new_admin = Address::generate(&env);

        // Propose.
        env.as_contract(&cid, || {
            RefractContractRegistry::propose_admin(
                env.clone(),
                admin.clone(),
                new_admin.clone(),
            )
            .unwrap()
        });

        let pending = env.as_contract(&cid, || {
            RefractContractRegistry::pending_admin(env.clone())
        });
        assert_eq!(pending, Some(new_admin.clone()));

        // New admin accepts.
        env.as_contract(&cid, || {
            RefractContractRegistry::accept_admin(env.clone(), new_admin.clone()).unwrap()
        });

        let current_admin =
            env.as_contract(&cid, || RefractContractRegistry::admin(env.clone()));
        assert_eq!(current_admin, Some(new_admin.clone()));
    }

    #[test]
    fn accept_admin_rejects_wrong_caller() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);
        let new_admin = Address::generate(&env);
        let intruder = Address::generate(&env);

        env.as_contract(&cid, || {
            RefractContractRegistry::propose_admin(
                env.clone(),
                admin.clone(),
                new_admin.clone(),
            )
            .unwrap()
        });

        assert_eq!(
            env.as_contract(&cid, || {
                RefractContractRegistry::accept_admin(env.clone(), intruder)
            }),
            Err(RegistryError::Unauthorized)
        );
    }

    #[test]
    fn all_roles_can_be_registered() {
        let env = Env::default();
        env.mock_all_auths();
        let (cid, admin) = setup(&env);

        let roles = [
            ContractRole::Pool,
            ContractRole::Oracle,
            ContractRole::Policy,
            ContractRole::Governance,
            ContractRole::Timelock,
            ContractRole::Treasury,
            ContractRole::Backstop,
            ContractRole::Bounty,
            ContractRole::Grants,
            ContractRole::VeToken,
            ContractRole::Multisig,
        ];

        for role in &roles {
            let addr = Address::generate(&env);
            env.as_contract(&cid, || {
                RefractContractRegistry::set(
                    env.clone(),
                    admin.clone(),
                    *role,
                    addr.clone(),
                )
                .unwrap()
            });
            assert!(env.as_contract(&cid, || {
                RefractContractRegistry::is_trusted(env.clone(), addr, *role)
            }));
        }
    }
}
