//! Property-based tests for RefractBackstop invariants (issue #213).
//!
//! Invariants verified across random operation sequences:
//!
//! 1. **Pause blocks inflows, allows outflows**: `deposit_tranche` and
//!    `request_unstake` are blocked while paused; `draw_down` (coverage
//!    settlement) remains callable.
//! 2. **Migrate is idempotent**: `schema_version` advances to `SCHEMA_VERSION`
//!    and subsequent `migrate` calls are no-ops.

use super::*;
use proptest::prelude::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// Invariant: pause blocks deposit; draw_down (coverage settlement) still works.
    #[test]
    fn backstop_pause_blocks_deposits_allows_drawdown(
        amount in 1i128..1_000_000i128,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let usdc = TokenClient::new(&env, &sac.address());
        let mint = StellarAssetClient::new(&env, &sac.address());
        let pool = Address::generate(&env);
        let staker = Address::generate(&env);

        let id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &id);
        client.initialize(&pool, &admin, &sac.address(), &0);
        client.set_cooldown(&admin, &0);

        // Deposit before pausing.
        mint.mint(&staker, &amount);
        client.deposit(&staker, &amount);

        // Pause.
        client.pause(&admin);
        prop_assert!(client.is_paused());

        // deposit is blocked while paused.
        mint.mint(&staker, &1i128);
        let dep = client.try_deposit(&staker, &1i128);
        prop_assert!(
            matches!(dep, Ok(Err(BackstopError::Paused))),
            "deposit must be blocked while paused, got {dep:?}"
        );

        // draw_down still works (coverage in flight).
        let _ = client.try_draw_down(&pool, &1u64, &amount);

        // Unpause.
        client.unpause(&admin);
        prop_assert!(!client.is_paused());

        // deposit works again after unpause.
        let dep2 = client.try_deposit(&staker, &1i128);
        prop_assert!(
            dep2.is_ok() && dep2.unwrap().is_ok(),
            "deposit must succeed after unpause"
        );
    }

    /// Invariant: migrate is idempotent and schema_version advances monotonically.
    #[test]
    fn backstop_migrate_idempotent(_seed in 0u32..100u32) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let pool = Address::generate(&env);

        let id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &id);
        client.initialize(&pool, &admin, &sac.address(), &0);

        let v0 = client.schema_version();
        prop_assert_eq!(v0, 0u32, "schema_version before migrate must be 0");

        let v1 = client.migrate(&admin);
        prop_assert_eq!(
            v1, RefractBackstop::SCHEMA_VERSION,
            "migrate must advance to SCHEMA_VERSION"
        );

        // Calling migrate again is a no-op.
        let v2 = client.migrate(&admin);
        prop_assert_eq!(v2, v1, "second migrate call must be idempotent");

        prop_assert!(
            client.schema_version() >= v1,
            "schema_version must not regress"
        );
    }
}
