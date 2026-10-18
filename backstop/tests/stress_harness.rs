#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! Deterministic scenario-based stress testing harness for `RefractBackstop`
//! solvency, coverage ratios, loss socialization, haircuts, and payout caps
//! under correlated claims (#283).

use refract_backstop::{
    BackstopState, RefractBackstop, RefractBackstopClient, BPS_DENOM, PRECISION,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, Env,
};

/// Configuration for a deterministic backstop stress scenario.
#[derive(Clone, Debug)]
pub struct ScenarioConfig {
    pub name: &'static str,
    pub premium_share_bps: i128,
    pub max_coverage_ratio_bps: i128,
    pub oracle_max_staleness_secs: u64,
}

/// Captured metrics at the end of a scenario run for CI reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenarioReport {
    pub scenario_name: &'static str,
    pub total_deposited: i128,
    pub total_premium_credited: i128,
    pub total_requested_shortfall: i128,
    pub total_covered_shortfall: i128,
    pub total_unpaid_haircut: i128,
    pub total_withdrawn: i128,
    pub ending_capital: i128,
    pub ending_shares: i128,
    pub share_price_bps: i128,
}

impl ScenarioReport {
    pub fn format_row(&self) -> String {
        format!(
            "| {:<42} | {:>14} | {:>14} | {:>14} | {:>14} | {:>14} | {:>10} |",
            self.scenario_name,
            self.total_deposited,
            self.total_covered_shortfall,
            self.total_unpaid_haircut,
            self.total_withdrawn,
            self.ending_capital,
            self.share_price_bps
        )
    }
}

/// Deterministic test fixture driving `RefractBackstop`, token balances, simulated
/// pool coverage obligations, and oracle freshness.
pub struct StressHarness<'a> {
    pub env: Env,
    pub contract_id: Address,
    pub client: RefractBackstopClient<'a>,
    pub pool: Address,
    pub admin: Address,
    pub token_id: Address,
    pub token_client: token::Client<'a>,
    pub sac: token::StellarAssetClient<'a>,
    pub stakers: Vec<Address>,
    pub config: ScenarioConfig,
    pub total_deposited: i128,
    pub total_premium_credited: i128,
    pub total_requested_shortfall: i128,
    pub total_covered_shortfall: i128,
    pub total_unpaid_haircut: i128,
    pub total_withdrawn: i128,
    pub active_coverage: i128,
    pub oracle_last_updated: u64,
}

impl<'a> StressHarness<'a> {
    pub fn new(config: ScenarioConfig, num_stakers: usize) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_000;
            li.sequence_number = 100;
        });

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &config.premium_share_bps);

        let mut stakers = Vec::with_capacity(num_stakers);
        for _ in 0..num_stakers {
            stakers.push(Address::generate(&env));
        }

        let harness = Self {
            env,
            contract_id,
            client,
            pool,
            admin,
            token_id,
            token_client,
            sac,
            stakers,
            config,
            total_deposited: 0,
            total_premium_credited: 0,
            total_requested_shortfall: 0,
            total_covered_shortfall: 0,
            total_unpaid_haircut: 0,
            total_withdrawn: 0,
            active_coverage: 0,
            oracle_last_updated: 1_000,
        };
        harness.assert_invariants();
        harness
    }

    pub fn advance_ledger(&mut self, delta_secs: u64, delta_seq: u32, refresh_oracle: bool) {
        self.env.ledger().with_mut(|li| {
            li.timestamp += delta_secs;
            li.sequence_number += delta_seq;
        });
        if refresh_oracle {
            self.oracle_last_updated = self.env.ledger().timestamp();
        }
        self.assert_invariants();
    }

    pub fn is_oracle_stale(&self) -> bool {
        let now = self.env.ledger().timestamp();
        now.saturating_sub(self.oracle_last_updated) > self.config.oracle_max_staleness_secs
    }

    pub fn step_deposit(&mut self, staker_idx: usize, amount: i128) -> i128 {
        let staker = self.stakers[staker_idx].clone();
        self.sac.mint(&staker, &amount);
        let shares = self.client.deposit(&staker, &amount);
        self.total_deposited += amount;
        self.assert_invariants();
        shares
    }

    pub fn step_accrue_premium(&mut self, gross_premium: i128) -> i128 {
        let expected_cut = gross_premium * self.config.premium_share_bps / BPS_DENOM;
        if expected_cut > 0 {
            self.sac.mint(&self.contract_id, &expected_cut);
        }
        let credited = self.client.receive_premium(&gross_premium);
        assert_eq!(credited, expected_cut);
        self.total_premium_credited += credited;
        self.assert_invariants();
        credited
    }

    /// Simulates a claim shortfall against the backstop. If `require_fresh_oracle`
    /// is true and the oracle is stale, the claim is rejected before touching
    /// backstop capital. Otherwise `cover_shortfall` drains at most `total_capital`,
    /// capping the payout and recording any unpaid shortfall as a policy haircut.
    pub fn step_claim_shortfall(
        &mut self,
        requested_shortfall: i128,
        require_fresh_oracle: bool,
    ) -> Result<(i128, i128), &'static str> {
        if require_fresh_oracle && self.is_oracle_stale() {
            self.assert_invariants();
            return Err("oracle_stale_claim_halted");
        }
        let pre_state = self.client.state();
        let covered = self
            .client
            .cover_shortfall(&self.pool, &requested_shortfall);

        // Payout-cap invariant: covered never exceeds min(requested, pre_state.total_capital).
        assert_eq!(covered, requested_shortfall.min(pre_state.total_capital));
        let haircut = requested_shortfall - covered;

        self.total_requested_shortfall += requested_shortfall;
        self.total_covered_shortfall += covered;
        self.total_unpaid_haircut += haircut;

        self.assert_invariants();
        Ok((covered, haircut))
    }

    pub fn step_withdraw(&mut self, staker_idx: usize, shares: i128) -> i128 {
        let staker = self.stakers[staker_idx].clone();
        let out = self.client.withdraw(&staker, &shares);
        self.total_withdrawn += out;
        self.assert_invariants();
        out
    }

    /// Asserts all 5 core backstop invariants after every step:
    /// 1. Solvency (`state.total_capital >= 0` and on-chain token balance == `state.total_capital`).
    /// 2. Share & Accounting Conservation (`total_deposited + total_premium_credited == total_covered_shortfall + total_withdrawn + state.total_capital`).
    /// 3. Share Non-Negativity (`state.total_shares >= 0` and sum of staker shares == `state.total_shares`).
    /// 4. Loss Socialization & Haircut Bound (`total_covered_shortfall + total_unpaid_haircut == total_requested_shortfall`).
    /// 5. Coverage Ratio Bound (`active_coverage * BPS_DENOM <= state.total_capital * max_coverage_ratio_bps` whenever new coverage is written).
    pub fn assert_invariants(&self) {
        let state: BackstopState = self.client.state();
        let token_bal = self.token_client.balance(&self.contract_id);

        // INV-1: Solvency & physical custody match
        assert!(state.total_capital >= 0, "total_capital went negative");
        assert_eq!(
            token_bal, state.total_capital,
            "on-chain token balance diverged from BackstopState::total_capital"
        );

        // INV-2: Exact capital conservation
        assert_eq!(
            self.total_deposited + self.total_premium_credited,
            self.total_covered_shortfall + self.total_withdrawn + state.total_capital,
            "capital conservation violated"
        );

        // INV-3: Share sum conservation
        assert!(state.total_shares >= 0, "total_shares went negative");
        let staker_share_sum: i128 = self.stakers.iter().map(|s| self.client.shares_of(s)).sum();
        assert_eq!(
            staker_share_sum, state.total_shares,
            "staker shares sum diverged from total_shares"
        );

        // INV-4: Shortfall payout cap + haircut conservation
        assert_eq!(
            self.total_covered_shortfall + self.total_unpaid_haircut,
            self.total_requested_shortfall,
            "shortfall + haircut conservation violated"
        );

        // INV-5: Coverage ratio bound
        if self.active_coverage > 0 {
            assert!(
                self.active_coverage * BPS_DENOM
                    <= state.total_capital * self.config.max_coverage_ratio_bps
            );
        }
    }

    pub fn build_report(&self) -> ScenarioReport {
        let state = self.client.state();
        let share_price_bps = if state.total_shares == 0 {
            BPS_DENOM
        } else {
            state.total_capital * BPS_DENOM / state.total_shares
        };
        ScenarioReport {
            scenario_name: self.config.name,
            total_deposited: self.total_deposited,
            total_premium_credited: self.total_premium_credited,
            total_requested_shortfall: self.total_requested_shortfall,
            total_covered_shortfall: self.total_covered_shortfall,
            total_unpaid_haircut: self.total_unpaid_haircut,
            total_withdrawn: self.total_withdrawn,
            ending_capital: state.total_capital,
            ending_shares: state.total_shares,
            share_price_bps,
        }
    }
}

#[test]
fn scenario_1_zero_capital_operation() {
    let mut h = StressHarness::new(
        ScenarioConfig {
            name: "1. Zero-Capital Operation",
            premium_share_bps: 2_000,
            max_coverage_ratio_bps: 8_000,
            oracle_max_staleness_secs: 300,
        },
        2,
    );

    // Claim shortfall against a zero-capital backstop covers 0 and records 100% haircut without panicking.
    let (covered, haircut) = h.step_claim_shortfall(500 * PRECISION, false).unwrap();
    assert_eq!(covered, 0);
    assert_eq!(haircut, 500 * PRECISION);
    assert!(h
        .client
        .try_withdraw(&h.stakers[0], &(10 * PRECISION))
        .is_err());

    let report = h.build_report();
    println!("{}", report.format_row());
    assert_eq!(report.ending_capital, 0);
}

#[test]
fn scenario_2_single_maximum_severity_claim() {
    let mut h = StressHarness::new(
        ScenarioConfig {
            name: "2. Single Max-Severity Claim",
            premium_share_bps: 2_500,
            max_coverage_ratio_bps: 8_000,
            oracle_max_staleness_secs: 300,
        },
        2,
    );

    h.step_deposit(0, 1_000 * PRECISION);
    h.step_deposit(1, 1_000 * PRECISION);
    h.step_accrue_premium(400 * PRECISION); // +100 * PRECISION -> total_capital = 2_100 * PRECISION

    // Single catastrophic claim of 3,000 * PRECISION (exceeds total backstop capital of 2,100 * PRECISION).
    let (covered, haircut) = h.step_claim_shortfall(3_000 * PRECISION, false).unwrap();
    assert_eq!(covered, 2_100 * PRECISION);
    assert_eq!(haircut, 900 * PRECISION);

    let report = h.build_report();
    println!("{}", report.format_row());
    assert_eq!(report.ending_capital, 0);
    assert_eq!(report.share_price_bps, 0);
}

#[test]
fn scenario_3_correlated_claims_same_epoch_loss_socialization() {
    let mut h = StressHarness::new(
        ScenarioConfig {
            name: "3. Correlated Claims (Same Epoch)",
            premium_share_bps: 2_000,
            max_coverage_ratio_bps: 8_000,
            oracle_max_staleness_secs: 300,
        },
        3,
    );

    // Three stakers deposit 1,000, 2,000, and 3,000 USDC (total = 6,000 * PRECISION).
    h.step_deposit(0, 1_000 * PRECISION);
    h.step_deposit(1, 2_000 * PRECISION);
    h.step_deposit(2, 3_000 * PRECISION);

    // Three correlated claims arrive in the same ledger epoch (1,200 + 1,800 + 1,500 = 4,500 * PRECISION, 75% loss).
    let (c1, h1) = h.step_claim_shortfall(1_200 * PRECISION, true).unwrap();
    let (c2, h2) = h.step_claim_shortfall(1_800 * PRECISION, true).unwrap();
    let (c3, h3) = h.step_claim_shortfall(1_500 * PRECISION, true).unwrap();
    assert_eq!(c1 + c2 + c3, 4_500 * PRECISION);
    assert_eq!(h1 + h2 + h3, 0);

    let report = h.build_report();
    println!("{}", report.format_row());
    // Remaining capital is 1,500 * PRECISION across 6,000 * PRECISION shares -> 2,500 bps (25% of par).
    assert_eq!(report.ending_capital, 1_500 * PRECISION);
    assert_eq!(report.share_price_bps, 2_500);
}

#[test]
fn scenario_4_depeg_and_oracle_staleness_event() {
    let mut h = StressHarness::new(
        ScenarioConfig {
            name: "4. Depeg & Oracle-Staleness Event",
            premium_share_bps: 1_500,
            max_coverage_ratio_bps: 8_000,
            oracle_max_staleness_secs: 300,
        },
        2,
    );

    h.step_deposit(0, 2_000 * PRECISION);
    h.step_deposit(1, 2_000 * PRECISION);

    // First depeg wave while oracle is fresh: 1,000 * PRECISION covered.
    let (covered_1, _) = h.step_claim_shortfall(1_000 * PRECISION, true).unwrap();
    assert_eq!(covered_1, 1_000 * PRECISION);

    // Advance ledger by 600s without refreshing oracle -> oracle becomes stale (> 300s).
    h.advance_ledger(600, 120, false);
    assert!(h.is_oracle_stale());

    // Claim during oracle staleness is halted, preserving backstop capital at 3,000 * PRECISION.
    assert_eq!(
        h.step_claim_shortfall(2_500 * PRECISION, true),
        Err("oracle_stale_claim_halted")
    );
    assert_eq!(h.client.state().total_capital, 3_000 * PRECISION);

    // Oracle recovers -> second depeg claim executes cleanly.
    h.advance_ledger(10, 2, true);
    let (covered_2, _) = h.step_claim_shortfall(1_500 * PRECISION, true).unwrap();
    assert_eq!(covered_2, 1_500 * PRECISION);

    let report = h.build_report();
    println!("{}", report.format_row());
    assert_eq!(report.ending_capital, 1_500 * PRECISION);
}

#[test]
fn scenario_5_simultaneous_withdrawals_queued_behind_losses() {
    let mut h = StressHarness::new(
        ScenarioConfig {
            name: "5. Simultaneous Withdrawals Behind Losses",
            premium_share_bps: 2_000,
            max_coverage_ratio_bps: 8_000,
            oracle_max_staleness_secs: 300,
        },
        3,
    );

    // Three stakers deposit 1,000 * PRECISION each (total = 3,000 * PRECISION).
    let s0 = h.step_deposit(0, 1_000 * PRECISION);
    let s1 = h.step_deposit(1, 1_000 * PRECISION);
    let s2 = h.step_deposit(2, 1_000 * PRECISION);

    // Loss of 1,500 * PRECISION hits before withdrawals execute (50% socialized haircut).
    h.step_claim_shortfall(1_500 * PRECISION, true).unwrap();

    // All three stakers withdraw simultaneously behind the loss; each receives exactly 500 * PRECISION (50% haircut).
    let w0 = h.step_withdraw(0, s0);
    let w1 = h.step_withdraw(1, s1);
    let w2 = h.step_withdraw(2, s2);
    assert_eq!(w0, 500 * PRECISION);
    assert_eq!(w1, 500 * PRECISION);
    assert_eq!(w2, 500 * PRECISION);

    let report = h.build_report();
    println!("{}", report.format_row());
    assert_eq!(report.ending_capital, 0);
    assert_eq!(report.ending_shares, 0);
}
