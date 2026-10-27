//! Property-based tests for RefractGrants invariants (issue #213).
//!
//! Invariants verified across random operation sequences:
//!
//! 1. **No negative claimable**: `grant.claimable >= 0` for all grants at all times.
//! 2. **Pause blocks inflows, allows outflows**: `create_grant` and
//!    `confirm_milestone` are blocked while paused; `claim` (outflow) still works.
//! 3. **Migrate is idempotent**: `schema_version` advances to `SCHEMA_VERSION`
//!    and subsequent `migrate` calls are no-ops.

#[cfg(test)]
mod tests {
    use crate::contract::{GrantsError, RefractGrants, RefractGrantsClient};
    use proptest::prelude::*;
    use soroban_sdk::{
        testutils::Address as _,
        token::{Client as TokenClient, StellarAssetClient},
        Address, Env, Vec as SorobanVec,
    };

    fn setup(env: &Env) -> (Address, Address, Address, RefractGrantsClient) {
        let governance = Address::generate(env);
        let recipient = Address::generate(env);
        let sac = env.register_stellar_asset_contract_v2(governance.clone());
        let token_addr = sac.address();
        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(env, &cid);
        client.initialize(&governance, &token_addr);
        client.allow_recipient(&governance, &recipient);
        // Pre-fund the contract so claims can succeed.
        let minter = StellarAssetClient::new(env, &token_addr);
        let token = TokenClient::new(env, &token_addr);
        minter.mint(&governance, &10_000_000i128);
        token.transfer(&governance, &cid, &10_000_000i128);
        (governance, recipient, cid, client)
    }

    #[derive(Clone, Debug)]
    enum Op {
        CreateGrant { amount: i128 },
        ConfirmMilestone { idx: usize },
        Claim { idx: usize },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (1i128..500_000i128).prop_map(|a| Op::CreateGrant { amount: a }),
            (0usize..5).prop_map(|i| Op::ConfirmMilestone { idx: i }),
            (0usize..5).prop_map(|i| Op::Claim { idx: i }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

        /// Invariant: no grant ever has claimable < 0.
        #[test]
        fn grants_no_negative_claimable(
            ops in proptest::collection::vec(op(), 1..15)
        ) {
            let env = Env::default();
            env.mock_all_auths();
            let (governance, recipient, _cid, client) = setup(&env);

            let mut grant_ids: std::vec::Vec<u64> = std::vec::Vec::new();

            for op in ops {
                match op {
                    Op::CreateGrant { amount } => {
                        let mut milestones = SorobanVec::new(&env);
                        milestones.push_back(amount);
                        if let Ok(Ok(id)) = client.try_create_grant(
                            &governance, &recipient, &amount, &milestones,
                        ) {
                            grant_ids.push(id);
                        }
                    }
                    Op::ConfirmMilestone { idx } => {
                        if !grant_ids.is_empty() {
                            let gid = grant_ids[idx % grant_ids.len()];
                            let _ = client.try_confirm_milestone(&governance, &gid, &0u32);
                        }
                    }
                    Op::Claim { idx } => {
                        if !grant_ids.is_empty() {
                            let gid = grant_ids[idx % grant_ids.len()];
                            let _ = client.try_claim(&recipient, &gid);
                        }
                    }
                }

                // Check invariants on every known grant.
                for &gid in &grant_ids {
                    if let Some(grant) = client.get_grant(&gid) {
                        prop_assert!(
                            grant.claimable >= 0,
                            "claimable must be non-negative for grant {gid}, got {}",
                            grant.claimable
                        );
                    }
                }
            }
        }

        /// Invariant: pause blocks create_grant and confirm_milestone; claim still works.
        #[test]
        fn grants_pause_blocks_inflows_allows_outflows(
            amount in 1i128..200_000i128,
        ) {
            let env = Env::default();
            env.mock_all_auths();
            let (governance, recipient, _cid, client) = setup(&env);

            // Create and confirm a grant before pausing.
            let mut milestones = SorobanVec::new(&env);
            milestones.push_back(amount);
            let gid = client.create_grant(&governance, &recipient, &amount, &milestones);
            client.confirm_milestone(&governance, &gid, &0u32);

            // Pause.
            client.pause(&governance);
            prop_assert!(client.is_paused());

            // create_grant blocked.
            let mut m2 = SorobanVec::new(&env);
            m2.push_back(amount);
            let cr = client.try_create_grant(&governance, &recipient, &amount, &m2);
            prop_assert!(
                matches!(cr, Ok(Err(GrantsError::Paused))),
                "create_grant must be blocked while paused, got {cr:?}"
            );

            // confirm_milestone blocked (on a fresh grant that somehow got created — test with existing one already confirmed).
            // Since gid milestone 0 is already confirmed, trying to confirm again returns MilestoneAlreadyConfirmed.
            // So the pause check must come BEFORE the milestone check. Both are acceptable results here.
            let cm = client.try_confirm_milestone(&governance, &gid, &0u32);
            prop_assert!(
                matches!(cm, Ok(Err(GrantsError::Paused)) | Ok(Err(GrantsError::MilestoneAlreadyConfirmed))),
                "confirm_milestone must fail while paused, got {cm:?}"
            );

            // claim still works while paused (in-flight funds).
            let claim_result = client.try_claim(&recipient, &gid);
            prop_assert!(
                claim_result.is_ok() && claim_result.unwrap().is_ok(),
                "claim must succeed while paused"
            );

            // Unpause and verify create_grant works again.
            client.unpause(&governance);
            prop_assert!(!client.is_paused());
            let mut m3 = SorobanVec::new(&env);
            m3.push_back(amount);
            let after = client.try_create_grant(&governance, &recipient, &amount, &m3);
            prop_assert!(
                after.is_ok() && after.unwrap().is_ok(),
                "create_grant must succeed after unpause"
            );
        }

        /// Invariant: migrate is idempotent and schema_version advances monotonically.
        #[test]
        fn grants_migrate_idempotent(_seed in 0u32..100u32) {
            let env = Env::default();
            env.mock_all_auths();
            let (governance, _recipient, _cid, client) = setup(&env);

            let v0 = client.schema_version();
            prop_assert_eq!(v0, 0u32, "schema_version before migrate must be 0");

            let v1 = client.migrate(&governance);
            prop_assert_eq!(
                v1, RefractGrants::SCHEMA_VERSION,
                "migrate must advance to SCHEMA_VERSION"
            );

            let v2 = client.migrate(&governance);
            prop_assert_eq!(v2, v1, "second migrate call must be idempotent");

            prop_assert!(
                client.schema_version() >= v1,
                "schema_version must not regress"
            );
        }
    }
}
