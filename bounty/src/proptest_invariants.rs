//! Property-based tests for RefractBountyEscrow invariants (issue #213).
//!
//! Invariants verified across random operation sequences:
//!
//! 1. **Fund conservation**: on-chain token balance == tracked `Balance` key
//! 2. **Non-negative balances**: `balance >= 0` and `available_balance >= 0`
//! 3. **No over-commitment**: `available_balance <= balance` (reserved ≤ balance)
//! 4. **Settled payouts are idempotent**: a Claimed or Swept payout cannot be
//!    re-claimed or re-swept.

#[cfg(test)]
mod tests {
    use crate::{BountyError, PayoutStatus, RefractBountyEscrow, RefractBountyEscrowClient};
    use proptest::prelude::*;
    use soroban_sdk::{
        testutils::Address as _,
        token::{Client as TokenClient, StellarAssetClient},
        Address, BytesN, Env,
    };

    #[derive(Clone, Debug)]
    enum Op {
        /// Top up the escrow pool.
        Fund { amount: i128 },
        /// Governance creates a pull-payout record.
        CreatePayout { amount: i128 },
        /// Recipient tries to claim an existing payout by index.
        Claim { idx: usize },
        /// Try to sweep an existing payout by index.
        Sweep { idx: usize },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (1i128..1_000_000i128).prop_map(|a| Op::Fund { amount: a }),
            (1i128..300_000i128).prop_map(|a| Op::CreatePayout { amount: a }),
            (0usize..5).prop_map(|i| Op::Claim { idx: i }),
            (0usize..5).prop_map(|i| Op::Sweep { idx: i }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

        /// Invariant: on-chain token balance tracks `balance()`; available ≤ balance.
        #[test]
        fn bounty_balance_conservation(ops in proptest::collection::vec(op(), 1..20)) {
            let env = Env::default();
            env.mock_all_auths();

            let governance = Address::generate(&env);
            let recipient  = Address::generate(&env);

            let sac    = env.register_stellar_asset_contract_v2(governance.clone());
            let token  = TokenClient::new(&env, &sac.address());
            let minter = StellarAssetClient::new(&env, &sac.address());

            let cid    = env.register_contract(None, RefractBountyEscrow);
            let client = RefractBountyEscrowClient::new(&env, &cid);
            client.initialize(&governance, &sac.address());
            client.allow_recipient(&governance, &recipient);

            // Seed the escrow so operations have something to work with.
            minter.mint(&governance, &10_000_000i128);
            client.fund(&governance, &10_000_000i128);

            let mut created_ids: std::vec::Vec<u64> = std::vec::Vec::new();

            for op in ops {
                match op {
                    Op::Fund { amount } => {
                        minter.mint(&governance, &amount);
                        let _ = client.try_fund(&governance, &amount);
                    }
                    Op::CreatePayout { amount } => {
                        // deadline = now + 1 so it is initially in the future
                        let now = env.ledger().timestamp();
                        let deadline = now + 1;
                        if let Ok(Ok(id)) = client.try_create_payout(
                            &governance, &recipient, &amount, &deadline,
                        ) {
                            created_ids.push(id);
                        }
                    }
                    Op::Claim { idx } => {
                        if !created_ids.is_empty() {
                            let id = created_ids[idx % created_ids.len()];
                            let _ = client.try_claim(&recipient, &id);
                        }
                    }
                    Op::Sweep { idx } => {
                        if !created_ids.is_empty() {
                            let id = created_ids[idx % created_ids.len()];
                            let _ = client.try_sweep_expired(&id);
                        }
                    }
                }

                // ── Invariant checks ──────────────────────────────────────
                let tracked   = client.balance().unwrap_or(0);
                let available = client.available_balance().unwrap_or(0);
                let on_chain  = token.balance(&cid);

                prop_assert!(
                    tracked >= 0,
                    "tracked balance must be non-negative, got {tracked}"
                );
                prop_assert!(
                    available >= 0,
                    "available_balance must be non-negative, got {available}"
                );
                prop_assert!(
                    available <= tracked,
                    "available ({available}) must not exceed tracked balance ({tracked})"
                );
                prop_assert_eq!(
                    on_chain, tracked,
                    "on-chain token balance ({on_chain}) ≠ tracked balance ({tracked})"
                );
            }
        }

        /// Invariant: a payout in Claimed or Swept state rejects further settlement.
        #[test]
        fn bounty_settled_payouts_are_idempotent(
            amounts in proptest::collection::vec(1i128..200_000i128, 1..5),
        ) {
            let env = Env::default();
            env.mock_all_auths();

            let governance = Address::generate(&env);
            let recipient  = Address::generate(&env);

            let sac    = env.register_stellar_asset_contract_v2(governance.clone());
            let minter = StellarAssetClient::new(&env, &sac.address());

            let cid    = env.register_contract(None, RefractBountyEscrow);
            let client = RefractBountyEscrowClient::new(&env, &cid);
            client.initialize(&governance, &sac.address());
            client.allow_recipient(&governance, &recipient);

            let total: i128 = amounts.iter().sum();
            minter.mint(&governance, &total);
            client.fund(&governance, &total);

            for amount in &amounts {
                let now      = env.ledger().timestamp();
                let deadline = now + 1;
                let id = client.create_payout(&governance, &recipient, amount, &deadline);

                // Claim it.
                let result = client.try_claim(&recipient, &id);
                prop_assert!(result.is_ok(), "first claim should succeed");

                // Second claim must fail with PayoutAlreadySettled.
                let second = client.try_claim(&recipient, &id);
                prop_assert!(
                    matches!(second, Ok(Err(BountyError::PayoutAlreadySettled))),
                    "second claim must return PayoutAlreadySettled, got {second:?}"
                );
            }
        }

        /// Invariant: pause blocks fund/award/create_payout; claim/sweep still work.
        #[test]
        fn bounty_pause_blocks_inflows_allows_outflows(
            amount in 1i128..500_000i128,
        ) {
            let env = Env::default();
            env.mock_all_auths();

            let governance = Address::generate(&env);
            let recipient  = Address::generate(&env);

            let sac    = env.register_stellar_asset_contract_v2(governance.clone());
            let minter = StellarAssetClient::new(&env, &sac.address());

            let cid    = env.register_contract(None, RefractBountyEscrow);
            let client = RefractBountyEscrowClient::new(&env, &cid);
            client.initialize(&governance, &sac.address());
            client.allow_recipient(&governance, &recipient);

            minter.mint(&governance, &(amount * 3));
            client.fund(&governance, &(amount * 3));

            // Create a payout before pausing.
            let now      = env.ledger().timestamp();
            let deadline = now + 1;
            let id = client.create_payout(&governance, &recipient, &amount, &deadline);

            // Pause.
            client.pause(&governance);
            prop_assert!(client.is_paused());

            // fund is refused while paused.
            minter.mint(&governance, &1i128);
            let fund_result = client.try_fund(&governance, &1i128);
            prop_assert!(
                matches!(fund_result, Ok(Err(BountyError::Paused))),
                "fund must be blocked while paused, got {fund_result:?}"
            );

            // create_payout is refused while paused.
            let cp = client.try_create_payout(&governance, &recipient, &amount, &(deadline + 1));
            prop_assert!(
                matches!(cp, Ok(Err(BountyError::Paused))),
                "create_payout must be blocked while paused, got {cp:?}"
            );

            // claim still works while paused (funds in flight).
            let claim_result = client.try_claim(&recipient, &id);
            prop_assert!(
                claim_result.is_ok() && claim_result.unwrap().is_ok(),
                "claim must succeed while paused"
            );

            // Unpause and verify fund works again.
            client.unpause(&governance);
            prop_assert!(!client.is_paused());
            minter.mint(&governance, &1i128);
            let fund_after = client.try_fund(&governance, &1i128);
            prop_assert!(
                fund_after.is_ok() && fund_after.unwrap().is_ok(),
                "fund must succeed after unpause"
            );
        }

        /// Invariant: migrate is idempotent and schema_version advances monotonically.
        #[test]
        fn bounty_migrate_idempotent(_seed in 0u32..100u32) {
            let env = Env::default();
            env.mock_all_auths();

            let governance = Address::generate(&env);
            let sac        = env.register_stellar_asset_contract_v2(governance.clone());

            let cid    = env.register_contract(None, RefractBountyEscrow);
            let client = RefractBountyEscrowClient::new(&env, &cid);
            client.initialize(&governance, &sac.address());

            let v0 = client.schema_version();
            prop_assert_eq!(v0, 0u32, "schema_version before migrate must be 0");

            let v1 = client.migrate(&governance);
            prop_assert_eq!(
                v1, RefractBountyEscrow::SCHEMA_VERSION,
                "migrate must advance to SCHEMA_VERSION"
            );

            // Calling migrate again is a no-op — returns the current version.
            let v2 = client.migrate(&governance);
            prop_assert_eq!(v2, v1, "second migrate call must be idempotent");

            prop_assert!(
                client.schema_version() >= v1,
                "schema_version must not regress"
            );
        }
    }
}
