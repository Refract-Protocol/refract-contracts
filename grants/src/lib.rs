#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with grantee
//! performance bond custody, integer basis-points sizing, and timelock-gated
//! slashing on milestone clawback or abandonment.

use std::collections::{BTreeMap, BTreeSet};

/// Basis-point denominator (100% = 10,000 bps).
pub const BPS_DENOMINATOR: i128 = 10_000;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Integer fixed-point basis-points calculation (`amount * bps / 10_000`).
/// Rejects negative amounts or `bps > 10_000`.
pub fn compute_bps_amount(amount: i128, bps: u32) -> Result<i128, GrantsError> {
    if amount < 0 || bps > 10_000 {
        return Err(GrantsError::InvalidSlashBps);
    }
    amount
        .checked_mul(bps as i128)
        .map(|prod| prod / BPS_DENOMINATOR)
        .ok_or(GrantsError::MilestoneSumMismatch)
}

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
    /// Required grantee performance bond has not been posted; grant remains `AcceptedUnfunded`.
    BondNotPosted,
    /// Bond has already been posted for this grant.
    BondAlreadyPosted,
    /// Supplied bond asset is not in the governance-allowlisted SEP-41 asset set.
    AssetNotAllowlisted,
    /// Supplied bond amount does not match the required bond sizing rule.
    InvalidBondAmount,
    /// Basis-points parameter is invalid or exceeds 10,000.
    InvalidSlashBps,
    /// Requested slash bps exceeds the configured maximum slash bps.
    SlashExceedsMaxBps,
    /// Milestone has already been clawed back / slashed (idempotency guard).
    MilestoneAlreadySlashed,
    /// Bond is not active or has no remaining balance to slash/release.
    BondNotActive,
    /// Bond cannot be released until all milestones are confirmed without clawbacks.
    GrantNotCompleted,
    /// Bond release is blocked because a milestone was clawed back.
    BondReleaseBlockedByClawback,
}

/// Per-round or per-grant bond sizing rule configured by governance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondSizingRule {
    /// No performance bond required.
    None,
    /// Fixed token amount required regardless of grant size.
    Fixed(i128),
    /// Basis points (out of 10,000) of the grant's `total_amount`.
    Bps(u32),
}

/// Destination where slashed bond funds are routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashDestination {
    /// Route slashed bond funds to `RefractTreasury`.
    Treasury(Address),
    /// Route slashed bond funds to `RefractBackstop`.
    Backstop(Address),
}

/// Governance configuration for grantee performance bonds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BondConfig {
    /// Allowlisted SEP-41 token addresses eligible for bond posting.
    pub allowlisted_assets: BTreeSet<Address>,
    /// Active sizing rule for newly created grants.
    pub sizing_rule: BondSizingRule,
    /// Maximum slash basis points per clawback (0..=10_000).
    pub max_slash_bps: u32,
    /// Destination for slashed bond funds.
    pub slash_destination: SlashDestination,
}

/// High-level funding status of a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    /// Grant is accepted by governance but awaits grantee performance bond posting;
    /// milestones cannot unlock in this state.
    AcceptedUnfunded,
    /// Grant is funded and active (either no bond required or bond posted).
    Funded,
    /// All milestones confirmed without clawback.
    Completed,
    /// At least one milestone was clawed back / abandoned.
    ClawedBack,
}

/// Lifecycle state of a grant's performance bond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondState {
    /// No bond required for this grant.
    NotRequired,
    /// Bond required by sizing rule but not yet posted by grantee.
    Unposted,
    /// Bond posted and held in isolated bond custody.
    Posted,
    /// Bond has been partially slashed due to a milestone clawback.
    PartiallySlashed,
    /// Bond has been completely slashed.
    FullySlashed,
    /// Bond has been returned in full to the grantee upon grant completion.
    Released,
}

/// Isolated performance bond state per grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BondRecord {
    /// Token asset address posted for the bond.
    pub asset: Option<Address>,
    /// Required bond amount computed at grant creation.
    pub required_amount: i128,
    /// Actual bond amount posted by the grantee.
    pub posted_amount: i128,
    /// Cumulative bond amount slashed across clawed-back milestones.
    pub slashed_amount: i128,
    /// Amount returned to the grantee upon completion.
    pub released_amount: i128,
    /// Current bond state.
    pub state: BondState,
    /// Set of milestone indices that have already triggered a slash (idempotency).
    pub slashed_milestones: BTreeSet<usize>,
}

impl BondRecord {
    /// Remaining unslashed and unreleased bond balance held in custody.
    pub fn remaining(&self) -> i128 {
        self.posted_amount
            .saturating_sub(self.slashed_amount)
            .saturating_sub(self.released_amount)
    }
}

/// Tagged accounting buckets ensuring milestone escrow and performance bond
/// custody never cross-contaminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingTag {
    MilestoneEscrow,
    BondCustody,
    SlashedBondRouting(SlashDestination),
    GranteePayout,
    GranteeBondReturn,
}

/// Structured audit journal entry for escrow vs. bond custody movements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub grant_id: GrantId,
    pub debit_tag: AccountingTag,
    pub credit_tag: AccountingTag,
    pub amount: i128,
}

/// Events emitted during performance bond lifecycle transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BondEvent {
    BondPosted {
        grant_id: GrantId,
        grantee: Address,
        asset: Address,
        amount: i128,
    },
    BondSlashed {
        grant_id: GrantId,
        milestone_index: usize,
        slash_bps: u32,
        slashed_amount: i128,
        destination: SlashDestination,
        reason: String,
    },
    BondReleased {
        grant_id: GrantId,
        grantee: Address,
        amount: i128,
    },
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
    /// Whether the milestone was clawed back / abandoned.
    pub clawed_back: bool,
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
    /// Current grant funding/completion status.
    pub status: GrantStatus,
    /// Isolated performance bond state for this grant.
    pub bond: BondRecord,
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
    /// Governance bond configuration.
    bond_config: BondConfig,
    /// Total milestone escrow balance held across active grants (never mixed with bonds).
    escrow_custody_balance: i128,
    /// Total performance bond balance held across active grants (never mixed with escrow).
    bond_custody_balance: i128,
    /// Cumulative slashed bond funds routed to treasury/backstop.
    slashed_routed_balance: i128,
    /// Tagged accounting journal entries.
    journal: Vec<JournalEntry>,
    /// Emitted bond events.
    events: Vec<BondEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            bond_config: BondConfig {
                allowlisted_assets: BTreeSet::new(),
                sizing_rule: BondSizingRule::None,
                max_slash_bps: 10_000,
                slash_destination: SlashDestination::Treasury(governance),
            },
            escrow_custody_balance: 0,
            bond_custody_balance: 0,
            slashed_routed_balance: 0,
            journal: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Configures per-round bond sizing rules, allowlisted SEP-41 assets,
    /// maximum slash bps, and slash destination. Governance/timelock-gated.
    pub fn set_bond_config(
        &mut self,
        caller: Address,
        config: BondConfig,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if config.max_slash_bps > 10_000 {
            return Err(GrantsError::InvalidSlashBps);
        }
        match config.sizing_rule {
            BondSizingRule::Fixed(amt) if amt <= 0 => return Err(GrantsError::InvalidBondAmount),
            BondSizingRule::Bps(bps) if bps == 0 || bps > 10_000 => {
                return Err(GrantsError::InvalidSlashBps)
            }
            _ => {}
        }
        self.bond_config = config;
        Ok(())
    }

    /// Returns the active bond configuration.
    pub fn bond_config(&self) -> &BondConfig {
        &self.bond_config
    }

    /// Returns the isolated milestone escrow balance.
    pub fn escrow_custody_balance(&self) -> i128 {
        self.escrow_custody_balance
    }

    /// Returns the isolated grantee performance bond custody balance.
    pub fn bond_custody_balance(&self) -> i128 {
        self.bond_custody_balance
    }

    /// Returns total slashed bond funds routed to the configured destination.
    pub fn slashed_routed_balance(&self) -> i128 {
        self.slashed_routed_balance
    }

    /// Returns the tagged accounting journal.
    pub fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }

    /// Returns the emitted bond lifecycle events.
    pub fn events(&self) -> &[BondEvent] {
        &self.events
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
    /// If a bond sizing rule is configured, the grant starts in `AcceptedUnfunded`
    /// until the grantee posts the required bond via [`Self::post_bond`].
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
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let required_bond = match self.bond_config.sizing_rule {
            BondSizingRule::None => 0,
            BondSizingRule::Fixed(amt) => amt,
            BondSizingRule::Bps(bps) => compute_bps_amount(total_amount, bps)?,
        };

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
                clawed_back: false,
            })
            .collect();

        let (status, bond_state) = if required_bond > 0 {
            (GrantStatus::AcceptedUnfunded, BondState::Unposted)
        } else {
            (GrantStatus::Funded, BondState::NotRequired)
        };

        self.escrow_custody_balance = self
            .escrow_custody_balance
            .checked_add(total_amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                status,
                bond: BondRecord {
                    asset: None,
                    required_amount: required_bond,
                    posted_amount: 0,
                    slashed_amount: 0,
                    released_amount: 0,
                    state: bond_state,
                    slashed_milestones: BTreeSet::new(),
                },
            },
        );

        Ok(grant_id)
    }

    /// Posts the required performance bond for `grant_id` in an allowlisted
    /// SEP-41 asset, transitioning the grant from `AcceptedUnfunded` to `Funded`.
    pub fn post_bond(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        asset: Address,
        amount: i128,
    ) -> Result<(), GrantsError> {
        if !self.bond_config.allowlisted_assets.contains(&asset) {
            return Err(GrantsError::AssetNotAllowlisted);
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != caller {
            return Err(GrantsError::NotRecipient);
        }
        if grant.bond.state != BondState::Unposted {
            return Err(GrantsError::BondAlreadyPosted);
        }
        if amount <= 0 || amount != grant.bond.required_amount {
            return Err(GrantsError::InvalidBondAmount);
        }

        grant.bond.asset = Some(asset);
        grant.bond.posted_amount = amount;
        grant.bond.state = BondState::Posted;
        grant.status = GrantStatus::Funded;

        self.bond_custody_balance = self
            .bond_custody_balance
            .checked_add(amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        self.journal.push(JournalEntry {
            grant_id,
            debit_tag: AccountingTag::GranteeBondReturn,
            credit_tag: AccountingTag::BondCustody,
            amount,
        });
        self.events.push(BondEvent::BondPosted {
            grant_id,
            grantee: caller,
            asset,
            amount,
        });

        Ok(())
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order. Rejects confirmation if a required performance bond
    /// has not been posted (`AcceptedUnfunded`).
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

        if grant.status == GrantStatus::AcceptedUnfunded || grant.bond.state == BondState::Unposted
        {
            return Err(GrantsError::BondNotPosted);
        }

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
        if milestone.clawed_back {
            return Err(GrantsError::MilestoneAlreadySlashed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        if grant
            .milestones
            .iter()
            .all(|m| m.confirmed && !m.clawed_back)
        {
            grant.status = GrantStatus::Completed;
        }

        Ok(())
    }

    /// Slashes a grantee's performance bond when a milestone is clawed back or
    /// abandoned. Timelock/governance-gated, idempotent per milestone, capped
    /// at `max_slash_bps`, and routed to the configured destination.
    pub fn slash_bond(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
        slash_bps: u32,
        reason: &str,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;

        if slash_bps == 0 || slash_bps > 10_000 {
            return Err(GrantsError::InvalidSlashBps);
        }
        if slash_bps > self.bond_config.max_slash_bps {
            return Err(GrantsError::SlashExceedsMaxBps);
        }

        let destination = self.bond_config.slash_destination;
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }
        if grant.bond.slashed_milestones.contains(&milestone_index)
            || grant.milestones[milestone_index].clawed_back
        {
            return Err(GrantsError::MilestoneAlreadySlashed);
        }
        if grant.milestones[milestone_index].confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }
        if grant.bond.state != BondState::Posted && grant.bond.state != BondState::PartiallySlashed
        {
            return Err(GrantsError::BondNotActive);
        }

        let remaining_bond = grant.bond.remaining();
        if remaining_bond <= 0 {
            return Err(GrantsError::BondNotActive);
        }

        let computed_slash = compute_bps_amount(grant.bond.posted_amount, slash_bps)?;
        let slashed_amount = computed_slash.min(remaining_bond);

        grant.milestones[milestone_index].clawed_back = true;
        grant.bond.slashed_milestones.insert(milestone_index);
        grant.bond.slashed_amount += slashed_amount;
        grant.status = GrantStatus::ClawedBack;
        grant.bond.state = if grant.bond.remaining() == 0 {
            BondState::FullySlashed
        } else {
            BondState::PartiallySlashed
        };

        // Deduct from isolated bond custody (never touching escrow_custody_balance).
        self.bond_custody_balance -= slashed_amount;
        self.slashed_routed_balance += slashed_amount;

        self.journal.push(JournalEntry {
            grant_id,
            debit_tag: AccountingTag::BondCustody,
            credit_tag: AccountingTag::SlashedBondRouting(destination),
            amount: slashed_amount,
        });
        self.events.push(BondEvent::BondSlashed {
            grant_id,
            milestone_index,
            slash_bps,
            slashed_amount,
            destination,
            reason: reason.to_string(),
        });

        Ok(slashed_amount)
    }

    /// Releases the remaining performance bond in full to the grantee upon
    /// successful completion of all milestones. Rejects release if any milestone
    /// is unconfirmed or clawed back.
    pub fn release_bond(
        &mut self,
        caller: Address,
        grant_id: GrantId,
    ) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if caller != grant.recipient && caller != self.governance {
            return Err(GrantsError::NotRecipient);
        }
        if grant.milestones.iter().any(|m| m.clawed_back)
            || !grant.bond.slashed_milestones.is_empty()
        {
            return Err(GrantsError::BondReleaseBlockedByClawback);
        }
        if !grant.milestones.iter().all(|m| m.confirmed) {
            return Err(GrantsError::GrantNotCompleted);
        }
        if grant.bond.state != BondState::Posted {
            return Err(GrantsError::BondNotActive);
        }

        let amount = grant.bond.remaining();
        if amount <= 0 {
            return Err(GrantsError::BondNotActive);
        }

        grant.bond.released_amount = amount;
        grant.bond.state = BondState::Released;
        self.bond_custody_balance -= amount;

        let grantee = grant.recipient;
        self.journal.push(JournalEntry {
            grant_id,
            debit_tag: AccountingTag::BondCustody,
            credit_tag: AccountingTag::GranteeBondReturn,
            amount,
        });
        self.events.push(BondEvent::BondReleased {
            grant_id,
            grantee,
            amount,
        });

        Ok(amount)
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
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

        self.escrow_custody_balance -= amount;
        self.journal.push(JournalEntry {
            grant_id,
            debit_tag: AccountingTag::MilestoneEscrow,
            credit_tag: AccountingTag::GranteePayout,
            amount,
        });

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;
    const USDC_ASSET: Address = 10;
    const UNLISTED_ASSET: Address = 99;
    const TREASURY: Address = 50;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    fn bonded_contract(rule: BondSizingRule, max_slash_bps: u32) -> RefractGrants {
        let mut grants = RefractGrants::new(GOVERNANCE);
        let mut allowlisted = BTreeSet::new();
        allowlisted.insert(USDC_ASSET);
        grants
            .set_bond_config(
                GOVERNANCE,
                BondConfig {
                    allowlisted_assets: allowlisted,
                    sizing_rule: rule,
                    max_slash_bps,
                    slash_destination: SlashDestination::Treasury(TREASURY),
                },
            )
            .unwrap();
        grants
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        // Nothing claimable before any confirmation.
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

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

    #[test]
    fn full_bond_lifecycle_bps_sizing_and_release_on_completion() {
        // 10% (1000 bps) bond required.
        let mut grants = bonded_contract(BondSizingRule::Bps(1_000), 5_000);
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 10_000, vec![4_000, 6_000])
            .unwrap();

        let g = grants.get_grant(id).unwrap();
        assert_eq!(g.status, GrantStatus::AcceptedUnfunded);
        assert_eq!(g.bond.required_amount, 1_000);
        assert_eq!(g.bond.state, BondState::Unposted);

        // Cannot confirm milestones while bond is unposted.
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 0),
            Err(GrantsError::BondNotPosted)
        );
        // Rejects unlisted asset and wrong bond amount.
        assert_eq!(
            grants.post_bond(RECIPIENT, id, UNLISTED_ASSET, 1_000),
            Err(GrantsError::AssetNotAllowlisted)
        );
        assert_eq!(
            grants.post_bond(RECIPIENT, id, USDC_ASSET, 500),
            Err(GrantsError::InvalidBondAmount)
        );

        // Post valid bond.
        grants.post_bond(RECIPIENT, id, USDC_ASSET, 1_000).unwrap();
        assert_eq!(grants.bond_custody_balance(), 1_000);
        assert_eq!(grants.escrow_custody_balance(), 10_000);

        // Cannot release bond before all milestones are confirmed.
        assert_eq!(
            grants.release_bond(RECIPIENT, id),
            Err(GrantsError::GrantNotCompleted)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(10_000));

        // Release bond in full on completion.
        assert_eq!(grants.release_bond(RECIPIENT, id), Ok(1_000));
        assert_eq!(grants.bond_custody_balance(), 0);
        assert_eq!(grants.escrow_custody_balance(), 0);
        assert_eq!(
            grants.get_grant(id).unwrap().bond.state,
            BondState::Released
        );
    }

    #[test]
    fn partial_slash_max_cap_idempotency_and_race_with_release() {
        // Fixed 2,000 bond, max 6,000 bps (60%) slash per milestone.
        let mut grants = bonded_contract(BondSizingRule::Fixed(2_000), 6_000);
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 10_000, vec![5_000, 5_000])
            .unwrap();
        grants.post_bond(RECIPIENT, id, USDC_ASSET, 2_000).unwrap();

        // Non-governance cannot slash.
        assert_eq!(
            grants.slash_bond(STRANGER, id, 0, 5_000, "abandoned"),
            Err(GrantsError::Unauthorized)
        );

        // Slash above max_slash_bps (6,000) is rejected.
        assert_eq!(
            grants.slash_bond(GOVERNANCE, id, 0, 7_500, "exceeds cap"),
            Err(GrantsError::SlashExceedsMaxBps)
        );

        // Partial slash: 5,000 bps (50%) of 2,000 = 1,000 routed to treasury.
        assert_eq!(
            grants.slash_bond(GOVERNANCE, id, 0, 5_000, "milestone 0 clawback"),
            Ok(1_000)
        );
        assert_eq!(grants.bond_custody_balance(), 1_000);
        assert_eq!(grants.slashed_routed_balance(), 1_000);
        // Milestone escrow balance is completely untouched by bond slashing.
        assert_eq!(grants.escrow_custody_balance(), 10_000);

        // Double-slash on the same milestone is rejected (idempotency).
        assert_eq!(
            grants.slash_bond(GOVERNANCE, id, 0, 2_000, "duplicate slash"),
            Err(GrantsError::MilestoneAlreadySlashed)
        );

        // Bond release race in same ledger after clawback is blocked.
        assert_eq!(
            grants.release_bond(RECIPIENT, id),
            Err(GrantsError::BondReleaseBlockedByClawback)
        );

        // Second milestone clawback at 6,000 bps (1,200) is capped at remaining bond (1,000).
        assert_eq!(
            grants.slash_bond(GOVERNANCE, id, 1, 6_000, "milestone 1 abandoned"),
            Ok(1_000)
        );
        assert_eq!(grants.bond_custody_balance(), 0);
        assert_eq!(grants.slashed_routed_balance(), 2_000);
        assert_eq!(
            grants.get_grant(id).unwrap().bond.state,
            BondState::FullySlashed
        );
    }
}
