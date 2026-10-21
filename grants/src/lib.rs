//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//!   Confirmation is deliberately separate from creation so different reviewers
//!   can confirm delivery of individual milestones.
//! * `claim` is permissionless for the recipient and releases whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount,
    /// The total amount was zero or negative.
    InvalidTotalAmount,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder,
    /// The caller is not the grant recipient.
    NotRecipient,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

/// A single balanced double-entry posting for every grant movement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub seq: u64,
    pub debit: &'static str,
    pub credit: &'static str,
    pub amount: i128,
    pub timestamp: u64,
}

/// Risk-guardian breaker watching oracle staleness and depeg bands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianConfig {
    pub max_staleness_secs: u64,
    pub depeg_bps: u32,
    pub peg_price: i128,
    pub last_price: i128,
    pub last_update: u64,
    pub payouts_paused: bool,
}

/// Capped keeper incentive for permissionless `execute_due`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeeperConfig {
    pub reward_per_call: i128,
    pub reward_cap_per_epoch: i128,
    pub epoch_len_secs: u64,
    pub epoch_start: u64,
    pub epoch_spent: i128,
}

/// Committed-but-undrawn capital backing a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantReservation {
    pub id: u64,
    pub amount: i128,
    pub drawn: i128,
    pub released: bool,
    pub purpose: &'static str,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Append-only balanced journal (never truncated).
    journal: Vec<JournalEntry>,
    /// Guardian breaker state (`None` until configured).
    guardian: Option<GuardianConfig>,
    /// Keeper reward state.
    keeper: KeeperConfig,
    /// Treasury reservations noted by this contract.
    reservations: BTreeMap<u64, GrantReservation>,
    /// Currently noted treasury reservation, if any.
    active_reservation: Option<(u64, i128)>,
    /// Logical clock used for journal timestamps in this off-ledger model.
    clock: u64,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            journal: Vec::new(),
            guardian: None,
            keeper: KeeperConfig {
                reward_per_call: 0,
                reward_cap_per_epoch: 0,
                epoch_len_secs: 86_400,
                epoch_start: 0,
                epoch_spent: 0,
            },
            reservations: BTreeMap::new(),
            active_reservation: None,
            clock: 0,
        }
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(*amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
            },
        );
        self.post_balanced("treasury", "grant_escrow", total_amount)?;

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        let amount = {
            let grant = self
                .grants
                .get_mut(&grant_id)
                .ok_or(GrantsError::GrantNotFound)?;
            if grant.recipient != recipient {
                return Err(GrantsError::NotRecipient);
            }
            let amount = grant.claimable;
            if amount <= 0 {
                return Err(GrantsError::NothingToClaim);
            }
            grant.claimable = 0;
            for milestone in grant.milestones.iter_mut() {
                if milestone.confirmed {
                    milestone.claimed = true;
                }
            }
            amount
        };
        self.check_guardians()?;
        self.post_balanced("grant_escrow", "grantee", amount)?;

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    // ─── #270 Double-entry journal ──────────────────────────────────

    fn post_balanced(
        &mut self,
        debit: &'static str,
        credit: &'static str,
        amount: i128,
    ) -> Result<u64, GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidMilestoneAmount);
        }
        let seq = self.journal.len() as u64;
        self.clock += 1;
        self.journal.push(JournalEntry {
            seq,
            debit,
            credit,
            amount,
            timestamp: self.clock,
        });
        Ok(seq)
    }

    pub fn journal_len(&self) -> usize {
        self.journal.len()
    }

    pub fn journal_entry(&self, seq: usize) -> Option<&JournalEntry> {
        self.journal.get(seq)
    }

    /// Accounting-identity check: every posting is a positive matched pair,
    /// gross approvals equal gross journal inflow, and payouts never exceed
    /// approvals. All arithmetic is checked.
    pub fn check_invariants(&self) -> Result<(), GrantsError> {
        let mut approved: i128 = 0;
        let mut paid: i128 = 0;
        for entry in &self.journal {
            if entry.amount <= 0 {
                return Err(GrantsError::MilestoneSumMismatch);
            }
            if entry.credit == "grant_escrow" {
                approved = approved
                    .checked_add(entry.amount)
                    .ok_or(GrantsError::MilestoneSumMismatch)?;
            } else if entry.debit == "grant_escrow" {
                paid = paid
                    .checked_add(entry.amount)
                    .ok_or(GrantsError::MilestoneSumMismatch)?;
            }
        }
        if paid > approved {
            return Err(GrantsError::MilestoneSumMismatch);
        }
        let mut total_grants: i128 = 0;
        for grant in self.grants.values() {
            total_grants = total_grants
                .checked_add(grant.total_amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            if grant.claimable < 0 {
                return Err(GrantsError::MilestoneSumMismatch);
            }
        }
        if total_grants != approved {
            return Err(GrantsError::MilestoneSumMismatch);
        }
        Ok(())
    }

    // ─── #272 Guardian ──────────────────────────────────────────────

    pub fn set_guardian_config(
        &mut self,
        caller: Address,
        max_staleness_secs: u64,
        depeg_bps: u32,
        peg_price: i128,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if depeg_bps == 0 || depeg_bps > 10_000 || peg_price <= 0 {
            return Err(GrantsError::Unauthorized);
        }
        let prev = self.guardian.clone();
        self.guardian = Some(GuardianConfig {
            max_staleness_secs,
            depeg_bps,
            peg_price,
            last_price: prev.as_ref().map(|p| p.last_price).unwrap_or(peg_price),
            last_update: prev.as_ref().map(|p| p.last_update).unwrap_or(self.clock),
            payouts_paused: prev.as_ref().map(|p| p.payouts_paused).unwrap_or(false),
        });
        Ok(())
    }

    pub fn note_feed(&mut self, price: i128, timestamp: u64) -> Result<(), GrantsError> {
        if price <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let mut cfg = self.guardian.clone().unwrap_or(GuardianConfig {
            max_staleness_secs: 1_800,
            depeg_bps: 500,
            peg_price: price,
            last_price: price,
            last_update: timestamp,
            payouts_paused: false,
        });
        cfg.last_price = price;
        cfg.last_update = timestamp;
        self.guardian = Some(cfg);
        let _ = self.check_guardians();
        Ok(())
    }

    pub fn check_guardians(&mut self) -> Result<(), GrantsError> {
        let mut cfg = match self.guardian.clone() {
            None => return Ok(()),
            Some(c) => c,
        };
        if cfg.payouts_paused {
            return Err(GrantsError::Unauthorized);
        }
        let age = self.clock.saturating_sub(cfg.last_update);
        if age > cfg.max_staleness_secs {
            cfg.payouts_paused = true;
            self.guardian = Some(cfg);
            return Err(GrantsError::Unauthorized);
        }
        let diff = if cfg.last_price >= cfg.peg_price {
            cfg.last_price - cfg.peg_price
        } else {
            cfg.peg_price - cfg.last_price
        };
        let band = diff
            .checked_mul(10_000)
            .ok_or(GrantsError::MilestoneSumMismatch)?
            .checked_div(cfg.peg_price)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        if band > cfg.depeg_bps as i128 {
            cfg.payouts_paused = true;
            self.guardian = Some(cfg);
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    pub fn clear_halt(&mut self, caller: Address) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if let Some(mut cfg) = self.guardian.clone() {
            cfg.payouts_paused = false;
            cfg.last_update = self.clock;
            self.guardian = Some(cfg);
        }
        Ok(())
    }

    // ─── #271 Keeper + #269 reservations ────────────────────────────

    pub fn set_keeper_config(
        &mut self,
        caller: Address,
        reward_per_call: i128,
        reward_cap_per_epoch: i128,
        epoch_len_secs: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if reward_per_call < 0 || reward_cap_per_epoch < 0 || epoch_len_secs == 0 {
            return Err(GrantsError::Unauthorized);
        }
        self.keeper = KeeperConfig {
            reward_per_call,
            reward_cap_per_epoch,
            epoch_len_secs,
            epoch_start: self.clock,
            epoch_spent: 0,
        };
        Ok(())
    }

    /// Permissionless sweep over matured bookkeeping. Pays a capped keeper
    /// reward from the accounting surplus when the epoch budget allows.
    pub fn execute_due(&mut self, _keeper: Address) -> Result<i128, GrantsError> {
        if self.clock >= self.keeper.epoch_start + self.keeper.epoch_len_secs {
            self.keeper.epoch_start = self.clock;
            self.keeper.epoch_spent = 0;
        }
        let mut reward: i128 = 0;
        if self.keeper.reward_per_call > 0
            && self.keeper.epoch_spent + self.keeper.reward_per_call
                <= self.keeper.reward_cap_per_epoch
        {
            self.keeper.epoch_spent += self.keeper.reward_per_call;
            reward = self.keeper.reward_per_call;
        }
        self.clock += 1;
        Ok(reward)
    }

    pub fn note_reservation(
        &mut self,
        caller: Address,
        reservation_id: u64,
        amount: i128,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        self.reservations.insert(
            reservation_id,
            GrantReservation {
                id: reservation_id,
                amount,
                drawn: 0,
                released: false,
                purpose: "grant_escrow",
            },
        );
        self.active_reservation = Some((reservation_id, amount));
        Ok(())
    }

    pub fn reserved(&self) -> Option<(u64, i128)> {
        self.active_reservation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        // Nothing claimable before any confirmation.
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(grants.claim(RECIPIENT, id), Err(GrantsError::NothingToClaim));

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        // Only the first milestone is confirmed, so only 200 is released.
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

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
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(STRANGER, id, 0),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn claim_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }

    #[test]
    fn rejects_unknown_grant_and_index() {
        let mut grants = contract();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, 99, 0),
            Err(GrantsError::GrantNotFound)
        );
        assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }
}
