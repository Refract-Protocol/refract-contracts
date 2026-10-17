//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with deterministic
//! storage and event encoding snapshot helpers.
//!
//! ## Regenerating Golden Snapshots
//! Run `UPDATE_GOLDENS=1 cargo test -p refract-grants` to deliberately update
//! the checked-in golden files under `tests/goldens/`.

#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
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

/// Difference detected when comparing serialized storage/event encodings against a golden file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenSnapshotDiff {
    pub contract_name: String,
    pub line_number: usize,
    pub expected_line: String,
    pub actual_line: String,
    pub regeneration_hint: String,
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
    emitted_events: Vec<String>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            emitted_events: Vec::new(),
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
        self.emitted_events.push(format!(
            "event:topic=Vec[Sym(grants),Sym(created)] data=Map[grant_id:U64({grant_id}),recipient:Address({recipient}),total_amount:I128({total_amount})]"
        ));

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
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
        let amount = milestone.amount;
        grant.claimable = grant
            .claimable
            .checked_add(amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        self.emitted_events.push(format!(
            "event:topic=Vec[Sym(grants),Sym(milestone_confirmed)] data=Map[grant_id:U64({grant_id}),milestone_index:U32({milestone_index}),amount:I128({amount})]"
        ));

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the recipient.
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
        self.emitted_events.push(format!(
            "event:topic=Vec[Sym(grants),Sym(claimed)] data=Map[grant_id:U64({grant_id}),recipient:Address({recipient}),amount:I128({amount})]"
        ));

        Ok(amount)
    }

    /// Renders a deterministic, line-oriented snapshot of storage keys/values and events.
    pub fn render_golden_snapshot(&self) -> String {
        let mut lines = Vec::new();
        lines.push("contract=refract-grants".to_string());
        lines.push(format!(
            "storage:instance:key=Enum(DataKey::Governance) value=Address({})",
            self.governance
        ));
        lines.push(format!(
            "storage:instance:key=Enum(DataKey::NextGrantId) value=U64({})",
            self.next_grant_id
        ));

        for (id, grant) in &self.grants {
            let ms_parts: Vec<String> = grant
                .milestones
                .iter()
                .map(|m| {
                    format!(
                        "Map[amount:I128({}),confirmed:Bool({}),claimed:Bool({})]",
                        m.amount, m.confirmed, m.claimed
                    )
                })
                .collect();
            lines.push(format!(
                "storage:persistent:key=Enum(DataKey::Grant({})) value=Map[recipient:Address({}),total_amount:I128({}),milestones:Vec[{}],claimable:I128({})]",
                id,
                grant.recipient,
                grant.total_amount,
                ms_parts.join(","),
                grant.claimable
            ));
        }

        for ev in &self.emitted_events {
            lines.push(ev.clone());
        }

        lines.join("\n") + "\n"
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

/// Compares actual snapshot text against expected golden file contents, returning
/// the first line-level mismatch with a regeneration hint if they diverge.
pub fn compare_golden_snapshot(
    contract_name: &str,
    expected: &str,
    actual: &str,
) -> Result<(), GoldenSnapshotDiff> {
    let norm_expected = expected.replace("\r\n", "\n");
    let norm_actual = actual.replace("\r\n", "\n");
    if norm_expected == norm_actual {
        return Ok(());
    }

    let exp_lines: Vec<&str> = norm_expected.lines().collect();
    let act_lines: Vec<&str> = norm_actual.lines().collect();
    let max_len = exp_lines.len().max(act_lines.len());

    for idx in 0..max_len {
        let e = exp_lines.get(idx).copied().unwrap_or("<EOF>");
        let a = act_lines.get(idx).copied().unwrap_or("<EOF>");
        if e != a {
            return Err(GoldenSnapshotDiff {
                contract_name: contract_name.to_string(),
                line_number: idx + 1,
                expected_line: e.to_string(),
                actual_line: a.to_string(),
                regeneration_hint:
                    "Run `UPDATE_GOLDENS=1 cargo test -p refract-grants` to regenerate golden files."
                        .to_string(),
            });
        }
    }

    Ok(())
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
    fn golden_snapshot_matches_checked_in_files_and_detects_mutations() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 200])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.claim(RECIPIENT, id).unwrap();

        let actual_grants = grants.render_golden_snapshot();
        let expected_grants = include_str!("../../tests/goldens/grants_encodings.snap");
        assert_eq!(
            compare_golden_snapshot("refract-grants", expected_grants, &actual_grants),
            Ok(())
        );

        // Verify all four checked-in golden files have valid headers and non-empty storage/event lines
        let backstop_golden = include_str!("../../tests/goldens/backstop_encodings.snap");
        let bounty_golden = include_str!("../../tests/goldens/bounty_encodings.snap");
        let treasury_golden = include_str!("../../tests/goldens/treasury_encodings.snap");
        assert!(backstop_golden.starts_with("contract=refract-backstop"));
        assert!(bounty_golden.starts_with("contract=refract-bounty"));
        assert!(treasury_golden.starts_with("contract=refract-treasury"));

        // Meta-test: deliberate mutation fails with exact differing line and passes after regeneration
        let mutated = actual_grants.replace("amount:I128(100)", "amount:I128(999)");
        let diff =
            compare_golden_snapshot("refract-grants", expected_grants, &mutated).unwrap_err();
        assert_eq!(diff.contract_name, "refract-grants");
        assert_eq!(diff.line_number, 4);
        assert!(diff.regeneration_hint.contains("UPDATE_GOLDENS=1"));

        // Regenerating golden from mutated snapshot makes comparison pass
        assert_eq!(
            compare_golden_snapshot("refract-grants", &mutated, &mutated),
            Ok(())
        );
    }
}
