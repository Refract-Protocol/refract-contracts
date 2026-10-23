use super::*;

const GOVERNANCE: Address = 1;
const RECIPIENT: Address = 2;
const STRANGER: Address = 3;
const CONTRACT: Address = 100;
const TREASURY: Address = 101;
const ASSET: Address = 500;
const FUNDER_A: Address = 10;
const FUNDER_B: Address = 11;
const FUNDER_C: Address = 12;

fn contract() -> RefractGrants {
    let mut grants = RefractGrants::new(GOVERNANCE, CONTRACT);
    grants
        .set_funding_policy(GOVERNANCE, FundingPolicy::Permissionless)
        .unwrap();
    grants.set_treasury(GOVERNANCE, TREASURY).unwrap();
    grants
}

fn ledger() -> MemoryLedger {
    let mut l = MemoryLedger::new();
    for who in [FUNDER_A, FUNDER_B, FUNDER_C, TREASURY, STRANGER] {
        l.mint(ASSET, who, 1_000_000);
    }
    l
}

/// Creates a grant and funds it in full from FUNDER_A.
fn funded_grant(
    grants: &mut RefractGrants,
    l: &mut MemoryLedger,
    total: i128,
    milestones: Vec<i128>,
) -> GrantId {
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, total, milestones)
        .unwrap();
    grants.co_fund(FUNDER_A, id, total, l).unwrap();
    id
}

fn held(grants: &RefractGrants, id: GrantId, who: Address) -> i128 {
    grants.funder_position(id, who).unwrap().held()
}

// ─── Existing behaviour ──────────────────────────────────────────────────

#[test]
fn full_lifecycle_create_confirm_claim() {
    let mut grants = contract();
    let mut l = ledger();
    let id = funded_grant(&mut grants, &mut l, 300, vec![100, 100, 100]);

    // Nothing claimable before any confirmation.
    assert_eq!(grants.claimable(id), Some(0));
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Err(GrantsError::NothingToClaim));

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    assert_eq!(grants.claimable(id), Some(100));
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(100));
    assert_eq!(grants.claimable(id), Some(0));

    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
    assert_eq!(grants.claimable(id), Some(200));
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(200));
    assert_eq!(grants.claimable(id), Some(0));
    assert_eq!(l.balance(ASSET, RECIPIENT), 300);
    assert_eq!(grants.escrow_balance(ASSET), 0);
    assert!(grants.escrow_matches_positions());
}

#[test]
fn claim_releases_only_confirmed_amounts() {
    let mut grants = contract();
    let mut l = ledger();
    let id = funded_grant(&mut grants, &mut l, 500, vec![200, 300]);

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    // Only the first milestone is confirmed, so only 200 is released.
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(200));
    assert_eq!(grants.claimable(id), Some(0));
}

#[test]
fn rejects_milestone_sum_mismatch() {
    let mut grants = contract();
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, ASSET, 300, vec![100, 100]),
        Err(GrantsError::MilestoneSumMismatch)
    );
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, ASSET, 300, vec![100, 100, 200]),
        Err(GrantsError::MilestoneSumMismatch)
    );
}

#[test]
fn rejects_zero_milestones() {
    let mut grants = contract();
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, ASSET, 100, vec![]),
        Err(GrantsError::NoMilestones)
    );
}

#[test]
fn rejects_non_positive_amounts() {
    let mut grants = contract();
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, ASSET, 0, vec![0]),
        Err(GrantsError::InvalidTotalAmount)
    );
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, ASSET, 100, vec![100, 0]),
        Err(GrantsError::InvalidMilestoneAmount)
    );
}

#[test]
fn rejects_out_of_order_confirmation() {
    let mut grants = contract();
    let mut l = ledger();
    let id = funded_grant(&mut grants, &mut l, 300, vec![100, 100, 100]);

    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 1),
        Err(GrantsError::MilestoneOutOfOrder)
    );
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 2),
        Err(GrantsError::MilestoneOutOfOrder)
    );

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 1),
        Err(GrantsError::MilestoneAlreadyConfirmed)
    );
}

#[test]
fn enforces_governance_gating() {
    let mut grants = contract();
    let mut l = ledger();
    assert_eq!(
        grants.create_grant(STRANGER, RECIPIENT, ASSET, 100, vec![100]),
        Err(GrantsError::Unauthorized)
    );

    let id = funded_grant(&mut grants, &mut l, 100, vec![100]);
    assert_eq!(
        grants.confirm_milestone(STRANGER, id, 0),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(grants.cancel_grant(STRANGER, id), Err(GrantsError::Unauthorized));
    assert_eq!(grants.clawback_grant(STRANGER, id), Err(GrantsError::Unauthorized));
    assert_eq!(
        grants.set_funding_policy(STRANGER, FundingPolicy::Allowlist),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        grants.set_funder_allowed(STRANGER, STRANGER, true),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(grants.set_treasury(STRANGER, STRANGER), Err(GrantsError::Unauthorized));
    assert_eq!(
        grants.credit_treasury_deposit(STRANGER, id, 1, &l),
        Err(GrantsError::Unauthorized)
    );
}

#[test]
fn claim_is_recipient_only() {
    let mut grants = contract();
    let mut l = ledger();
    let id = funded_grant(&mut grants, &mut l, 100, vec![100]);
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

    assert_eq!(grants.claim(STRANGER, id, &mut l), Err(GrantsError::NotRecipient));
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(100));
}

#[test]
fn rejects_unknown_grant_and_index() {
    let mut grants = contract();
    let mut l = ledger();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, 99, 0),
        Err(GrantsError::GrantNotFound)
    );
    assert_eq!(grants.claim(RECIPIENT, 99, &mut l), Err(GrantsError::GrantNotFound));

    let id = funded_grant(&mut grants, &mut l, 100, vec![100]);
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 5),
        Err(GrantsError::InvalidMilestoneIndex)
    );
}

// ─── Co-funding (#250) ───────────────────────────────────────────────────

#[test]
fn two_funders_pay_milestones_pro_rata() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 1_000, vec![400, 600])
        .unwrap();
    grants.co_fund(TREASURY, id, 750, &mut l).unwrap();
    grants.co_fund(FUNDER_A, id, 250, &mut l).unwrap();
    assert_eq!(grants.escrow_balance(ASSET), 1_000);

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    assert_eq!(grants.funder_position(id, TREASURY).unwrap().allocated, 300);
    assert_eq!(grants.funder_position(id, FUNDER_A).unwrap().allocated, 100);
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(400));

    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(600));
    assert_eq!(grants.funder_position(id, TREASURY).unwrap().disbursed, 750);
    assert_eq!(grants.funder_position(id, FUNDER_A).unwrap().disbursed, 250);
    assert_eq!(grants.escrow_balance(ASSET), 0);
    assert!(grants.escrow_matches_positions());
}

#[test]
fn three_funders_remainder_goes_to_earliest_funders() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 300, vec![100, 200])
        .unwrap();
    for f in [FUNDER_A, FUNDER_B, FUNDER_C] {
        grants.co_fund(f, id, 100, &mut l).unwrap();
    }

    // 100 / 3 = 33 each, leftover 1 goes to FUNDER_A (first depositor).
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    let alloc: Vec<i128> = [FUNDER_A, FUNDER_B, FUNDER_C]
        .iter()
        .map(|f| grants.funder_position(id, *f).unwrap().allocated)
        .collect();
    assert_eq!(alloc, vec![34, 33, 33]);

    // Unspent is now 66/67/67 = 200: the last milestone takes everything.
    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(300));
    for f in [FUNDER_A, FUNDER_B, FUNDER_C] {
        let p = grants.funder_position(id, f).unwrap();
        assert_eq!(p.disbursed, 100);
        assert_eq!(p.unspent(), 0);
    }
    assert!(grants.escrow_matches_positions());
}

#[test]
fn pro_rata_shares_are_deterministic_and_never_stuck() {
    let mut a = FunderPosition::new(FUNDER_A);
    a.deposited = 1;
    let mut b = FunderPosition::new(FUNDER_B);
    b.deposited = 1;
    let mut c = FunderPosition::new(FUNDER_C);
    c.deposited = 1;
    let funders = vec![a, b, c];
    assert_eq!(pro_rata_shares(&funders, 1).unwrap(), vec![1, 0, 0]);
    assert_eq!(pro_rata_shares(&funders, 2).unwrap(), vec![1, 1, 0]);
    assert_eq!(pro_rata_shares(&funders, 3).unwrap(), vec![1, 1, 1]);
    assert_eq!(pro_rata_shares(&funders, 4), Err(GrantsError::InsufficientEscrow));

    // A funder with nothing left never receives a remainder unit.
    let mut empty = FunderPosition::new(FUNDER_A);
    empty.deposited = 5;
    empty.disbursed = 5;
    let mut full = FunderPosition::new(FUNDER_B);
    full.deposited = 7;
    assert_eq!(pro_rata_shares(&[empty, full], 7).unwrap(), vec![0, 7]);
}

#[test]
fn milestone_needs_enough_escrow() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 300, vec![200, 100])
        .unwrap();
    grants.co_fund(FUNDER_A, id, 150, &mut l).unwrap();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 0),
        Err(GrantsError::InsufficientEscrow)
    );
    grants.co_fund(FUNDER_B, id, 50, &mut l).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
}

#[test]
fn clawback_refunds_each_funder_their_unspent_share() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 900, vec![300, 300, 300])
        .unwrap();
    grants.co_fund(FUNDER_A, id, 600, &mut l).unwrap();
    grants.co_fund(FUNDER_B, id, 300, &mut l).unwrap();

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    grants.claim(RECIPIENT, id, &mut l).unwrap(); // A paid 200, B paid 100
    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap(); // allocated, not claimed

    grants.clawback_grant(GOVERNANCE, id).unwrap();
    assert_eq!(grants.claimable(id), Some(0));
    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Err(GrantsError::NothingToClaim));

    let a_before = l.balance(ASSET, FUNDER_A);
    let b_before = l.balance(ASSET, FUNDER_B);
    assert_eq!(grants.withdraw_refund(FUNDER_A, id, &mut l), Ok(400));
    assert_eq!(grants.withdraw_refund(FUNDER_B, id, &mut l), Ok(200));
    assert_eq!(l.balance(ASSET, FUNDER_A), a_before + 400);
    assert_eq!(l.balance(ASSET, FUNDER_B), b_before + 200);
    assert_eq!(
        grants.withdraw_refund(FUNDER_A, id, &mut l),
        Err(GrantsError::NothingToRefund)
    );
    assert_eq!(grants.escrow_balance(ASSET), 0);
    assert!(grants.escrow_matches_positions());
}

#[test]
fn cancel_keeps_confirmed_amounts_claimable() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 300, vec![100, 200])
        .unwrap();
    grants.co_fund(FUNDER_A, id, 150, &mut l).unwrap();
    grants.co_fund(FUNDER_B, id, 150, &mut l).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

    grants.cancel_grant(GOVERNANCE, id).unwrap();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 1),
        Err(GrantsError::GrantNotActive)
    );
    assert_eq!(
        grants.co_fund(FUNDER_C, id, 1, &mut l),
        Err(GrantsError::GrantNotActive)
    );
    assert_eq!(grants.cancel_grant(GOVERNANCE, id), Err(GrantsError::GrantNotActive));

    assert_eq!(grants.claim(RECIPIENT, id, &mut l), Ok(100));
    assert_eq!(grants.withdraw_refund(FUNDER_A, id, &mut l), Ok(100));
    assert_eq!(grants.withdraw_refund(FUNDER_B, id, &mut l), Ok(100));
    assert_eq!(grants.escrow_balance(ASSET), 0);
    assert!(grants.escrow_matches_positions());
}

#[test]
fn late_co_funder_only_funds_later_milestones() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 400, vec![200, 200])
        .unwrap();
    grants.co_fund(FUNDER_A, id, 200, &mut l).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    grants.claim(RECIPIENT, id, &mut l).unwrap();

    // FUNDER_B joins after milestone 0 was paid.
    grants.co_fund(FUNDER_B, id, 200, &mut l).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    assert_eq!(grants.funder_position(id, FUNDER_A).unwrap().allocated, 0);
    assert_eq!(grants.funder_position(id, FUNDER_B).unwrap().allocated, 200);
    assert!(grants.escrow_matches_positions());
}

#[test]
fn funder_cap_is_enforced() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 1_000, vec![1_000])
        .unwrap();
    for i in 0..MAX_FUNDERS as u64 {
        let f = 1_000 + i;
        l.mint(ASSET, f, 10);
        grants.co_fund(f, id, 10, &mut l).unwrap();
    }
    l.mint(ASSET, 9_999, 10);
    assert_eq!(
        grants.co_fund(9_999, id, 10, &mut l),
        Err(GrantsError::TooManyFunders)
    );
    // An existing funder may still top up.
    l.mint(ASSET, 1_000, 10);
    grants.co_fund(1_000, id, 10, &mut l).unwrap();
    assert_eq!(grants.get_grant(id).unwrap().funders.len(), MAX_FUNDERS);
}

#[test]
fn deposits_cannot_exceed_total() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 100, vec![100])
        .unwrap();
    grants.co_fund(FUNDER_A, id, 60, &mut l).unwrap();
    assert_eq!(grants.co_fund(FUNDER_B, id, 41, &mut l), Err(GrantsError::OverFunded));
    grants.co_fund(FUNDER_B, id, 40, &mut l).unwrap();
    assert_eq!(grants.co_fund(FUNDER_B, id, 0, &mut l), Err(GrantsError::InvalidAmount));
}

#[test]
fn allowlist_policy_gates_co_funding() {
    let mut grants = contract();
    let mut l = ledger();
    grants
        .set_funding_policy(GOVERNANCE, FundingPolicy::Allowlist)
        .unwrap();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 100, vec![100])
        .unwrap();
    assert_eq!(
        grants.co_fund(FUNDER_A, id, 10, &mut l),
        Err(GrantsError::FunderNotAllowed)
    );
    grants.set_funder_allowed(GOVERNANCE, FUNDER_A, true).unwrap();
    grants.co_fund(FUNDER_A, id, 10, &mut l).unwrap();
    // The treasury is always allowed.
    grants.co_fund(TREASURY, id, 10, &mut l).unwrap();
    grants.set_funder_allowed(GOVERNANCE, FUNDER_A, false).unwrap();
    assert_eq!(
        grants.co_fund(FUNDER_A, id, 10, &mut l),
        Err(GrantsError::FunderNotAllowed)
    );
}

#[test]
fn treasury_push_deposit_is_credited_once() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 500, vec![500])
        .unwrap();
    // Treasury pushes funds with its own `withdraw(.., to = grants)`.
    l.transfer(ASSET, TREASURY, CONTRACT, 300).unwrap();
    grants.credit_treasury_deposit(GOVERNANCE, id, 300, &l).unwrap();
    assert_eq!(grants.funder_position(id, TREASURY).unwrap().deposited, 300);
    // The same tokens can't be credited twice.
    assert_eq!(
        grants.credit_treasury_deposit(GOVERNANCE, id, 1, &l),
        Err(GrantsError::UnaccountedBalanceTooLow)
    );
    assert!(grants.escrow_matches_positions());
}

/// A token that charges a fee on transfer, so the contract receives less
/// than `amount`.
struct FeeLedger(MemoryLedger);

impl TokenLedger for FeeLedger {
    fn balance(&self, asset: Address, owner: Address) -> i128 {
        self.0.balance(asset, owner)
    }
    fn transfer(
        &mut self,
        asset: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), TransferError> {
        self.0.transfer(asset, from, to, amount - 1)
    }
}

#[test]
fn deposit_is_rejected_when_balance_delta_differs() {
    let mut grants = contract();
    let mut l = FeeLedger(ledger());
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 100, vec![100])
        .unwrap();
    assert_eq!(
        grants.co_fund(FUNDER_A, id, 50, &mut l),
        Err(GrantsError::TransferMismatch)
    );
    assert!(grants.funder_position(id, FUNDER_A).is_none());
    assert_eq!(grants.escrow_balance(ASSET), 0);
}

#[test]
fn frozen_funder_does_not_block_other_refunds() {
    let mut grants = contract();
    let mut l = ledger();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, ASSET, 200, vec![200])
        .unwrap();
    grants.co_fund(FUNDER_A, id, 100, &mut l).unwrap();
    grants.co_fund(FUNDER_B, id, 100, &mut l).unwrap();
    grants.clawback_grant(GOVERNANCE, id).unwrap();

    l.set_frozen(ASSET, FUNDER_A, true);
    assert_eq!(
        grants.withdraw_refund(FUNDER_A, id, &mut l),
        Err(GrantsError::TransferFailed(TransferError::Frozen))
    );
    // Nothing changed for A, and B is unaffected.
    assert_eq!(grants.funder_position(id, FUNDER_A).unwrap().refund_owed, 100);
    assert_eq!(grants.withdraw_refund(FUNDER_B, id, &mut l), Ok(100));
    assert!(grants.escrow_matches_positions());

    l.set_frozen(ASSET, FUNDER_A, false);
    assert_eq!(grants.withdraw_refund(FUNDER_A, id, &mut l), Ok(100));
    assert_eq!(grants.escrow_balance(ASSET), 0);
}

#[test]
fn failed_claim_transfer_changes_nothing() {
    let mut grants = contract();
    let mut l = ledger();
    let id = funded_grant(&mut grants, &mut l, 100, vec![100]);
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    l.set_frozen(ASSET, RECIPIENT, true);
    assert_eq!(
        grants.claim(RECIPIENT, id, &mut l),
        Err(GrantsError::TransferFailed(TransferError::Frozen))
    );
    assert_eq!(grants.claimable(id), Some(100));
    assert_eq!(held(&grants, id, FUNDER_A), 100);
    assert_eq!(grants.escrow_balance(ASSET), 100);
    assert!(grants.escrow_matches_positions());
}

/// xorshift64, so the randomized test needs no extra dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn escrow_equals_sum_of_positions_under_random_sequences() {
    for seed in 1..=200u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut grants = contract();
        let mut l = MemoryLedger::new();
        let funders: Vec<Address> = (0..6).map(|i| 10 + i).collect();
        for f in &funders {
            l.mint(ASSET, *f, 1_000_000);
        }
        l.mint(ASSET, TREASURY, 1_000_000);

        let mut ids = Vec::new();
        for _ in 0..3 {
            let n = 1 + rng.below(4) as usize;
            let ms: Vec<i128> = (0..n).map(|_| 1 + rng.below(500) as i128).collect();
            let total = ms.iter().sum();
            ids.push(
                grants
                    .create_grant(GOVERNANCE, RECIPIENT, ASSET, total, ms)
                    .unwrap(),
            );
        }

        for _ in 0..60 {
            let id = ids[rng.below(ids.len() as u64) as usize];
            let f = funders[rng.below(funders.len() as u64) as usize];
            // Errors are expected for many random steps; the invariant must
            // hold either way.
            let _ = match rng.below(7) {
                0 | 1 => grants.co_fund(f, id, 1 + rng.below(300) as i128, &mut l),
                2 => {
                    let next = grants
                        .get_grant(id)
                        .unwrap()
                        .milestones
                        .iter()
                        .position(|m| !m.confirmed)
                        .unwrap_or(0);
                    grants.confirm_milestone(GOVERNANCE, id, next)
                }
                3 => grants.claim(RECIPIENT, id, &mut l).map(|_| ()),
                4 => {
                    if rng.below(2) == 0 {
                        grants.cancel_grant(GOVERNANCE, id)
                    } else {
                        grants.clawback_grant(GOVERNANCE, id)
                    }
                }
                5 => grants.withdraw_refund(f, id, &mut l).map(|_| ()),
                _ => {
                    let amount = 1 + rng.below(100) as i128;
                    if l.transfer(ASSET, TREASURY, CONTRACT, amount).is_ok() {
                        grants.credit_treasury_deposit(GOVERNANCE, id, amount, &l)
                    } else {
                        Ok(())
                    }
                }
            };
            assert!(grants.escrow_matches_positions(), "seed {seed}");
            assert!(
                l.balance(ASSET, CONTRACT) >= grants.escrow_balance(ASSET),
                "seed {seed}: escrow not backed by tokens"
            );
            for id in &ids {
                for p in &grants.get_grant(*id).unwrap().funders {
                    assert!(p.unspent() >= 0, "seed {seed}");
                }
            }
        }
    }
}
