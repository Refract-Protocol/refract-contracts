#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{vec, Address, BytesN, Env};

const CRIT: i128 = 50_000_0000000;
const HIGH: i128 = 10_000_0000000;
const MED: i128 = 2_000_0000000;
const LOW: i128 = 500_0000000;

struct Setup<'a> {
    env: Env,
    client: RefractBountyClient<'a>,
    token: TokenClient<'a>,
    governance: Address,
    approvers: [Address; 3],
    reporter: Address,
}

fn tiers() -> RewardTiers {
    RewardTiers {
        critical: CRIT,
        high: HIGH,
        medium: MED,
        low: LOW,
    }
}

/// 2-of-3 approver set, escrow funded with `fund` tokens.
fn setup<'a>(fund: i128) -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let governance = Address::generate(&env);
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = TokenClient::new(&env, &sac.address());
    let id = env.register_contract(None, RefractBounty);
    let client = RefractBountyClient::new(&env, &id);
    let approvers = [
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    ];
    client.initialize(
        &governance,
        &sac.address(),
        &vec![
            &env,
            approvers[0].clone(),
            approvers[1].clone(),
            approvers[2].clone(),
        ],
        &2,
        &tiers(),
    );
    let funder = Address::generate(&env);
    if fund > 0 {
        StellarAssetClient::new(&env, &sac.address()).mint(&funder, &fund);
        client.fund(&funder, &fund);
    }
    let reporter = Address::generate(&env);
    Setup {
        env,
        client,
        token,
        governance,
        approvers,
        reporter,
    }
}

fn hash(env: &Env, n: u8) -> BytesN<32> {
    BytesN::from_array(env, &[n; 32])
}

fn advance(env: &Env, ledgers: u32) {
    let seq = env.ledger().sequence();
    env.ledger().set_sequence_number(seq + ledgers);
}

fn approve_n(s: &Setup, h: &BytesN<32>, n: usize) {
    for a in s.approvers.iter().take(n) {
        s.client.approve_advisory(a, h);
    }
}

fn status(s: &Setup, h: &BytesN<32>) -> AdvisoryStatus {
    s.client.get_advisory(h).unwrap().status
}

// ── Initialization ──────────────────────────────────────────────────────────

#[test]
fn init_rejects_bad_threshold_and_duplicates() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractBounty);
    let c = RefractBountyClient::new(&env, &id);
    let g = Address::generate(&env);
    let t = Address::generate(&env);
    let a = Address::generate(&env);
    let b = Address::generate(&env);
    assert_eq!(
        c.try_initialize(&g, &t, &vec![&env, a.clone(), b.clone()], &3, &tiers()),
        Err(Ok(BountyError::InvalidThreshold))
    );
    assert_eq!(
        c.try_initialize(&g, &t, &vec![&env, a.clone(), b.clone()], &0, &tiers()),
        Err(Ok(BountyError::InvalidThreshold))
    );
    assert_eq!(
        c.try_initialize(&g, &t, &vec![&env, a.clone(), a.clone()], &1, &tiers()),
        Err(Ok(BountyError::InvalidApproverSet))
    );
    c.initialize(&g, &t, &vec![&env, a, b], &2, &tiers());
    assert_eq!(
        c.try_initialize(&g, &t, &vec![&env, g.clone()], &1, &tiers()),
        Err(Ok(BountyError::AlreadyInitialized))
    );
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

#[test]
fn submit_approve_pay_happy_path() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::High);
    assert_eq!(status(&s, &h), AdvisoryStatus::Submitted);

    approve_n(&s, &h, 2);
    assert_eq!(status(&s, &h), AdvisoryStatus::Approved);
    assert_eq!(s.client.reserved(), HIGH);

    // Not payable while the dispute window is open.
    assert_eq!(
        s.client.try_pay_advisory(&h),
        Err(Ok(BountyError::DisputeWindowOpen))
    );
    advance(&s.env, DEFAULT_DISPUTE_WINDOW_LEDGERS + 1);
    assert_eq!(s.client.pay_advisory(&h), HIGH);
    assert_eq!(status(&s, &h), AdvisoryStatus::Paid);
    assert_eq!(s.token.balance(&s.reporter), HIGH);
    assert_eq!(s.client.reserved(), 0);
    assert_eq!(s.client.balance(), 100_000_0000000 - HIGH);
}

#[test]
fn duplicate_report_rejected() {
    let s = setup(0);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Low);
    assert_eq!(
        s.client
            .try_submit_advisory(&s.reporter, &h, &Severity::Low),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn duplicate_approval_not_double_counted() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Medium);
    s.client.approve_advisory(&s.approvers[0], &h);
    assert_eq!(
        s.client.try_approve_advisory(&s.approvers[0], &h),
        Err(Ok(BountyError::AlreadyApproved))
    );
    let adv = s.client.get_advisory(&h).unwrap();
    assert_eq!(adv.approvals.len(), 1);
    assert_eq!(adv.status, AdvisoryStatus::Submitted);
}

#[test]
fn n_minus_one_approvals_is_not_enough() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client
        .submit_advisory(&s.reporter, &h, &Severity::Critical);
    approve_n(&s, &h, 1);
    assert_eq!(status(&s, &h), AdvisoryStatus::Submitted);
    assert_eq!(
        s.client.try_pay_advisory(&h),
        Err(Ok(BountyError::WrongStatus))
    );
    assert_eq!(s.client.reserved(), 0);
}

#[test]
fn non_approver_and_reporter_cannot_approve() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Low);
    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.client.try_approve_advisory(&outsider, &h),
        Err(Ok(BountyError::NotApprover))
    );

    // An approver who is also the reporter of a report cannot approve it.
    let h2 = hash(&s.env, 2);
    s.client
        .submit_advisory(&s.approvers[0], &h2, &Severity::Low);
    assert_eq!(
        s.client.try_approve_advisory(&s.approvers[0], &h2),
        Err(Ok(BountyError::SelfApproval))
    );
}

#[test]
fn approval_requires_backed_escrow() {
    let s = setup(HIGH - 1);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::High);
    s.client.approve_advisory(&s.approvers[0], &h);
    assert_eq!(
        s.client.try_approve_advisory(&s.approvers[1], &h),
        Err(Ok(BountyError::InsufficientBalance))
    );
}

// ── Disputes ────────────────────────────────────────────────────────────────

#[test]
fn dispute_then_governance_rescore_and_pay() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Medium);
    approve_n(&s, &h, 2);
    s.client.dispute_advisory(&s.reporter, &h);
    assert_eq!(status(&s, &h), AdvisoryStatus::Disputed);

    // Disputed is never payable.
    advance(&s.env, DEFAULT_DISPUTE_WINDOW_LEDGERS + 1);
    assert_eq!(
        s.client.try_pay_advisory(&h),
        Err(Ok(BountyError::WrongStatus))
    );

    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.client
            .try_resolve_dispute(&outsider, &h, &Severity::High, &true),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(
        s.client
            .resolve_dispute(&s.governance, &h, &Severity::High, &true),
        HIGH
    );
    let adv = s.client.get_advisory(&h).unwrap();
    assert_eq!(adv.status, AdvisoryStatus::Paid);
    assert_eq!(adv.severity, Severity::High);
    assert_eq!(s.token.balance(&s.reporter), HIGH);
    assert_eq!(s.client.reserved(), 0);
}

#[test]
fn dispute_rejected_releases_reservation() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client
        .submit_advisory(&s.reporter, &h, &Severity::Critical);
    approve_n(&s, &h, 2);
    s.client.dispute_advisory(&s.reporter, &h);
    assert_eq!(
        s.client
            .resolve_dispute(&s.governance, &h, &Severity::Low, &false),
        0
    );
    assert_eq!(status(&s, &h), AdvisoryStatus::Rejected);
    assert_eq!(s.client.reserved(), 0);
    assert_eq!(s.client.balance(), 100_000_0000000);
}

#[test]
fn approval_after_dispute_fails() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Low);
    approve_n(&s, &h, 2);
    s.client.dispute_advisory(&s.reporter, &h);
    assert_eq!(
        s.client.try_approve_advisory(&s.approvers[2], &h),
        Err(Ok(BountyError::WrongStatus))
    );
}

#[test]
fn dispute_after_payout_fails() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Low);
    approve_n(&s, &h, 2);
    advance(&s.env, DEFAULT_DISPUTE_WINDOW_LEDGERS + 1);
    s.client.pay_advisory(&h);
    assert_eq!(
        s.client.try_dispute_advisory(&s.reporter, &h),
        Err(Ok(BountyError::WrongStatus))
    );
}

#[test]
fn dispute_outside_window_or_by_other_fails() {
    let s = setup(100_000_0000000);
    let h = hash(&s.env, 1);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Low);
    // Cannot dispute before approval.
    assert_eq!(
        s.client.try_dispute_advisory(&s.reporter, &h),
        Err(Ok(BountyError::WrongStatus))
    );
    approve_n(&s, &h, 2);
    let other = Address::generate(&s.env);
    assert_eq!(
        s.client.try_dispute_advisory(&other, &h),
        Err(Ok(BountyError::Unauthorized))
    );
    advance(&s.env, DEFAULT_DISPUTE_WINDOW_LEDGERS + 1);
    assert_eq!(
        s.client.try_dispute_advisory(&s.reporter, &h),
        Err(Ok(BountyError::DisputeWindowExpired))
    );
}

// ── Config ──────────────────────────────────────────────────────────────────

#[test]
fn governance_manages_approvers_and_tiers() {
    let s = setup(100_000_0000000);
    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.client.try_set_reward_tiers(&outsider, &tiers()),
        Err(Ok(BountyError::Unauthorized))
    );
    let new_approver = Address::generate(&s.env);
    s.client
        .set_approvers(&s.governance, &vec![&s.env, new_approver.clone()], &1);
    let h = hash(&s.env, 9);
    s.client.submit_advisory(&s.reporter, &h, &Severity::Low);
    assert_eq!(
        s.client.try_approve_advisory(&s.approvers[0], &h),
        Err(Ok(BountyError::NotApprover))
    );
    s.client.approve_advisory(&new_approver, &h);
    assert_eq!(status(&s, &h), AdvisoryStatus::Approved);
}

// ── Invariant ───────────────────────────────────────────────────────────────

/// Escrow invariant: reserved == Σ reward of open (approved/disputed)
/// advisories, reserved <= balance, and balance == token balance held.
#[test]
fn escrow_invariant_holds_across_lifecycle() {
    let funded = 200_000_0000000;
    let s = setup(funded);
    let sevs = [
        Severity::Low,
        Severity::Medium,
        Severity::High,
        Severity::Critical,
    ];
    let hashes: std::vec::Vec<BytesN<32>> = (0..4u8).map(|i| hash(&s.env, i)).collect();

    let check = |s: &Setup| {
        let mut open = 0i128;
        for h in hashes.iter() {
            if let Some(a) = s.client.get_advisory(h) {
                if matches!(
                    a.status,
                    AdvisoryStatus::Approved | AdvisoryStatus::Disputed
                ) {
                    open += a.reserved;
                } else {
                    assert_eq!(a.reserved, 0);
                }
            }
        }
        assert_eq!(s.client.reserved(), open);
        assert!(s.client.reserved() <= s.client.balance());
        assert_eq!(s.client.balance(), s.token.balance(&s.client.address));
    };

    for (h, sev) in hashes.iter().zip(sevs.iter()) {
        s.client.submit_advisory(&s.reporter, h, sev);
        check(&s);
        approve_n(&s, h, 2);
        check(&s);
    }
    s.client.dispute_advisory(&s.reporter, &hashes[0]);
    s.client.dispute_advisory(&s.reporter, &hashes[1]);
    check(&s);
    advance(&s.env, DEFAULT_DISPUTE_WINDOW_LEDGERS + 1);
    s.client.pay_advisory(&hashes[2]);
    check(&s);
    s.client
        .resolve_dispute(&s.governance, &hashes[0], &Severity::Medium, &true);
    check(&s);
    s.client
        .resolve_dispute(&s.governance, &hashes[1], &Severity::Low, &false);
    check(&s);
    s.client.pay_advisory(&hashes[3]);
    check(&s);
    assert_eq!(s.client.balance(), funded - HIGH - MED - CRIT);
}

// ── Resource benchmarks (issue #221) ────────────────────────────────────────

fn measure<T>(env: &Env, ep: &str, f: impl FnOnce() -> T) -> T {
    env.budget().reset_default();
    let r = f();
    std::println!(
        "BUDGET refract_bounty {ep} {} {}",
        env.budget().cpu_instruction_cost(),
        env.budget().memory_bytes_cost()
    );
    r
}

/// One `BUDGET` line per state-changing entrypoint, read by
/// `cargo xtask budget-check`.
#[test]
fn budget_entrypoints() {
    let env = Env::default();
    env.mock_all_auths();
    let governance = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let id = env.register_contract(None, RefractBounty);
    let c = RefractBountyClient::new(&env, &id);
    let a0 = Address::generate(&env);
    let a1 = Address::generate(&env);
    let approvers = vec![&env, a0.clone(), a1.clone()];
    measure(&env, "initialize", || {
        c.initialize(&governance, &sac.address(), &approvers, &2, &tiers())
    });
    let funder = Address::generate(&env);
    StellarAssetClient::new(&env, &sac.address()).mint(&funder, &(CRIT * 4));
    measure(&env, "fund", || c.fund(&funder, &(CRIT * 4)));
    measure(&env, "set_reward_tiers", || {
        c.set_reward_tiers(&governance, &tiers())
    });
    measure(&env, "set_approvers", || {
        c.set_approvers(&governance, &approvers, &2)
    });
    measure(&env, "set_dispute_window", || {
        c.set_dispute_window(&governance, &DEFAULT_DISPUTE_WINDOW_LEDGERS)
    });

    let reporter = Address::generate(&env);
    let (h1, h2) = (hash(&env, 1), hash(&env, 2));
    measure(&env, "submit_advisory", || {
        c.submit_advisory(&reporter, &h1, &Severity::High)
    });
    c.submit_advisory(&reporter, &h2, &Severity::High);
    c.approve_advisory(&a0, &h1);
    // Measure the threshold-crossing approval (the more expensive branch).
    measure(&env, "approve_advisory", || c.approve_advisory(&a1, &h1));
    approve_both(&c, &a0, &a1, &h2);
    measure(&env, "dispute_advisory", || {
        c.dispute_advisory(&reporter, &h2)
    });
    advance(&env, DEFAULT_DISPUTE_WINDOW_LEDGERS + 1);
    measure(&env, "pay_advisory", || c.pay_advisory(&h1));
    measure(&env, "resolve_dispute", || {
        c.resolve_dispute(&governance, &h2, &Severity::Critical, &true)
    });
}

fn approve_both(c: &RefractBountyClient, a0: &Address, a1: &Address, h: &BytesN<32>) {
    c.approve_advisory(a0, h);
    c.approve_advisory(a1, h);
}

/// Worst case: the largest allowed approver set with an M-of-M threshold.
/// The final approval scans the full set and approval list.
#[test]
fn budget_worst_case_max_approvers() {
    let env = Env::default();
    env.mock_all_auths();
    let governance = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let id = env.register_contract(None, RefractBounty);
    let c = RefractBountyClient::new(&env, &id);
    let mut approvers = soroban_sdk::Vec::new(&env);
    for _ in 0..MAX_APPROVERS {
        approvers.push_back(Address::generate(&env));
    }
    c.initialize(
        &governance,
        &sac.address(),
        &approvers,
        &MAX_APPROVERS,
        &tiers(),
    );
    let funder = Address::generate(&env);
    StellarAssetClient::new(&env, &sac.address()).mint(&funder, &CRIT);
    c.fund(&funder, &CRIT);
    let reporter = Address::generate(&env);
    let h = hash(&env, 1);
    c.submit_advisory(&reporter, &h, &Severity::Critical);
    for i in 0..MAX_APPROVERS - 1 {
        c.approve_advisory(&approvers.get(i).unwrap(), &h);
    }
    let last = approvers.get(MAX_APPROVERS - 1).unwrap();
    measure(&env, "approve_advisory_worst_case", || {
        c.approve_advisory(&last, &h)
    });
    assert_eq!(status_of(&c, &h), AdvisoryStatus::Approved);
}

fn status_of(c: &RefractBountyClient, h: &BytesN<32>) -> AdvisoryStatus {
    c.get_advisory(h).unwrap().status
}
