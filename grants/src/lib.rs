#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with an append-only
//! double-entry accounting journal and balance identity checks.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
pub type Address = u64;

/// Asset identifier used by the double-entry journal.
pub type AssetId = u64;

/// Virtual escrow account address used in the double-entry journal for
/// confirmed-but-unclaimed milestone obligations.
pub const GRANTS_ESCROW_ACCOUNT: Address = u64::MAX - 1;

/// Virtual treasury funding account address used in the double-entry journal.
pub const TREASURY_FUNDING_ACCOUNT: Address = u64::MAX;

/// A single double-entry journal posting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub entry_id: u64,
    pub debit_account: Address,
    pub credit_account: Address,
    pub asset: AssetId,
    pub amount: i128,
    pub action_tag: &'static str,
    pub ledger_sequence: u32,
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
    /// Debit and credit accounts must be distinct.
    SelfTransfer,
    /// Double-entry accounting invariant violation.
    AccountingImbalance,
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

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    governance: Address,
    next_grant_id: GrantId,
    grants: BTreeMap<GrantId, Grant>,
    journal: Vec<JournalEntry>,
    balances: BTreeMap<(Address, AssetId), i128>,
    total_debits: BTreeMap<AssetId, i128>,
    total_credits: BTreeMap<AssetId, i128>,
    ledger_sequence: u32,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            journal: Vec::new(),
            balances: BTreeMap::new(),
            total_debits: BTreeMap::new(),
            total_credits: BTreeMap::new(),
            ledger_sequence: 1,
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

    /// Net double-entry journal balance `(debits - credits)` for `(account, asset)`.
    pub fn balance_of(&self, account: Address, asset: AssetId) -> i128 {
        self.balances.get(&(account, asset)).copied().unwrap_or(0)
    }

    /// Total gross posted volume (`total_debits == total_credits`) for `asset`.
    pub fn total_supply(&self, asset: AssetId) -> i128 {
        self.total_debits.get(&asset).copied().unwrap_or(0)
    }

    /// Read-only view of all append-only journal entries.
    pub fn journal(&self) -> &[JournalEntry] {
        &self.journal
    }

    /// Verify that for `asset`, `sum(debits) == sum(credits)`, the sum of net
    /// account balances equals zero, and the escrow account balance equals the
    /// sum of all grants' `claimable` amounts.
    pub fn assert_balanced(&self, asset: AssetId) -> Result<bool, GrantsError> {
        let debits = self.total_debits.get(&asset).copied().unwrap_or(0);
        let credits = self.total_credits.get(&asset).copied().unwrap_or(0);
        if debits != credits {
            return Err(GrantsError::AccountingImbalance);
        }

        let net_sum: i128 = self
            .balances
            .iter()
            .filter(|((_, a), _)| *a == asset)
            .map(|(_, bal)| *bal)
            .sum();
        if net_sum != 0 {
            return Err(GrantsError::AccountingImbalance);
        }

        if asset == 0 {
            let total_claimable: i128 = self.grants.values().map(|g| g.claimable).sum();
            let escrow_bal = self.balance_of(GRANTS_ESCROW_ACCOUNT, 0);
            if total_claimable != escrow_bal {
                return Err(GrantsError::AccountingImbalance);
            }
        }

        Ok(true)
    }

    /// Internal balanced posting helper rejecting zero/negative amounts and
    /// self-transfers (`debit_account == credit_account`).
    pub fn post_journal_entry(
        &mut self,
        debit_account: Address,
        credit_account: Address,
        asset: AssetId,
        amount: i128,
        action_tag: &'static str,
    ) -> Result<u64, GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidMilestoneAmount);
        }
        if debit_account == credit_account {
            return Err(GrantsError::SelfTransfer);
        }

        let entry_id = (self.journal.len() as u64) + 1;
        self.journal.push(JournalEntry {
            entry_id,
            debit_account,
            credit_account,
            asset,
            amount,
            action_tag,
            ledger_sequence: self.ledger_sequence,
        });

        *self.balances.entry((debit_account, asset)).or_insert(0) += amount;
        *self.balances.entry((credit_account, asset)).or_insert(0) -= amount;
        *self.total_debits.entry(asset).or_insert(0) += amount;
        *self.total_credits.entry(asset).or_insert(0) += amount;

        Ok(entry_id)
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
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

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable and
    /// posting a balanced journal entry (`debit = GRANTS_ESCROW_ACCOUNT`,
    /// `credit = TREASURY_FUNDING_ACCOUNT`).
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let milestone_amount = {
            let grant = self
                .grants
                .get_mut(&grant_id)
                .ok_or(GrantsError::GrantNotFound)?;

            if milestone_index >= grant.milestones.len() {
                return Err(GrantsError::InvalidMilestoneIndex);
            }

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
            milestone.amount
        };

        self.post_journal_entry(
            GRANTS_ESCROW_ACCOUNT,
            TREASURY_FUNDING_ACCOUNT,
            0,
            milestone_amount,
            "confirm_milestone",
        )?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient and posts a balanced journal entry (`debit = recipient`,
    /// `credit = GRANTS_ESCROW_ACCOUNT`).
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

        self.post_journal_entry(recipient, GRANTS_ESCROW_ACCOUNT, 0, amount, "claim")?;

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

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();
        assert_eq!(grants.assert_balanced(0), Ok(true));

        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.assert_balanced(0), Ok(true));
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.assert_balanced(0), Ok(true));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.assert_balanced(0), Ok(true));
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.assert_balanced(0), Ok(true));
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(grants.balance_of(RECIPIENT, 0), 300);
        assert_eq!(grants.balance_of(GRANTS_ESCROW_ACCOUNT, 0), 0);
        assert_eq!(grants.balance_of(TREASURY_FUNDING_ACCOUNT, 0), -300);
    }

    #[test]
    fn rejects_self_transfer_and_zero_postings() {
        let mut grants = contract();
        assert_eq!(
            grants.post_journal_entry(RECIPIENT, RECIPIENT, 0, 100, "self"),
            Err(GrantsError::SelfTransfer)
        );
        assert_eq!(
            grants.post_journal_entry(RECIPIENT, GRANTS_ESCROW_ACCOUNT, 0, 0, "zero"),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn deterministic_sequence_preserves_accounting_identity() {
        let mut grants = contract();
        for step in 1..=10 {
            let m1 = (step * 37) as i128;
            let m2 = (step * 53) as i128;
            let gid = grants
                .create_grant(GOVERNANCE, RECIPIENT + step, m1 + m2, vec![m1, m2])
                .unwrap();
            grants.confirm_milestone(GOVERNANCE, gid, 0).unwrap();
            assert_eq!(grants.assert_balanced(0), Ok(true));
            if step % 2 == 0 {
                grants.claim(RECIPIENT + step, gid).unwrap();
                assert_eq!(grants.assert_balanced(0), Ok(true));
            }
            grants.confirm_milestone(GOVERNANCE, gid, 1).unwrap();
            assert_eq!(grants.assert_balanced(0), Ok(true));
            grants.claim(RECIPIENT + step, gid).unwrap();
            assert_eq!(grants.assert_balanced(0), Ok(true));
        }
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
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
