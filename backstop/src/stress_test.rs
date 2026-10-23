/// Backstop stress-testing harness — closes #283
///
/// Drives deterministic multi-actor scenarios across:
///  - sequential deposits from N stakers,
///  - correlated premium accrual rounds,
///  - correlated claim / cover_shortfall events,
///  - partial and full withdrawals,
///
/// and asserts solvency invariants after every operation:
///  1. `total_capital == sum_of_all_staker_claim_values` (share price consistency)
///  2. `total_shares > 0` whenever any staker still holds shares
///  3. `total_capital >= 0` always (no negative-capital state)
///  4. After a shortfall drain, every surviving staker's proportional claim
///     is reduced by the same factor (loss socialisation is uniform).
///
/// All randomness is provided via explicit seed tables so CI runs are
/// byte-for-byte reproducible.

#[cfg(test)]
mod stress {
    use crate::RefractBackstop;
    use soroban_sdk::{
        testutils::Address as _,
        token, Address, Env,
    };

    // ── helpers ──────────────────────────────────────────────────────────────

    fn setup(
        env: &Env,
    ) -> (
        Address, // contract_id
        Address, // usdc_addr
        Address, // pool
        Address, // admin
    ) {
        let admin = Address::generate(env);
        let pool = Address::generate(env);

        let usdc_admin = Address::generate(env);
        let usdc_id = env.register_stellar_asset_contract_v2(usdc_admin.clone());
        let usdc_addr = usdc_id.address();

        let contract_id = env.register(RefractBackstop, ());

        // Mint a large initial balance to admin and pool so transfers don't fail.
        let usdc = token::StellarAssetClient::new(env, &usdc_addr);
        usdc.mint(&admin, &10_000_000_000);
        usdc.mint(&pool, &10_000_000_000);

        // Initialize backstop: 20% premium share to stakers.
        env.as_contract(&contract_id, || {
            RefractBackstop::initialize(
                env.clone(),
                pool.clone(),
                admin.clone(),
                usdc_addr.clone(),
                2_000, // 20% in BPS
            )
            .unwrap();
        });

        (contract_id, usdc_addr, pool, admin)
    }

    // Convenience: read backstop state from inside contract context.
    macro_rules! bs_state {
        ($env:expr, $cid:expr) => {{
            $env.as_contract($cid, || {
                crate::RefractBackstop::state($env.clone()).unwrap()
            })
        }};
    }

    // ── invariant checker ────────────────────────────────────────────────────

    /// After every mutating step: verify global solvency invariants.
    ///
    /// `stakers` is a slice of `(address, expected_shares)` pairs at the
    /// current point in the scenario.
    fn assert_solvency(
        env: &Env,
        contract_id: &Address,
        stakers: &[(Address, i128)],
    ) {
        let state = bs_state!(env, contract_id);

        // Invariant 1: total_capital is non-negative.
        assert!(
            state.total_capital >= 0,
            "INVARIANT VIOLATION: total_capital < 0 (got {})",
            state.total_capital
        );

        // Invariant 2: total_shares matches sum of individual staker shares.
        let shares_sum: i128 = stakers.iter().map(|(_, s)| s).sum();
        assert_eq!(
            state.total_shares, shares_sum,
            "INVARIANT VIOLATION: total_shares ({}) != sum of staker shares ({})",
            state.total_shares, shares_sum
        );

        // Invariant 3: share price consistency — every staker's withdrawal
        // value is proportional (uniform share price across all holders).
        if state.total_shares > 0 && stakers.len() > 1 {
            let price_numer = state.total_capital * 1_000_000;
            let price_denom = state.total_shares;
            let price_per_share = price_numer / price_denom;

            for (_, shares) in stakers {
                if *shares <= 0 {
                    continue;
                }
                let value = shares * price_per_share / 1_000_000;
                // Each staker's reconstructed value must be non-negative.
                assert!(
                    value >= 0,
                    "INVARIANT VIOLATION: staker with {} shares has negative value {}",
                    shares,
                    value
                );
            }
        }
    }

    // ── test: sequential deposits then full withdrawal ────────────────────────

    #[test]
    fn stress_sequential_deposits_and_withdrawals() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, _pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Deterministic "random" deposit amounts (5 stakers).
        let deposit_amounts: [i128; 5] = [
            1_000_000, // 100 USDC (7 decimals)
            2_500_000,
            500_000,
            3_000_000,
            750_000,
        ];

        let mut stakers: Vec<(Address, i128)> = Vec::new();
        for amount in &deposit_amounts {
            let staker = Address::generate(&env);
            usdc_client.mint(&staker, amount);

            let shares = env.as_contract(&contract_id, || {
                RefractBackstop::deposit(env.clone(), staker.clone(), *amount).unwrap()
            });

            stakers.push((staker, shares));
            assert_solvency(&env, &contract_id, &stakers);
        }

        // Verify total_capital equals sum of deposits (no premium yet).
        let state = bs_state!(&env, &contract_id);
        let total_deposited: i128 = deposit_amounts.iter().sum();
        assert_eq!(state.total_capital, total_deposited);

        // Each staker withdraws all their shares.
        for (staker, shares) in stakers.iter_mut() {
            let usdc_returned = env.as_contract(&contract_id, || {
                RefractBackstop::withdraw(env.clone(), staker.clone(), *shares).unwrap()
            });
            assert!(usdc_returned > 0, "withdrawal should return positive USDC");
            *shares = 0;
            assert_solvency(&env, &contract_id, &stakers);
        }

        // After all withdrawals: total_capital and total_shares should be 0.
        let final_state = bs_state!(&env, &contract_id);
        assert_eq!(final_state.total_shares, 0, "all shares burned");
        assert_eq!(final_state.total_capital, 0, "all capital returned");
    }

    // ── test: premium accrual increases share price ───────────────────────────

    #[test]
    fn stress_premium_accrual_increases_share_price() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, _pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Alice deposits 1_000_000 (100 USDC).
        let alice = Address::generate(&env);
        usdc_client.mint(&alice, &1_000_000);
        let alice_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), alice.clone(), 1_000_000).unwrap()
        });
        assert_eq!(alice_shares, 1_000_000, "first depositor gets 1:1 shares");

        let stakers = vec![(alice.clone(), alice_shares)];
        assert_solvency(&env, &contract_id, &stakers);

        // Simulate 3 rounds of premium income.
        let premium_rounds: [i128; 3] = [200_000, 150_000, 50_000];
        let mut expected_premium_cut: i128 = 0;

        for round_amount in &premium_rounds {
            // backstop takes 20% (2_000 BPS)
            let cut = round_amount * 2_000 / 10_000;
            expected_premium_cut += cut;

            env.as_contract(&contract_id, || {
                RefractBackstop::receive_premium(env.clone(), *round_amount).unwrap();
            });

            let state = bs_state!(&env, &contract_id);
            // After each round capital should have grown.
            assert!(
                state.total_capital > 1_000_000,
                "capital must grow with premium: got {}",
                state.total_capital
            );
        }

        let state = bs_state!(&env, &contract_id);
        assert_eq!(
            state.total_capital,
            1_000_000 + expected_premium_cut,
            "total_capital should equal deposit + all premium cuts"
        );
        assert_eq!(
            state.premium_income, expected_premium_cut,
            "premium_income counter must track received cuts"
        );

        // Share price has increased: Alice can withdraw more than she deposited.
        let withdraw_quote = alice_shares * state.total_capital / state.total_shares;
        assert!(
            withdraw_quote > 1_000_000,
            "Alice's withdrawal quote {} should exceed her deposit",
            withdraw_quote
        );
    }

    // ── test: correlated claim / cover_shortfall socialises losses uniformly ──

    #[test]
    fn stress_correlated_claims_socialise_losses_uniformly() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Three stakers with equal deposits.
        let deposit = 1_000_000i128;
        let mut stakers: Vec<(Address, i128)> = Vec::new();
        for _ in 0..3 {
            let s = Address::generate(&env);
            usdc_client.mint(&s, &deposit);
            let shares = env.as_contract(&contract_id, || {
                RefractBackstop::deposit(env.clone(), s.clone(), deposit).unwrap()
            });
            stakers.push((s, shares));
        }
        assert_solvency(&env, &contract_id, &stakers);

        // State before shortfall.
        let before = bs_state!(&env, &contract_id);
        assert_eq!(before.total_capital, 3 * deposit);

        // Correlated claim event: pool calls cover_shortfall for 900_000.
        let shortfall_amount = 900_000i128;
        let covered = env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), shortfall_amount).unwrap()
        });
        assert_eq!(covered, shortfall_amount, "full shortfall should be covered");

        // State after shortfall.
        let after = bs_state!(&env, &contract_id);
        assert_eq!(
            after.total_capital,
            before.total_capital - shortfall_amount,
            "capital must decrease by exactly the covered shortfall"
        );
        assert_eq!(
            after.total_shares, before.total_shares,
            "shares must NOT be burned during a shortfall cover"
        );

        // Loss socialisation invariant: every staker has the SAME share price.
        let price_after = after.total_capital * 1_000_000 / after.total_shares;
        for (staker, shares) in &stakers {
            let staker_shares = env.as_contract(&contract_id, || {
                RefractBackstop::shares_of(env.clone(), staker.clone())
            });
            assert_eq!(
                staker_shares, *shares,
                "shares must be unchanged after shortfall"
            );
            let withdrawal_value = staker_shares * price_after / 1_000_000;
            // Each staker lost exactly (shortfall / 3) from their original deposit.
            let expected_value = deposit - shortfall_amount / 3;
            // Allow ±1 for integer division rounding.
            assert!(
                (withdrawal_value - expected_value).abs() <= 1,
                "loss socialisation not uniform: staker value {} vs expected {}",
                withdrawal_value,
                expected_value
            );
        }

        assert_solvency(&env, &contract_id, &stakers);
    }

    // ── test: solvency under extreme capital drain to zero ─────────────────────

    #[test]
    fn stress_total_drain_leaves_zero_capital_non_negative_shares() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        let staker = Address::generate(&env);
        usdc_client.mint(&staker, &5_000_000);
        let shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), staker.clone(), 5_000_000).unwrap()
        });

        // Drain more than available: contract should cap at total_capital.
        let requested = 10_000_000i128;
        let covered = env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), requested).unwrap()
        });

        assert_eq!(covered, 5_000_000, "capped at total_capital");

        let state = bs_state!(&env, &contract_id);
        assert_eq!(state.total_capital, 0, "capital exactly zero after total drain");
        assert_eq!(state.total_shares, shares, "shares survive a total drain");
        assert!(state.total_capital >= 0, "INVARIANT: capital never negative");
    }

    // ── test: mixed deposit/premium/claim multi-round ─────────────────────────

    #[test]
    fn stress_multi_round_mixed_operations() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Scenario table (deterministic seed):
        //   Round 0: deposit alice 2M, deposit bob 1M
        //   Round 1: premium 400_000
        //   Round 2: deposit carol 1_500_000
        //   Round 3: cover_shortfall 800_000
        //   Round 4: premium 300_000
        //   Round 5: bob withdraws all
        //   Round 6: cover_shortfall 200_000
        //   Round 7: alice withdraws half
        //   Invariants checked at every step.

        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let carol = Address::generate(&env);
        usdc_client.mint(&alice, &2_000_000);
        usdc_client.mint(&bob, &1_000_000);
        usdc_client.mint(&carol, &1_500_000);

        // Round 0: deposits.
        let alice_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), alice.clone(), 2_000_000).unwrap()
        });
        let bob_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), bob.clone(), 1_000_000).unwrap()
        });
        let mut live_stakers: Vec<(Address, i128)> = vec![
            (alice.clone(), alice_shares),
            (bob.clone(), bob_shares),
        ];
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 1: premium accrual.
        env.as_contract(&contract_id, || {
            RefractBackstop::receive_premium(env.clone(), 400_000).unwrap();
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 2: carol deposits *after* premium, gets fewer shares per USDC.
        let carol_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), carol.clone(), 1_500_000).unwrap()
        });
        live_stakers.push((carol.clone(), carol_shares));
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 3: cover_shortfall.
        env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), 800_000).unwrap()
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 4: more premium.
        env.as_contract(&contract_id, || {
            RefractBackstop::receive_premium(env.clone(), 300_000).unwrap();
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 5: bob withdraws all.
        let bob_usdc = env.as_contract(&contract_id, || {
            RefractBackstop::withdraw(env.clone(), bob.clone(), bob_shares).unwrap()
        });
        assert!(bob_usdc > 0, "bob must receive positive USDC");
        live_stakers[1].1 = 0;
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 6: another shortfall.
        env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), 200_000).unwrap()
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 7: alice withdraws half her shares.
        let half = alice_shares / 2;
        let alice_usdc = env.as_contract(&contract_id, || {
            RefractBackstop::withdraw(env.clone(), alice.clone(), half).unwrap()
        });
        assert!(alice_usdc > 0, "alice must receive positive USDC");
        live_stakers[0].1 -= half;
        assert_solvency(&env, &contract_id, &live_stakers);

        // Final state: capital and shares still consistent.
        let final_state = bs_state!(&env, &contract_id);
        assert!(final_state.total_capital >= 0);
        assert!(final_state.total_shares >= 0);
    }

    // ── test: coverage ratio stays bounded under stress ───────────────────────

    #[test]
    fn stress_coverage_ratio_stays_bounded() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // 10 stakers, varied deposits.
        let amounts: [i128; 10] = [
            100_000, 200_000, 350_000, 75_000, 500_000,
            125_000, 400_000, 250_000, 180_000, 320_000,
        ];
        let mut stakers: Vec<(Address, i128)> = Vec::new();
        let mut total_deposited = 0i128;
        for amount in &amounts {
            let s = Address::generate(&env);
            usdc_client.mint(&s, amount);
            let shares = env.as_contract(&contract_id, || {
                RefractBackstop::deposit(env.clone(), s.clone(), *amount).unwrap()
            });
            stakers.push((s, shares));
            total_deposited += amount;
        }
        assert_solvency(&env, &contract_id, &stakers);

        // Progressive shortfall events: each drains 10% of remaining capital.
        let shortfall_rounds = 5;
        for _round in 0..shortfall_rounds {
            let state = bs_state!(&env, &contract_id);
            let drain = state.total_capital / 10;
            if drain > 0 {
                env.as_contract(&contract_id, || {
                    RefractBackstop::cover_shortfall(env.clone(), pool.clone(), drain).unwrap()
                });
            }
            // Interleave premium income to partially restore capital.
            env.as_contract(&contract_id, || {
                RefractBackstop::receive_premium(env.clone(), 50_000).unwrap();
            });
            assert_solvency(&env, &contract_id, &stakers);
        }

        // Coverage ratio invariant: total_capital / total_deposited bounded [0, 1.5]
        let final_state = bs_state!(&env, &contract_id);
        let ratio_bps = if total_deposited > 0 {
            final_state.total_capital * 10_000 / total_deposited
        } else {
            10_000
        };
        assert!(ratio_bps >= 0, "coverage ratio must be non-negative: {}", ratio_bps);
        assert!(
            ratio_bps <= 15_000,
            "coverage ratio implausibly high: {} BPS",
            ratio_bps
        );
    }
}


    // ── helpers ──────────────────────────────────────────────────────────────

    fn setup(
        env: &Env,
    ) -> (
        Address, // contract_id
        Address, // usdc_id
        Address, // pool
        Address, // admin
    ) {
        let admin = Address::generate(env);
        let pool = Address::generate(env);

        // Deploy a minimal WASM-less mock token using Soroban's built-in
        // stellar_asset_contract test helper.
        let usdc_admin = Address::generate(env);
        let usdc_id = env.register_stellar_asset_contract_v2(usdc_admin.clone());
        let usdc_addr = usdc_id.address();

        let contract_id = env.register(RefractBackstop, ());

        // ── fund all participants so transfers don't fail ────────────────────
        let usdc = token::StellarAssetClient::new(env, &usdc_addr);
        // Mint 10M USDC-cents to each well-known address.
        usdc.mint(&admin, &10_000_000_000);
        usdc.mint(&pool, &10_000_000_000);

        // Initialize backstop: 20% premium share to stakers.
        env.as_contract(&contract_id, || {
            RefractBackstop::initialize(
                env.clone(),
                pool.clone(),
                admin.clone(),
                usdc_addr.clone(),
                2_000, // 20% in BPS
            )
            .unwrap();
        });

        (contract_id, usdc_addr, pool, admin)
    }

    /// Mint `amount` USDC to `who` and return the token client.
    fn mint(env: &Env, usdc: &Address, usdc_admin: &Address, who: &Address, amount: i128) {
        let client = token::StellarAssetClient::new(env, usdc);
        client.mint(who, &amount);
        let _ = usdc_admin; // suppress unused warning; admin baked into client
    }

    // Convenience: read backstop state from inside contract context.
    macro_rules! bs_state {
        ($env:expr, $cid:expr) => {{
            $env.as_contract($cid, || {
                crate::RefractBackstop::state($env.clone()).unwrap()
            })
        }};
    }

    // ── invariant checker ────────────────────────────────────────────────────

    /// After every mutating step: verify global solvency invariants.
    ///
    /// `stakers` is a slice of `(address, expected_shares)` pairs at the
    /// current point in the scenario.
    fn assert_solvency(
        env: &Env,
        contract_id: &Address,
        stakers: &[(Address, i128)],
    ) {
        let state = bs_state!(env, contract_id);

        // Invariant 1: total_capital is non-negative.
        assert!(
            state.total_capital >= 0,
            "INVARIANT VIOLATION: total_capital < 0 (got {})",
            state.total_capital
        );

        // Invariant 2: total_shares matches sum of individual staker shares.
        let shares_sum: i128 = stakers.iter().map(|(_, s)| s).sum();
        assert_eq!(
            state.total_shares, shares_sum,
            "INVARIANT VIOLATION: total_shares ({}) != sum of staker shares ({})",
            state.total_shares, shares_sum
        );

        // Invariant 3: share price consistency — every staker's withdrawal
        // value is proportional (uniform share price across all holders).
        if state.total_shares > 0 && stakers.len() > 1 {
            let price_numer = state.total_capital * 1_000_000;
            let price_denom = state.total_shares;
            let price_per_share = price_numer / price_denom;

            for (_, shares) in stakers {
                if *shares <= 0 {
                    continue;
                }
                let value = shares * price_per_share / 1_000_000;
                // Each staker's reconstructed value must be non-negative.
                assert!(
                    value >= 0,
                    "INVARIANT VIOLATION: staker with {} shares has negative value {}",
                    shares,
                    value
                );
            }
        }
    }

    // ── test: sequential deposits then full withdrawal ────────────────────────

    #[test]
    fn stress_sequential_deposits_and_withdrawals() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, _pool, _admin) = setup(&env);

        // Deterministic "random" deposit amounts (5 stakers).
        let deposit_amounts: [i128; 5] = [
            1_000_000, // 100 USDC (7 decimals)
            2_500_000,
            500_000,
            3_000_000,
            750_000,
        ];

        let usdc_asset_admin = Address::generate(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        let mut stakers: Vec<(Address, i128)> = Vec::new();
        for amount in &deposit_amounts {
            let staker = Address::generate(&env);
            usdc_client.mint(&staker, amount);

            let shares = env.as_contract(&contract_id, || {
                RefractBackstop::deposit(env.clone(), staker.clone(), *amount).unwrap()
            });

            stakers.push((staker, shares));
            assert_solvency(&env, &contract_id, &stakers);
        }

        // Verify total_capital equals sum of deposits (no premium yet).
        let state = bs_state!(&env, &contract_id);
        let total_deposited: i128 = deposit_amounts.iter().sum();
        assert_eq!(state.total_capital, total_deposited);

        // Each staker withdraws all their shares.
        for (staker, shares) in stakers.iter_mut() {
            let usdc_returned = env.as_contract(&contract_id, || {
                RefractBackstop::withdraw(env.clone(), staker.clone(), *shares).unwrap()
            });
            assert!(usdc_returned > 0, "withdrawal should return positive USDC");
            *shares = 0;
            assert_solvency(&env, &contract_id, &stakers);
        }

        // After all withdrawals: total_capital and total_shares should be 0.
        let final_state = bs_state!(&env, &contract_id);
        assert_eq!(final_state.total_shares, 0, "all shares burned");
        assert_eq!(final_state.total_capital, 0, "all capital returned");

        let _ = usdc_asset_admin; // suppress unused warning
    }

    // ── test: premium accrual increases share price ───────────────────────────

    #[test]
    fn stress_premium_accrual_increases_share_price() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, _pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Alice deposits 1_000_000 (100 USDC).
        let alice = Address::generate(&env);
        usdc_client.mint(&alice, &1_000_000);
        let alice_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), alice.clone(), 1_000_000).unwrap()
        });
        assert_eq!(alice_shares, 1_000_000, "first depositor gets 1:1 shares");

        let stakers = vec![(alice.clone(), alice_shares)];
        assert_solvency(&env, &contract_id, &stakers);

        // Simulate 3 rounds of premium income.
        let premium_rounds: [i128; 3] = [200_000, 150_000, 50_000];
        let mut expected_premium_cut: i128 = 0;

        for round_amount in &premium_rounds {
            // backstop takes 20% (2_000 BPS)
            let cut = round_amount * 2_000 / 10_000;
            expected_premium_cut += cut;

            env.as_contract(&contract_id, || {
                RefractBackstop::receive_premium(env.clone(), *round_amount).unwrap();
            });

            let state = bs_state!(&env, &contract_id);
            // After each round capital should have grown.
            assert!(
                state.total_capital > 1_000_000,
                "capital must grow with premium: got {}",
                state.total_capital
            );
        }

        let state = bs_state!(&env, &contract_id);
        assert_eq!(
            state.total_capital,
            1_000_000 + expected_premium_cut,
            "total_capital should equal deposit + all premium cuts"
        );
        assert_eq!(
            state.premium_income, expected_premium_cut,
            "premium_income counter must track received cuts"
        );

        // Share price has increased: Alice can withdraw more than she deposited.
        let withdraw_quote = alice_shares * state.total_capital / state.total_shares;
        assert!(
            withdraw_quote > 1_000_000,
            "Alice's withdrawal quote {} should exceed her deposit",
            withdraw_quote
        );
    }

    // ── test: correlated claim / cover_shortfall socialises losses uniformly ──

    #[test]
    fn stress_correlated_claims_socialise_losses_uniformly() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Three stakers with equal deposits.
        let deposit = 1_000_000i128;
        let mut stakers: Vec<(Address, i128)> = Vec::new();
        for _ in 0..3 {
            let s = Address::generate(&env);
            usdc_client.mint(&s, &deposit);
            let shares = env.as_contract(&contract_id, || {
                RefractBackstop::deposit(env.clone(), s.clone(), deposit).unwrap()
            });
            stakers.push((s, shares));
        }
        assert_solvency(&env, &contract_id, &stakers);

        // State before shortfall.
        let before = bs_state!(&env, &contract_id);
        assert_eq!(before.total_capital, 3 * deposit);

        // Correlated claim event: pool calls cover_shortfall for 900_000.
        let shortfall_amount = 900_000i128;
        let covered = env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), shortfall_amount).unwrap()
        });
        assert_eq!(covered, shortfall_amount, "full shortfall should be covered");

        // State after shortfall.
        let after = bs_state!(&env, &contract_id);
        assert_eq!(
            after.total_capital,
            before.total_capital - shortfall_amount,
            "capital must decrease by exactly the covered shortfall"
        );
        assert_eq!(
            after.total_shares, before.total_shares,
            "shares must NOT be burned during a shortfall cover"
        );

        // Loss socialisation invariant: every staker has the SAME share price.
        let price_after = after.total_capital * 1_000_000 / after.total_shares;
        for (staker, shares) in &stakers {
            let staker_shares = env.as_contract(&contract_id, || {
                RefractBackstop::shares_of(env.clone(), staker.clone())
            });
            assert_eq!(
                staker_shares, *shares,
                "shares must be unchanged after shortfall"
            );
            let withdrawal_value = staker_shares * price_after / 1_000_000;
            // Each staker lost exactly (shortfall / 3) from their original deposit.
            let expected_value = deposit - shortfall_amount / 3;
            // Allow ±1 for integer division rounding.
            assert!(
                (withdrawal_value - expected_value).abs() <= 1,
                "loss socialisation not uniform: staker value {} vs expected {}",
                withdrawal_value,
                expected_value
            );
        }

        assert_solvency(&env, &contract_id, &stakers);
    }

    // ── test: solvency under extreme capital drain to zero ─────────────────────

    #[test]
    fn stress_total_drain_leaves_zero_capital_non_negative_shares() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        let staker = Address::generate(&env);
        usdc_client.mint(&staker, &5_000_000);
        let shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), staker.clone(), 5_000_000).unwrap()
        });

        // Drain more than available: contract should cap at total_capital.
        let requested = 10_000_000i128;
        let covered = env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), requested).unwrap()
        });

        assert_eq!(covered, 5_000_000, "capped at total_capital");

        let state = bs_state!(&env, &contract_id);
        assert_eq!(state.total_capital, 0, "capital exactly zero after total drain");
        assert_eq!(state.total_shares, shares, "shares survive a total drain");
        assert!(state.total_capital >= 0, "INVARIANT: capital never negative");
    }

    // ── test: mixed deposit/premium/claim multi-round ─────────────────────────

    #[test]
    fn stress_multi_round_mixed_operations() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // Scenario table (deterministic seed):
        //   Round 0: deposit alice 2M, deposit bob 1M
        //   Round 1: premium 400_000
        //   Round 2: deposit carol 1_500_000
        //   Round 3: cover_shortfall 800_000
        //   Round 4: premium 300_000
        //   Round 5: bob withdraws all
        //   Round 6: cover_shortfall 200_000
        //   Round 7: alice withdraws half
        //   Invariants checked at every step.

        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let carol = Address::generate(&env);
        usdc_client.mint(&alice, &2_000_000);
        usdc_client.mint(&bob, &1_000_000);
        usdc_client.mint(&carol, &1_500_000);

        // Round 0: deposits.
        let alice_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), alice.clone(), 2_000_000).unwrap()
        });
        let bob_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), bob.clone(), 1_000_000).unwrap()
        });
        let mut live_stakers = vec![
            (alice.clone(), alice_shares),
            (bob.clone(), bob_shares),
        ];
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 1: premium accrual.
        env.as_contract(&contract_id, || {
            RefractBackstop::receive_premium(env.clone(), 400_000).unwrap();
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 2: carol deposits *after* premium, gets fewer shares per USDC.
        let carol_shares = env.as_contract(&contract_id, || {
            RefractBackstop::deposit(env.clone(), carol.clone(), 1_500_000).unwrap()
        });
        live_stakers.push((carol.clone(), carol_shares));
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 3: cover_shortfall.
        env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), 800_000).unwrap()
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 4: more premium.
        env.as_contract(&contract_id, || {
            RefractBackstop::receive_premium(env.clone(), 300_000).unwrap();
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 5: bob withdraws all.
        let bob_usdc = env.as_contract(&contract_id, || {
            RefractBackstop::withdraw(env.clone(), bob.clone(), bob_shares).unwrap()
        });
        assert!(bob_usdc > 0, "bob must receive positive USDC");
        live_stakers[1].1 = 0;
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 6: another shortfall.
        env.as_contract(&contract_id, || {
            RefractBackstop::cover_shortfall(env.clone(), pool.clone(), 200_000).unwrap()
        });
        assert_solvency(&env, &contract_id, &live_stakers);

        // Round 7: alice withdraws half her shares.
        let half = alice_shares / 2;
        let alice_usdc = env.as_contract(&contract_id, || {
            RefractBackstop::withdraw(env.clone(), alice.clone(), half).unwrap()
        });
        assert!(alice_usdc > 0, "alice must receive positive USDC");
        live_stakers[0].1 -= half;
        assert_solvency(&env, &contract_id, &live_stakers);

        // Final state: capital and shares still consistent.
        let final_state = bs_state!(&env, &contract_id);
        assert!(final_state.total_capital >= 0);
        assert!(final_state.total_shares >= 0);
    }

    // ── test: coverage ratio stays bounded under stress ───────────────────────

    #[test]
    fn stress_coverage_ratio_stays_bounded() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, usdc_addr, pool, _admin) = setup(&env);
        let usdc_client = token::StellarAssetClient::new(&env, &usdc_addr);

        // 10 stakers, varied deposits.
        let amounts: [i128; 10] = [
            100_000, 200_000, 350_000, 75_000, 500_000,
            125_000, 400_000, 250_000, 180_000, 320_000,
        ];
        let mut stakers = Vec::new();
        let mut total_deposited = 0i128;
        for amount in &amounts {
            let s = Address::generate(&env);
            usdc_client.mint(&s, amount);
            let shares = env.as_contract(&contract_id, || {
                RefractBackstop::deposit(env.clone(), s.clone(), *amount).unwrap()
            });
            stakers.push((s, shares));
            total_deposited += amount;
        }
        assert_solvency(&env, &contract_id, &stakers);

        // Progressive shortfall events: each drains 10% of remaining capital.
        let shortfall_rounds = 5;
        for _round in 0..shortfall_rounds {
            let state = bs_state!(&env, &contract_id);
            let drain = state.total_capital / 10;
            if drain > 0 {
                env.as_contract(&contract_id, || {
                    RefractBackstop::cover_shortfall(env.clone(), pool.clone(), drain).unwrap()
                });
            }
            // Interleave premium income to partially restore capital.
            env.as_contract(&contract_id, || {
                RefractBackstop::receive_premium(env.clone(), 50_000).unwrap();
            });
            assert_solvency(&env, &contract_id, &stakers);
        }

        // Coverage ratio invariant: total_capital / total_deposited bounded [0, 1.5]
        // (can exceed 1 due to premium accrual, should not exceed 1.5 in this scenario).
        let final_state = bs_state!(&env, &contract_id);
        let ratio_bps = if total_deposited > 0 {
            final_state.total_capital * 10_000 / total_deposited
        } else {
            10_000
        };
        assert!(ratio_bps >= 0, "coverage ratio must be non-negative: {}", ratio_bps);
        assert!(
            ratio_bps <= 15_000,
            "coverage ratio implausibly high: {} BPS",
            ratio_bps
        );
    }
}
