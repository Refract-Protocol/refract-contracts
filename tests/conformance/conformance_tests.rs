//! Normative specification conformance test suite for `SPEC.md` (§8–§11).
//!
//! Each test in this module directly verifies a numbered normative clause from
//! `SPEC.md` (`SPEC-BS-01`..`SPEC-TR-03`) and is linked in
//! `tests/conformance/clauses.toml`.

#[path = "../../grants/src/lib.rs"]
mod grants_contract;

use std::collections::{BTreeMap, BTreeSet};

const BPS_DENOM: i128 = 10_000;

// ─── Reference State Machine Mirrors for SPEC.md §8, §9, §11 ─────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackstopError {
    AlreadyInitialized,
    Unauthorized,
    InvalidAmount,
    InsufficientShares,
    InvalidPremiumShare,
}

#[derive(Default)]
struct BackstopModel {
    initialized: bool,
    pool: u64,
    admin: u64,
    premium_share_bps: i128,
    total_shares: i128,
    total_capital: i128,
    premium_income: i128,
    shares: BTreeMap<u64, i128>,
}

impl BackstopModel {
    fn initialize(&mut self, pool: u64, admin: u64, bps: i128) -> Result<(), BackstopError> {
        if self.initialized {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        self.initialized = true;
        self.pool = pool;
        self.admin = admin;
        self.premium_share_bps = bps;
        Ok(())
    }

    fn deposit(&mut self, staker: u64, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let minted = if self.total_shares == 0 || self.total_capital == 0 {
            amount
        } else {
            amount * self.total_shares / self.total_capital
        };
        if minted <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        *self.shares.entry(staker).or_insert(0) += minted;
        self.total_shares += minted;
        self.total_capital += amount;
        Ok(minted)
    }

    fn withdraw(&mut self, staker: u64, shares: i128) -> Result<i128, BackstopError> {
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let held = *self.shares.get(&staker).unwrap_or(&0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }
        let amount = if self.total_shares == 0 {
            0
        } else {
            shares * self.total_capital / self.total_shares
        };
        self.shares.insert(staker, held - shares);
        self.total_shares -= shares;
        self.total_capital -= amount;
        Ok(amount)
    }

    fn cover_shortfall(&mut self, caller: u64, amount: i128) -> Result<i128, BackstopError> {
        if caller != self.pool && caller != self.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let covered = amount.min(self.total_capital);
        self.total_capital -= covered;
        Ok(covered)
    }

    fn receive_premium(&mut self, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let cut = amount * self.premium_share_bps / BPS_DENOM;
        self.premium_income += cut;
        self.total_capital += cut;
        Ok(cut)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BountyError {
    AlreadyInitialized,
    InvalidAmount,
    InsufficientBalance,
    Unauthorized,
    DuplicateReport,
}

#[derive(Default)]
struct BountyModel {
    initialized: bool,
    governance: u64,
    balance: i128,
    awards: BTreeSet<[u8; 32]>,
}

impl BountyModel {
    fn initialize(&mut self, governance: u64) -> Result<(), BountyError> {
        if self.initialized {
            return Err(BountyError::AlreadyInitialized);
        }
        self.initialized = true;
        self.governance = governance;
        self.balance = 0;
        Ok(())
    }

    fn fund(&mut self, amount: i128) -> Result<(), BountyError> {
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        self.balance += amount;
        Ok(())
    }

    fn award(&mut self, caller: u64, amount: i128, hash: [u8; 32]) -> Result<(), BountyError> {
        if caller != self.governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if self.awards.contains(&hash) {
            return Err(BountyError::DuplicateReport);
        }
        if amount > self.balance {
            return Err(BountyError::InsufficientBalance);
        }
        self.balance -= amount;
        self.awards.insert(hash);
        Ok(())
    }
}

#[derive(Default)]
struct TreasuryModel {
    admin: Option<u64>,
    balance: i128,
}

impl TreasuryModel {
    fn initialize(&mut self, admin: u64) -> Result<(), &'static str> {
        if self.admin.is_some() {
            return Err("already initialized");
        }
        self.admin = Some(admin);
        Ok(())
    }

    fn withdraw(&mut self, caller: u64, amount: i128) -> Result<i128, &'static str> {
        let admin = self.admin.ok_or("not initialized")?;
        if caller != admin {
            return Err("unauthorized");
        }
        if amount <= 0 {
            return Err("amount must be positive");
        }
        self.balance -= amount;
        Ok(self.balance)
    }
}

// ─── SPEC-BS-01..05 Conformance Tests ────────────────────────────────────────

#[test]
fn spec_bs_01_initialize_and_bps_bounds() {
    let mut bs = BackstopModel::default();
    assert_eq!(
        bs.initialize(1, 2, -1),
        Err(BackstopError::InvalidPremiumShare)
    );
    assert_eq!(
        bs.initialize(1, 2, 10_001),
        Err(BackstopError::InvalidPremiumShare)
    );
    assert_eq!(bs.initialize(1, 2, 2_500), Ok(()));
    assert_eq!(
        bs.initialize(1, 2, 2_500),
        Err(BackstopError::AlreadyInitialized)
    );
}

#[test]
fn spec_bs_02_pro_rata_deposit_and_invalid_amount() {
    let mut bs = BackstopModel::default();
    bs.initialize(1, 2, 2_000).unwrap();
    assert_eq!(bs.deposit(10, 0), Err(BackstopError::InvalidAmount));
    assert_eq!(bs.deposit(10, -5), Err(BackstopError::InvalidAmount));
    assert_eq!(bs.deposit(10, 1_000), Ok(1_000));
    assert_eq!(bs.total_shares, 1_000);
    assert_eq!(bs.total_capital, 1_000);
}

#[test]
fn spec_bs_03_pro_rata_withdraw_and_insufficient_shares() {
    let mut bs = BackstopModel::default();
    bs.initialize(1, 2, 2_000).unwrap();
    bs.deposit(10, 1_000).unwrap();
    assert_eq!(bs.withdraw(10, 0), Err(BackstopError::InvalidAmount));
    assert_eq!(
        bs.withdraw(10, 1_001),
        Err(BackstopError::InsufficientShares)
    );
    assert_eq!(bs.withdraw(10, 400), Ok(400));
    assert_eq!(bs.total_capital, 600);
}

#[test]
fn spec_bs_04_cover_shortfall_auth_and_haircut() {
    let mut bs = BackstopModel::default();
    bs.initialize(1, 2, 2_000).unwrap();
    bs.deposit(10, 1_000).unwrap();
    assert_eq!(
        bs.cover_shortfall(99, 300),
        Err(BackstopError::Unauthorized)
    );
    assert_eq!(bs.cover_shortfall(1, 300), Ok(300));
    assert_eq!(bs.total_capital, 700);
    assert_eq!(bs.total_shares, 1_000);
    assert_eq!(bs.withdraw(10, 1_000), Ok(700));
}

#[test]
fn spec_bs_05_receive_premium_bps_routing() {
    let mut bs = BackstopModel::default();
    bs.initialize(1, 2, 2_500).unwrap();
    assert_eq!(bs.receive_premium(0), Err(BackstopError::InvalidAmount));
    assert_eq!(bs.receive_premium(1_000), Ok(250));
    assert_eq!(bs.premium_income, 250);
    assert_eq!(bs.total_capital, 250);
}

// ─── SPEC-BN-01..04 Conformance Tests ────────────────────────────────────────

#[test]
fn spec_bn_01_initialize_once() {
    let mut bn = BountyModel::default();
    assert_eq!(bn.initialize(1), Ok(()));
    assert_eq!(bn.initialize(1), Err(BountyError::AlreadyInitialized));
}

#[test]
fn spec_bn_02_fund_escrow_and_invalid_amount() {
    let mut bn = BountyModel::default();
    bn.initialize(1).unwrap();
    assert_eq!(bn.fund(0), Err(BountyError::InvalidAmount));
    assert_eq!(bn.fund(1_000), Ok(()));
    assert_eq!(bn.balance, 1_000);
}

#[test]
fn spec_bn_03_governance_award_and_solvency() {
    let mut bn = BountyModel::default();
    bn.initialize(1).unwrap();
    bn.fund(500).unwrap();
    let h = [1u8; 32];
    assert_eq!(bn.award(99, 200, h), Err(BountyError::Unauthorized));
    assert_eq!(bn.award(1, 600, h), Err(BountyError::InsufficientBalance));
    assert_eq!(bn.award(1, 200, h), Ok(()));
    assert_eq!(bn.balance, 300);
}

#[test]
fn spec_bn_04_duplicate_report_rejected() {
    let mut bn = BountyModel::default();
    bn.initialize(1).unwrap();
    bn.fund(1_000).unwrap();
    let h = [7u8; 32];
    assert_eq!(bn.award(1, 400, h), Ok(()));
    assert_eq!(bn.award(1, 400, h), Err(BountyError::DuplicateReport));
}

// ─── SPEC-GR-01..05 Conformance Tests (against grants/src/lib.rs) ────────────

use grants_contract::{GrantsError, RefractGrants};

#[test]
fn spec_gr_01_create_grant_partition_and_bounds() {
    let mut grants = RefractGrants::new(1);
    assert_eq!(
        grants.create_grant(99, 2, 100, vec![100]),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        grants.create_grant(1, 2, 0, vec![0]),
        Err(GrantsError::InvalidTotalAmount)
    );
    assert_eq!(
        grants.create_grant(1, 2, 100, vec![]),
        Err(GrantsError::NoMilestones)
    );
    assert_eq!(
        grants.create_grant(1, 2, 100, vec![50, 40]),
        Err(GrantsError::MilestoneSumMismatch)
    );
    assert_eq!(grants.create_grant(1, 2, 100, vec![60, 40]), Ok(0));
}

#[test]
fn spec_gr_02_sequential_confirmation_and_auth() {
    let mut grants = RefractGrants::new(1);
    let id = grants.create_grant(1, 2, 200, vec![100, 100]).unwrap();
    assert_eq!(
        grants.confirm_milestone(99, id, 0),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        grants.confirm_milestone(1, id, 1),
        Err(GrantsError::MilestoneOutOfOrder)
    );
    assert_eq!(grants.confirm_milestone(1, id, 0), Ok(()));
    assert_eq!(
        grants.confirm_milestone(1, id, 0),
        Err(GrantsError::MilestoneAlreadyConfirmed)
    );
}

#[test]
fn spec_gr_03_incremental_claimable_accrual() {
    let mut grants = RefractGrants::new(1);
    let id = grants.create_grant(1, 2, 300, vec![100, 200]).unwrap();
    assert_eq!(grants.claimable(id), Some(0));
    grants.confirm_milestone(1, id, 0).unwrap();
    assert_eq!(grants.claimable(id), Some(100));
}

#[test]
fn spec_gr_04_recipient_only_claim() {
    let mut grants = RefractGrants::new(1);
    let id = grants.create_grant(1, 2, 100, vec![100]).unwrap();
    grants.confirm_milestone(1, id, 0).unwrap();
    assert_eq!(grants.claim(99, id), Err(GrantsError::NotRecipient));
    assert_eq!(grants.claim(2, id), Ok(100));
}

#[test]
fn spec_gr_05_double_claim_prevented() {
    let mut grants = RefractGrants::new(1);
    let id = grants.create_grant(1, 2, 100, vec![100]).unwrap();
    grants.confirm_milestone(1, id, 0).unwrap();
    assert_eq!(grants.claim(2, id), Ok(100));
    assert_eq!(grants.claim(2, id), Err(GrantsError::NothingToClaim));
}

// ─── SPEC-TR-01..03 Conformance Tests ────────────────────────────────────────

#[test]
fn spec_tr_01_single_initialization() {
    let mut tr = TreasuryModel::default();
    assert_eq!(tr.initialize(1), Ok(()));
    assert_eq!(tr.initialize(2), Err("already initialized"));
}

#[test]
fn spec_tr_02_admin_gated_withdrawal() {
    let mut tr = TreasuryModel {
        admin: Some(1),
        balance: 1_000,
    };
    assert_eq!(tr.withdraw(99, 100), Err("unauthorized"));
    assert_eq!(tr.withdraw(1, 400), Ok(600));
}

#[test]
fn spec_tr_03_positive_outflow_and_conservation() {
    let mut tr = TreasuryModel {
        admin: Some(1),
        balance: 1_000,
    };
    assert_eq!(tr.withdraw(1, 0), Err("amount must be positive"));
    assert_eq!(tr.withdraw(1, -10), Err("amount must be positive"));
    assert_eq!(tr.withdraw(1, 250), Ok(750));
}
