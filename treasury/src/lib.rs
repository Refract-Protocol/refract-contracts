//! # RefractTreasury
//!
//! Protocol fee and reserve vault with cross-contract version handshake and
//! ABI compatibility probes (`version()`, `expected_counterpart_version`,
//! `probe_counterpart`).

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, token,
    Address, Env, Symbol,
};

pub const TREASURY_ABI_ID: u64 = 0x5452_4541_535F_5631;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub abi_id: u64,
}

#[contractclient(name = "VersionedCounterpartClient")]
pub trait VersionedCounterpart {
    fn version(env: Env) -> ContractVersion;
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CounterpartExpectation {
    pub role: Symbol,
    pub counterpart: Address,
    pub expected: ContractVersion,
    pub verified: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    InvalidCap = 5,
    CapExceeded = 6,
    CounterpartNotConfigured = 7,
    InterfaceMismatch = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    Balance,
    SelfVersion,
    Counterpart(Symbol),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub token: Address,
    pub max_single_withdrawal: i128,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
        max_single_withdrawal: i128,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        governance.require_auth();
        if max_single_withdrawal <= 0 {
            return Err(TreasuryError::InvalidCap);
        }
        let cfg = Config {
            governance,
            token,
            max_single_withdrawal,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(
            &DataKey::SelfVersion,
            &ContractVersion {
                major: 1,
                minor: 0,
                patch: 0,
                abi_id: TREASURY_ABI_ID,
            },
        );
        Ok(())
    }

    /// Returns this contract's semantic version and compile-time ABI identifier.
    pub fn version(env: Env) -> ContractVersion {
        env.storage()
            .instance()
            .get(&DataKey::SelfVersion)
            .unwrap_or(ContractVersion {
                major: 1,
                minor: 0,
                patch: 0,
                abi_id: TREASURY_ABI_ID,
            })
    }

    /// Updates this contract's advertised version (used on upgrade/migration).
    pub fn set_self_version(env: Env, version: ContractVersion) -> Result<(), TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::SelfVersion, &version);
        Ok(())
    }

    /// Configures the expected counterpart version for a role (`"backstop"`, `"grants"`, `"pool"`).
    /// Resets `verified` to `false` until the next successful compatibility probe.
    pub fn set_expected_counterpart_version(
        env: Env,
        role: Symbol,
        counterpart: Address,
        expected: ContractVersion,
    ) -> Result<CounterpartExpectation, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let exp = CounterpartExpectation {
            role: role.clone(),
            counterpart,
            expected,
            verified: false,
        };
        env.storage()
            .instance()
            .set(&DataKey::Counterpart(role), &exp);
        Ok(exp)
    }

    pub fn expected_counterpart_version(env: Env, role: Symbol) -> Option<CounterpartExpectation> {
        env.storage().instance().get(&DataKey::Counterpart(role))
    }

    /// Invalidates a cached counterpart verification after an upgrade so the next call re-probes.
    pub fn invalidate_counterpart_probe(env: Env, role: Symbol) -> Result<(), TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut exp: CounterpartExpectation = env
            .storage()
            .instance()
            .get(&DataKey::Counterpart(role.clone()))
            .ok_or(TreasuryError::CounterpartNotConfigured)?;
        exp.verified = false;
        env.storage()
            .instance()
            .set(&DataKey::Counterpart(role), &exp);
        Ok(())
    }

    /// Probes the counterpart's `version()` and verifies compatibility:
    /// `actual.major == expected.major && actual.minor >= expected.minor && actual.abi_id == expected.abi_id`.
    pub fn probe_counterpart(env: Env, role: Symbol) -> Result<ContractVersion, TreasuryError> {
        let mut exp: CounterpartExpectation = env
            .storage()
            .instance()
            .get(&DataKey::Counterpart(role.clone()))
            .ok_or(TreasuryError::CounterpartNotConfigured)?;

        let client = VersionedCounterpartClient::new(&env, &exp.counterpart);
        let actual = client
            .try_version()
            .map_err(|_| TreasuryError::InterfaceMismatch)?
            .map_err(|_| TreasuryError::InterfaceMismatch)?;

        if !Self::is_compatible(&exp.expected, &actual) {
            exp.verified = false;
            env.storage()
                .instance()
                .set(&DataKey::Counterpart(role.clone()), &exp);
            env.events().publish(
                (symbol_short!("treasury"), symbol_short!("abi_err")),
                (role, exp.expected.major, actual.major, actual.abi_id),
            );
            return Err(TreasuryError::InterfaceMismatch);
        }

        exp.verified = true;
        env.storage()
            .instance()
            .set(&DataKey::Counterpart(role.clone()), &exp);
        env.events().publish(
            (symbol_short!("treasury"), symbol_short!("abi_ok")),
            (role, actual.major, actual.minor, actual.patch),
        );
        Ok(actual)
    }

    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<i128, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        token::Client::new(&env, &cfg.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let next = Self::read_balance(&env)? + amount;
        env.storage().instance().set(&DataKey::Balance, &next);
        Ok(next)
    }

    /// Disburses funds to a configured counterpart (`role`), probing compatibility before the
    /// first state-mutating call and caching the `verified` flag.
    pub fn disburse_to_counterpart(
        env: Env,
        role: Symbol,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if amount > cfg.max_single_withdrawal {
            return Err(TreasuryError::CapExceeded);
        }

        let mut exp: CounterpartExpectation = env
            .storage()
            .instance()
            .get(&DataKey::Counterpart(role.clone()))
            .ok_or(TreasuryError::CounterpartNotConfigured)?;

        if !exp.verified {
            let _ = Self::probe_counterpart(env.clone(), role.clone())?;
            exp = env
                .storage()
                .instance()
                .get(&DataKey::Counterpart(role))
                .ok_or(TreasuryError::CounterpartNotConfigured)?;
        }

        let bal = Self::read_balance(&env)?;
        if amount > bal {
            return Err(TreasuryError::InsufficientBalance);
        }

        token::Client::new(&env, &cfg.token).transfer(
            &env.current_contract_address(),
            &exp.counterpart,
            &amount,
        );
        let next = bal - amount;
        env.storage().instance().set(&DataKey::Balance, &next);
        Ok(next)
    }

    pub fn balance(env: Env) -> i128 {
        Self::read_balance(&env).unwrap_or(0)
    }

    fn is_compatible(expected: &ContractVersion, actual: &ContractVersion) -> bool {
        actual.major == expected.major
            && actual.minor >= expected.minor
            && actual.abi_id == expected.abi_id
    }

    fn read_config(env: &Env) -> Result<Config, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, TreasuryError> {
        let _ = Self::read_config(env)?;
        Ok(env.storage().instance().get(&DataKey::Balance).unwrap_or(0))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn version_handshake_verifies_minor_forward_compat_and_blocks_mismatched_counterparts() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&gov, &10_000);

        let treasury_id = env.register_contract(None, RefractTreasury);
        let treasury = RefractTreasuryClient::new(&env, &treasury_id);
        treasury.initialize(&gov, &token_id, &5_000);
        treasury.deposit(&gov, &5_000);

        // Register a second RefractTreasury instance acting as a versioned counterpart
        let counterpart_id = env.register_contract(None, RefractTreasury);
        let counterpart = RefractTreasuryClient::new(&env, &counterpart_id);
        counterpart.initialize(&gov, &token_id, &5_000);

        let role = symbol_short!("backstop");

        // Configure expectation: major=1, minor=1, abi_id=TREASURY_ABI_ID
        // Counterpart currently has minor=0 (< expected 1) -> InterfaceMismatch
        treasury.set_expected_counterpart_version(
            &role,
            &counterpart_id,
            &ContractVersion {
                major: 1,
                minor: 1,
                patch: 0,
                abi_id: TREASURY_ABI_ID,
            },
        );
        assert_eq!(
            treasury.try_disburse_to_counterpart(&role, &1_000),
            Err(Ok(TreasuryError::InterfaceMismatch))
        );
        // Read views remain available even when counterpart is mismatched
        assert_eq!(treasury.balance(), 5_000);

        // Upgrade counterpart to minor=2 (forward-compatible with expected minor=1)
        counterpart.set_self_version(&ContractVersion {
            major: 1,
            minor: 2,
            patch: 3,
            abi_id: TREASURY_ABI_ID,
        });
        assert_eq!(treasury.disburse_to_counterpart(&role, &1_000), 4_000);
        assert!(
            treasury
                .expected_counterpart_version(&role)
                .unwrap()
                .verified
        );

        // If counterpart upgrades to major=2 and probe is invalidated, next mutating call is blocked
        counterpart.set_self_version(&ContractVersion {
            major: 2,
            minor: 0,
            patch: 0,
            abi_id: TREASURY_ABI_ID,
        });
        treasury.invalidate_counterpart_probe(&role);
        assert_eq!(
            treasury.try_disburse_to_counterpart(&role, &500),
            Err(Ok(TreasuryError::InterfaceMismatch))
        );
    }
}
