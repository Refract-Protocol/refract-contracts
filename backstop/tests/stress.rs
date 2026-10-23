//! Scenario-based stress harness for backstop solvency under correlated
//! claims (issue #283).
//!
//! Each scenario drives the real `RefractBackstop` contract together with a
//! real Stellar Asset Contract token, a mock pool contract that settles claims
//! and forwards premiums, and a mock oracle. Time moves in explicit epochs
//! (one day each), advancing both ledger sequence and timestamp, so premium
//! flow, oracle staleness and reclaim timeouts are reproducible.
//!
//! After **every** step the harness checks:
//!
//! * **Conservation (exact).** `token.balance(backstop) == total_capital +
//!   liability`, and the sum of every account's balance equals everything
//!   ever minted.
//! * **Share accounting (exact).** `total_shares == Σ shares_of(staker)`.
//! * **Loss socialization.** Every staker's claim is
//!   `floor(shares * capital / total_shares)`, so all stakers carry the same
//!   loss per share. The quotes sum to at most `total_capital` and fall short
//!   by less than one unit per staker (rounding dust stays in the tranche).
//! * **Payout cap.** Each draw covers exactly `min(requested, capital)`.
//! * **Haircut.** Each withdrawal receives exactly the current quote. A
//!   staker's haircut equals their pro-rata share of the cumulative loss,
//!   within one unit of rounding.
//! * **Coverage ratio.** `capital * 1e7 / exposure` (protocol 1e7
//!   fixed-point, no floats) is reported and checked per scenario.
//!
//! Run with `cargo test -p refract-backstop --test stress -- --nocapture` to
//! see the scenario reports. CI runs exactly that command.

#![allow(clippy::disallowed_methods)]

use refract_backstop::{RefractBackstop, RefractBackstopClient, WithdrawResult};
use refract_common::payout::TransferOutcome;
use refract_common::registry::Role;
use refract_registry::{RefractRegistry, RefractRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, IssuerFlags, Ledger as _},
    token, Address, Env,
};
use std::fmt::Write as _;

/// Protocol fixed-point scale.
const SCALE: i128 = 10_000_000;
const EPOCH_SECS: u64 = 86_400;
const LEDGERS_PER_EPOCH: u32 = 17_280;
/// Backstop's cut of gross premium, in basis points.
const PREMIUM_SHARE_BPS: i128 = 2_000;

// ─── Mock oracle and pool ────────────────────────────────────────────────────

mod mocks {
    use refract_backstop::RefractBackstopClient;
    use soroban_sdk::auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation};
    use soroban_sdk::{
        contract, contractimpl, contracttype, token, vec, Address, Env, IntoVal, Symbol,
    };

    /// Price is in 1e7 fixed point; 1e7 == peg.
    pub const MAX_ORACLE_AGE: u64 = 2 * 86_400;
    pub const DEPEG_THRESHOLD: i128 = 9_700_000;

    #[contracttype]
    enum OKey {
        Price,
        UpdatedAt,
    }

    #[contract]
    pub struct MockOracle;

    #[contractimpl]
    impl MockOracle {
        pub fn set(env: Env, price: i128, updated_at: u64) {
            env.storage().instance().set(&OKey::Price, &price);
            env.storage().instance().set(&OKey::UpdatedAt, &updated_at);
        }
        pub fn read(env: Env) -> (i128, u64) {
            (
                env.storage()
                    .instance()
                    .get(&OKey::Price)
                    .unwrap_or(10_000_000),
                env.storage().instance().get(&OKey::UpdatedAt).unwrap_or(0),
            )
        }
    }

    #[contracttype]
    enum PKey {
        Token,
        Backstop,
        Oracle,
        Claimants,
    }

    /// How one claim was settled.
    #[contracttype]
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct ClaimOutcome {
        pub claim: i128,
        pub from_pool: i128,
        pub requested_from_backstop: i128,
        pub covered: i128,
        pub uncovered: i128,
    }

    /// First-loss pool stand-in: pays claims from its own balance first and
    /// draws the remainder from the backstop. Forwards premium only while the
    /// oracle is fresh.
    #[contract]
    pub struct MockPool;

    #[contractimpl]
    impl MockPool {
        pub fn init(
            env: Env,
            token: Address,
            backstop: Address,
            oracle: Address,
            claimants: Address,
        ) {
            env.storage().instance().set(&PKey::Token, &token);
            env.storage().instance().set(&PKey::Backstop, &backstop);
            env.storage().instance().set(&PKey::Oracle, &oracle);
            env.storage().instance().set(&PKey::Claimants, &claimants);
        }

        /// Forward `gross` premium; the backstop pulls its cut. Returns the
        /// cut, or 0 if the oracle is stale (premium flow halts).
        ///
        /// The backstop calls `token.transfer(pool, backstop, cut)` while
        /// handling this call, so the pool pre-authorizes exactly that nested
        /// transfer, as a production pool must.
        pub fn premium(env: Env, gross: i128, cut: i128) -> i128 {
            if Self::stale(&env) {
                return 0;
            }
            let token: Address = env.storage().instance().get(&PKey::Token).unwrap();
            let backstop: Address = env.storage().instance().get(&PKey::Backstop).unwrap();
            let me = env.current_contract_address();
            if cut > 0 {
                env.authorize_as_current_contract(vec![
                    &env,
                    InvokerContractAuthEntry::Contract(SubContractInvocation {
                        context: ContractContext {
                            contract: token,
                            fn_name: Symbol::new(&env, "transfer"),
                            args: (me.clone(), backstop.clone(), cut).into_val(&env),
                        },
                        sub_invocations: vec![&env],
                    }),
                ]);
            }
            RefractBackstopClient::new(&env, &backstop).receive_premium(&me, &gross)
        }

        pub fn claim(env: Env, amount: i128) -> ClaimOutcome {
            let token: Address = env.storage().instance().get(&PKey::Token).unwrap();
            let backstop: Address = env.storage().instance().get(&PKey::Backstop).unwrap();
            let claimants: Address = env.storage().instance().get(&PKey::Claimants).unwrap();
            let me = env.current_contract_address();
            let tc = token::Client::new(&env, &token);

            let from_pool = amount.min(tc.balance(&me));
            let shortfall = amount - from_pool;
            let covered = if shortfall > 0 {
                RefractBackstopClient::new(&env, &backstop).cover_shortfall(&me, &shortfall)
            } else {
                0
            };
            let paid = from_pool + covered;
            if paid > 0 {
                tc.transfer(&me, &claimants, &paid);
            }
            ClaimOutcome {
                claim: amount,
                from_pool,
                requested_from_backstop: shortfall,
                covered,
                uncovered: amount - paid,
            }
        }

        /// Depeg-triggered claims: `exposure * (peg - price) / peg` once the
        /// price is below the trigger, read from the oracle.
        pub fn depeg_claim(env: Env, exposure: i128) -> ClaimOutcome {
            let oracle: Address = env.storage().instance().get(&PKey::Oracle).unwrap();
            let (price, _) = MockOracleClient::new(&env, &oracle).read();
            if price >= DEPEG_THRESHOLD {
                return ClaimOutcome::default();
            }
            let severity = exposure * (10_000_000 - price) / 10_000_000;
            Self::claim(env, severity)
        }

        fn stale(env: &Env) -> bool {
            let oracle: Address = env.storage().instance().get(&PKey::Oracle).unwrap();
            let (_, updated_at) = MockOracleClient::new(env, &oracle).read();
            env.ledger().timestamp() > updated_at + MAX_ORACLE_AGE
        }
    }
}

use mocks::{ClaimOutcome, MockOracle, MockOracleClient, MockPool, MockPoolClient};

// ─── Harness ─────────────────────────────────────────────────────────────────

struct Staker {
    name: &'static str,
    addr: Address,
    /// Total tokens deposited (cost basis).
    deposited: i128,
}

struct Harness {
    name: &'static str,
    env: Env,
    token: token::Client<'static>,
    sac: token::StellarAssetClient<'static>,
    bs: RefractBackstopClient<'static>,
    pool: MockPoolClient<'static>,
    oracle: MockOracleClient<'static>,
    claimants: Address,
    stakers: Vec<Staker>,
    /// Insured exposure used for the coverage ratio.
    exposure: i128,
    epoch: u32,
    minted: i128,
    /// Every address that can hold tokens, for global conservation.
    holders: Vec<Address>,
    /// Losses absorbed by the backstop so far.
    cumulative_covered: i128,
    cumulative_uncovered: i128,
    /// Pending withdrawal requests `(staker index, shares)`, settled after
    /// the epoch's claims.
    queue: Vec<(usize, i128)>,
    report: String,
}

impl Harness {
    fn new(name: &'static str, exposure: i128) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| {
            l.timestamp = 1_000_000;
            l.sequence_number = 100;
            // Scenarios advance ~17k ledgers per epoch. None of the contracts
            // bump their own instance TTL yet (archival handling is tracked
            // separately), so give entries a TTL that outlives the run and
            // keep this harness about accounting.
            l.min_persistent_entry_ttl = 10_000_000;
            l.min_temp_entry_ttl = 10_000_000;
            l.max_entry_ttl = 20_000_000;
        });

        let admin = Address::generate(&env);
        let issuer = Address::generate(&env);
        let sac_contract = env.register_stellar_asset_contract_v2(issuer);
        sac_contract.issuer().set_flag(IssuerFlags::RevocableFlag);
        let token_id = sac_contract.address();

        let registry_id = env.register_contract(None, RefractRegistry);
        let registry = RefractRegistryClient::new(&env, &registry_id);
        registry.initialize(&admin, &u64::MAX);

        let oracle_id = env.register_contract(None, MockOracle);
        let pool_id = env.register_contract(None, MockPool);
        let bs_id = env.register_contract(None, RefractBackstop);
        let claimants = Address::generate(&env);

        registry.bootstrap_set(&Role::Pool, &pool_id);
        registry.bootstrap_set(&Role::Oracle, &oracle_id);

        let bs = RefractBackstopClient::new(&env, &bs_id);
        bs.initialize(&admin, &token_id, &registry_id, &PREMIUM_SHARE_BPS);
        let oracle = MockOracleClient::new(&env, &oracle_id);
        oracle.set(&SCALE, &1_000_000);
        let pool = MockPoolClient::new(&env, &pool_id);
        pool.init(&token_id, &bs_id, &oracle_id, &claimants);

        let token = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);
        let holders = vec![bs_id.clone(), pool_id.clone(), claimants.clone()];

        let mut h = Harness {
            name,
            env,
            token,
            sac,
            bs,
            pool,
            oracle,
            claimants,
            stakers: Vec::new(),
            exposure,
            epoch: 0,
            minted: 0,
            holders,
            cumulative_covered: 0,
            cumulative_uncovered: 0,
            queue: Vec::new(),
            report: String::new(),
        };
        let _ = writeln!(h.report, "\n=== scenario: {name} (exposure {exposure}) ===");
        let _ = writeln!(
            h.report,
            "{:>5} | {:<44} | {:>10} | {:>10} | {:>9} | {:>12} | {:>10}",
            "epoch", "step", "capital", "shares", "liability", "price/share", "coverage"
        );
        h
    }

    fn mint(&mut self, to: &Address, amount: i128) {
        self.sac.mint(to, &amount);
        self.minted += amount;
    }

    fn add_staker(&mut self, name: &'static str) -> usize {
        let addr = Address::generate(&self.env);
        self.holders.push(addr.clone());
        self.stakers.push(Staker {
            name,
            addr,
            deposited: 0,
        });
        self.stakers.len() - 1
    }

    fn deposit(&mut self, i: usize, amount: i128) -> i128 {
        let addr = self.stakers[i].addr.clone();
        self.mint(&addr, amount);
        let shares = self.bs.deposit(&addr, &amount);
        self.stakers[i].deposited += amount;
        let label = format!("{} deposits {}", self.stakers[i].name, amount);
        self.step(&label);
        shares
    }

    /// Fund the pool's own first-loss liquidity.
    fn fund_pool(&mut self, amount: i128) {
        let pool = self.pool.address.clone();
        self.mint(&pool, amount);
        self.step(&format!("pool liquidity +{amount}"));
    }

    fn advance_epoch(&mut self) {
        self.epoch += 1;
        self.env.ledger().with_mut(|l| {
            l.timestamp += EPOCH_SECS;
            l.sequence_number += LEDGERS_PER_EPOCH;
        });
    }

    /// Oracle publishes `price` now.
    fn oracle_update(&mut self, price: i128) {
        let now = self.env.ledger().timestamp();
        self.oracle.set(&price, &now);
        self.step(&format!("oracle price {price}"));
    }

    /// Pool forwards `gross` premium (minted to the pool first so that the
    /// pull is fully funded). Returns the backstop's cut (0 if stale).
    fn premium(&mut self, gross: i128) -> i128 {
        let pool = self.pool.address.clone();
        let cut = gross * PREMIUM_SHARE_BPS / 10_000;
        self.mint(&pool, cut);
        let before = self.bs.state().premium_income;
        let got = self.pool.premium(&gross, &cut);
        let after = self.bs.state().premium_income;
        assert_eq!(after - before, got, "premium income must match the cut");
        if got == 0 {
            // Stale oracle: nothing pulled, minted cut stays in the pool.
            self.step(&format!("premium {gross} HALTED (stale)"));
        } else {
            assert_eq!(got, cut);
            self.step(&format!("premium {gross} -> cut {got}"));
        }
        got
    }

    fn claim(&mut self, amount: i128) -> ClaimOutcome {
        let capital_before = self.bs.state().total_capital;
        let out = self.pool.claim(&amount);
        self.check_claim(&out, capital_before);
        self.step(&format!(
            "claim {amount}: pool {} bs {} unc {}",
            out.from_pool, out.covered, out.uncovered
        ));
        out
    }

    fn depeg_claim(&mut self) -> ClaimOutcome {
        let capital_before = self.bs.state().total_capital;
        let out = self.pool.depeg_claim(&self.exposure);
        self.check_claim(&out, capital_before);
        self.step(&format!(
            "depeg claim {}: bs {} unc {}",
            out.claim, out.covered, out.uncovered
        ));
        out
    }

    /// Payout-cap invariant for a single settled claim.
    fn check_claim(&mut self, out: &ClaimOutcome, capital_before: i128) {
        assert_eq!(
            out.covered,
            out.requested_from_backstop.min(capital_before),
            "[{}] backstop must cover exactly min(requested, capital)",
            self.name
        );
        assert!(out.covered <= capital_before);
        assert_eq!(out.from_pool + out.covered + out.uncovered, out.claim);
        assert_eq!(
            self.bs.state().total_capital,
            capital_before - out.covered,
            "[{}] capital must drop by exactly the covered amount",
            self.name
        );
        self.cumulative_covered += out.covered;
        self.cumulative_uncovered += out.uncovered;
    }

    /// Queue a withdrawal of `bps` of staker `i`'s current shares.
    fn request_withdraw(&mut self, i: usize, bps: i128) {
        let shares = self.bs.shares_of(&self.stakers[i].addr) * bps / 10_000;
        self.queue.push((i, shares));
    }

    /// Settle queued withdrawals in FIFO order. Returns `(staker, result)`.
    fn settle_queue(&mut self) -> Vec<(usize, WithdrawResult)> {
        let queue = std::mem::take(&mut self.queue);
        let mut out = Vec::new();
        for (i, shares) in queue {
            out.push((i, self.withdraw(i, shares)));
        }
        out
    }

    fn withdraw(&mut self, i: usize, shares: i128) -> WithdrawResult {
        let st = self.bs.state();
        let expected = shares * st.total_capital / st.total_shares;
        let addr = self.stakers[i].addr.clone();
        let bal_before = self.token.balance(&addr);
        let r = self.bs.withdraw(&addr, &shares);
        // Haircut invariant: paid exactly the current quote.
        assert_eq!(
            r.amount, expected,
            "[{}] withdrawal must equal quote",
            self.name
        );
        if r.outcome == TransferOutcome::Success {
            assert_eq!(self.token.balance(&addr) - bal_before, r.amount);
        } else {
            assert_eq!(self.token.balance(&addr), bal_before);
        }
        let label = format!(
            "{} withdraws {} sh -> {} ({:?})",
            self.stakers[i].name, shares, r.amount, r.outcome
        );
        self.step(&label);
        r
    }

    fn coverage_ratio(&self) -> i128 {
        if self.exposure == 0 {
            return 0;
        }
        self.bs.state().total_capital * SCALE / self.exposure
    }

    /// Record a report row and check every invariant.
    fn step(&mut self, label: &str) {
        let st = self.bs.state();
        let liability = self.bs.liability();
        let price = if st.total_shares == 0 {
            0
        } else {
            st.total_capital * SCALE / st.total_shares
        };
        let _ = writeln!(
            self.report,
            "{:>5} | {:<44} | {:>10} | {:>10} | {:>9} | {:>12} | {:>10}",
            self.epoch,
            label,
            st.total_capital,
            st.total_shares,
            liability,
            fmt_fixed(price),
            fmt_fixed(self.coverage_ratio())
        );
        self.check_invariants();
    }

    fn check_invariants(&self) {
        let st = self.bs.state();
        let liability = self.bs.liability();
        let bs_balance = self.token.balance(&self.bs.address);

        // 1. Backstop conservation (exact).
        assert_eq!(
            bs_balance,
            st.total_capital + liability,
            "[{}] backstop balance must equal capital + liability",
            self.name
        );
        assert!(st.total_capital >= 0 && st.total_shares >= 0 && liability >= 0);

        // 2. Global conservation (exact): nothing is created or destroyed.
        let held: i128 = self.holders.iter().map(|a| self.token.balance(a)).sum();
        assert_eq!(
            held, self.minted,
            "[{}] global token conservation",
            self.name
        );

        // 3. Share accounting (exact).
        let shares: i128 = self
            .stakers
            .iter()
            .map(|s| self.bs.shares_of(&s.addr))
            .sum();
        assert_eq!(shares, st.total_shares, "[{}] share sum", self.name);

        // 4. Loss socialization: identical price per share, quotes bounded
        //    by capital, rounding dust < 1 unit per staker.
        if st.total_shares > 0 {
            let mut quoted = 0i128;
            for s in &self.stakers {
                let sh = self.bs.shares_of(&s.addr);
                let q = self.bs.quote(&sh);
                assert_eq!(q, sh * st.total_capital / st.total_shares);
                quoted += q;
            }
            assert!(
                quoted <= st.total_capital,
                "[{}] quotes exceed capital",
                self.name
            );
            assert!(
                st.total_capital - quoted < self.stakers.len().max(1) as i128,
                "[{}] rounding dust too large",
                self.name
            );
        }
    }

    fn finish(self) {
        println!("{}", self.report);
        println!(
            "    covered by backstop: {} | uncovered: {} | final coverage: {}",
            self.cumulative_covered,
            self.cumulative_uncovered,
            fmt_fixed(self.coverage_ratio())
        );
    }
}

fn fmt_fixed(v: i128) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let v = v.abs();
    format!("{sign}{}.{:07}", v / SCALE, v % SCALE)
}

// ─── Scenarios ───────────────────────────────────────────────────────────────

/// No staker capital at all: draws cover nothing and the full shortfall is
/// reported as uncovered. Premium that arrives with no stakers is held as
/// capital without shares and goes to the first depositor (documented
/// behaviour, asserted so a change is noticed).
#[test]
fn scenario_zero_capital_operation() {
    let mut h = Harness::new("zero_capital_operation", 1_000_000);
    h.add_staker("alice");

    h.advance_epoch();
    let out = h.claim(50_000);
    assert_eq!(out.covered, 0);
    assert_eq!(out.uncovered, 50_000);

    h.advance_epoch();
    h.oracle_update(SCALE);
    let cut = h.premium(10_000);
    assert_eq!(cut, 2_000);
    assert_eq!(h.bs.state().total_shares, 0);

    // First depositor captures the orphaned premium.
    h.advance_epoch();
    h.deposit(0, 8_000);
    let alice = h.stakers[0].addr.clone();
    assert_eq!(h.bs.quote(&h.bs.shares_of(&alice)), 10_000);

    // Wipe out, then confirm new deposits are refused instead of diluted.
    h.advance_epoch();
    let out = h.claim(10_000);
    assert_eq!(out.covered, 10_000);
    assert_eq!(h.bs.state().total_capital, 0);
    let bob = h.add_staker("bob");
    let bob_addr = h.stakers[bob].addr.clone();
    h.mint(&bob_addr, 1_000);
    assert!(h.bs.try_deposit(&bob_addr, &1_000).is_err());
    h.step("bob deposit refused (insolvent tranche)");
    h.finish();
}

/// One maximum-severity claim that exhausts pool liquidity and the entire
/// backstop. Every staker loses 100%, pro rata.
#[test]
fn scenario_single_max_severity_claim() {
    let mut h = Harness::new("single_max_severity_claim", 500_000);
    for name in ["alice", "bob", "carol"] {
        h.add_staker(name);
    }
    h.deposit(0, 100_000);
    h.deposit(1, 50_000);
    h.deposit(2, 25_000);
    h.fund_pool(40_000);

    h.advance_epoch();
    let out = h.claim(h.exposure);
    assert_eq!(out.from_pool, 40_000);
    assert_eq!(out.covered, 175_000);
    assert_eq!(out.uncovered, 500_000 - 40_000 - 175_000);
    assert_eq!(h.bs.state().total_capital, 0);
    for s in &h.stakers {
        assert_eq!(h.bs.quote(&h.bs.shares_of(&s.addr)), 0);
    }
    assert_eq!(h.coverage_ratio(), 0);
    h.finish();
}

/// Several correlated claims in the same epoch. While they fit within
/// capital, order does not matter. Once capital runs out, claims are
/// first-come-first-served and the later ones go uncovered.
#[test]
fn scenario_correlated_claims_same_epoch() {
    let mut h = Harness::new("correlated_claims_same_epoch", 400_000);
    for name in ["alice", "bob"] {
        h.add_staker(name);
    }
    h.deposit(0, 60_000);
    h.deposit(1, 40_000);

    h.advance_epoch();
    h.oracle_update(SCALE);
    h.premium(50_000); // +10_000 capital

    h.advance_epoch();
    let claims = [30_000, 25_000, 35_000, 40_000];
    let mut covered = Vec::new();
    for c in claims {
        covered.push(h.claim(c).covered);
    }
    // 110_000 capital: first three fit (90_000), the fourth gets the last
    // 20_000.
    assert_eq!(covered, vec![30_000, 25_000, 35_000, 20_000]);
    assert_eq!(h.bs.state().total_capital, 0);
    assert_eq!(h.cumulative_uncovered, 20_000);

    // Same claims in reverse order: same total covered, different victims.
    let mut r = Harness::new("correlated_claims_reverse_order", 400_000);
    for name in ["alice", "bob"] {
        r.add_staker(name);
    }
    r.deposit(0, 60_000);
    r.deposit(1, 40_000);
    r.advance_epoch();
    r.oracle_update(SCALE);
    r.premium(50_000);
    r.advance_epoch();
    let mut rev = Vec::new();
    for c in claims.iter().rev() {
        rev.push(r.claim(*c).covered);
    }
    assert_eq!(rev, vec![40_000, 35_000, 25_000, 10_000]);
    assert_eq!(rev.iter().sum::<i128>(), covered.iter().sum::<i128>());
    h.finish();
    r.finish();
}

/// A depeg drives oracle-triggered claims, then the oracle goes stale and
/// premium flow stops. Capital must not grow while stale, and the books still
/// balance.
#[test]
fn scenario_depeg_then_oracle_staleness() {
    let mut h = Harness::new("depeg_then_oracle_staleness", 1_000_000);
    for name in ["alice", "bob", "carol"] {
        h.add_staker(name);
    }
    h.deposit(0, 100_000);
    h.deposit(1, 100_000);
    h.deposit(2, 50_000);

    h.advance_epoch();
    h.oracle_update(9_900_000); // 0.99: inside tolerance
    assert_eq!(h.depeg_claim().claim, 0);
    h.premium(20_000);

    h.advance_epoch();
    h.oracle_update(9_000_000); // 0.90: depeg
    let out = h.depeg_claim();
    assert_eq!(out.claim, 100_000);
    assert_eq!(out.covered, 100_000);

    // Oracle stops updating. Two epochs later it is stale.
    let capital_at_stale = {
        h.advance_epoch();
        h.advance_epoch();
        h.advance_epoch();
        h.bs.state().total_capital
    };
    for _ in 0..3 {
        assert_eq!(h.premium(20_000), 0);
        h.advance_epoch();
    }
    assert_eq!(h.bs.state().total_capital, capital_at_stale);

    // Feed recovers: premiums resume.
    h.oracle_update(SCALE);
    assert_eq!(h.premium(20_000), 4_000);
    h.finish();
}

/// Bank run: every staker withdraws in the same epoch. Each gets exactly the
/// current quote. There is no first-mover advantage beyond sub-unit rounding,
/// and the share price never drops for those who stay.
#[test]
fn scenario_simultaneous_withdrawals() {
    let mut h = Harness::new("simultaneous_withdrawals", 300_000);
    for name in ["alice", "bob", "carol", "dave"] {
        h.add_staker(name);
    }
    h.deposit(0, 33_333);
    h.deposit(1, 33_333);
    h.deposit(2, 33_334);
    h.deposit(3, 7);
    h.advance_epoch();
    h.claim(10_001); // uneven loss so rounding matters

    h.advance_epoch();
    for i in 0..4 {
        h.request_withdraw(i, 10_000);
    }
    let mut last_price = i128::MAX;
    let queue = std::mem::take(&mut h.queue);
    for (i, shares) in queue {
        let st = h.bs.state();
        let price_before = if st.total_shares == 0 {
            0
        } else {
            st.total_capital * SCALE / st.total_shares
        };
        assert!(price_before <= last_price || last_price == i128::MAX);
        h.withdraw(i, shares);
        let st = h.bs.state();
        if st.total_shares > 0 {
            let price_after = st.total_capital * SCALE / st.total_shares;
            assert!(
                price_after >= price_before,
                "price must not drop for remaining stakers"
            );
            last_price = price_after;
        }
    }
    let st = h.bs.state();
    assert_eq!(st.total_shares, 0);
    // Only rounding dust (< 1 unit per staker) is left behind.
    assert!(st.total_capital < 4);
    h.finish();
}

/// Deliberately under-capitalized: claims far exceed the tranche. Coverage
/// ratio starts below 1.0, the backstop is drained exactly, and the
/// remainder is reported uncovered rather than over-drawn.
#[test]
fn scenario_under_capitalized() {
    let mut h = Harness::new("under_capitalized", 2_000_000);
    h.add_staker("alice");
    h.add_staker("bob");
    h.deposit(0, 30_000);
    h.deposit(1, 20_000);
    assert!(h.coverage_ratio() < SCALE / 10);

    h.advance_epoch();
    let mut total_uncovered = 0;
    for c in [40_000, 40_000, 40_000] {
        total_uncovered += h.claim(c).uncovered;
    }
    assert_eq!(h.cumulative_covered, 50_000);
    assert_eq!(total_uncovered, 70_000);
    assert_eq!(h.bs.state().total_capital, 0);
    h.finish();
}

/// Withdrawals queued behind losses: requests made at the start of an epoch
/// settle after that epoch's claims, so they take the haircut. The haircut
/// equals each staker's pro-rata share of the loss.
#[test]
fn scenario_withdrawals_queued_behind_losses() {
    let mut h = Harness::new("withdrawals_queued_behind_losses", 500_000);
    for name in ["alice", "bob", "carol"] {
        h.add_staker(name);
    }
    h.deposit(0, 50_000);
    h.deposit(1, 30_000);
    h.deposit(2, 20_000);

    h.advance_epoch();
    h.request_withdraw(0, 10_000);
    h.request_withdraw(1, 5_000);
    // Claims land before the queue settles.
    h.claim(15_000);
    h.claim(5_000); // 20% total loss
    let results = h.settle_queue();

    let (i0, r0) = &results[0];
    assert_eq!(*i0, 0);
    // Alice deposited 50_000 and takes 20% of the loss.
    assert_eq!(r0.amount, 40_000);
    let haircut_alice = h.stakers[0].deposited - r0.amount;
    assert_eq!(haircut_alice, 10_000);

    let (_, r1) = &results[1];
    assert_eq!(r1.amount, 12_000); // half of bob's 30_000 * 0.8

    // Carol never withdrew; she carries exactly her 20% share.
    let carol = h.stakers[2].addr.clone();
    assert_eq!(h.bs.quote(&h.bs.shares_of(&carol)), 16_000);
    h.finish();
}

/// A frozen staker joins a bank run (issue #285 interplay). Their payout
/// becomes a liability that later losses cannot touch, conservation still
/// holds, and the payout succeeds once the issuer unfreezes.
#[test]
fn scenario_frozen_staker_during_run_then_losses() {
    let mut h = Harness::new("frozen_staker_during_run", 200_000);
    h.add_staker("alice");
    h.add_staker("frozen_bob");
    h.deposit(0, 50_000);
    h.deposit(1, 50_000);

    let bob = h.stakers[1].addr.clone();
    h.sac.set_authorized(&bob, &false);

    h.advance_epoch();
    let r = h.withdraw(1, h.bs.shares_of(&bob));
    assert_eq!(r.outcome, TransferOutcome::Frozen);
    assert_eq!(h.bs.liability(), 50_000);

    h.advance_epoch();
    let out = h.claim(80_000);
    assert_eq!(out.covered, 50_000, "liability must not be drawn");
    assert_eq!(h.bs.liability(), 50_000);

    h.sac.set_authorized(&bob, &true);
    let id = r.payout_id.unwrap();
    assert_eq!(h.bs.retry_payout(&id), TransferOutcome::Success);
    h.step("bob unfrozen, retry paid");
    assert_eq!(h.token.balance(&bob), 50_000);
    let _ = &h.claimants;
    h.finish();
}
