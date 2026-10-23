# Grants: Lifecycle Changes and Reviewer Conflicts of Interest

This covers two controls in `RefractGrants` (`grants/`):

* changing a funded grant: amendment, deadline extension, termination
  (issue #280, [`grants/src/lifecycle.rs`](../grants/src/lifecycle.rs));
* reviewer conflict-of-interest (COI) declarations and recusal
  (issue #281, [`grants/src/review.rs`](../grants/src/review.rs)).

The grants crate is the reference state model. "Governance" below means the
address set at construction, which in production is the timelock, so every
governance action passes a proposal and the timelock delay.

## 1. Lifecycle changes

### States

| State | Meaning |
|-------|---------|
| `Funded` | Created; no change requested |
| `AmendmentRequested` / `ExtensionRequested` / `TerminationRequested` | One request is open |
| `Amended` / `Extended` | The latest executed change |
| `Terminated` | Ended early; earned funds can still be claimed and a refund may be pending |
| `Closed` | Nothing left to claim or refund |

New requests can only be filed from `Funded`, `Amended` or `Extended`, so at
most one request is open per grant.

### Flow

1. **Request** (`request_change`) — grantee or governance. Stores the old
   schedule, a reason hash and the requester.
2. **Approve** (`approve_request`) — governance, or an assigned, active,
   unconflicted reviewer who is not the requester. Terminations need
   governance.
3. **Execute** (`execute_request`) — governance only. Fails with
   `StaleRequest` if a milestone was confirmed or the schedule changed after
   the request was filed.

Governance can reject and the requester can withdraw (`close_request`) at any
time before execution; the grant returns to its previous state.

### Rules

| Change | Rules |
|--------|-------|
| Amendment | Replaces only unconfirmed milestones. New amounts > 0, deadlines ordered and not before the last confirmed one. The new pending total may be lower (the difference becomes `pending_refund`) but never higher. Rejected once every milestone is confirmed. |
| Extension | One date per unconfirmed milestone; no date moves earlier, at least one moves later, no deadline is added where there was none. Amounts are unchanged. |
| Termination | Allowed only while funds are escrowed. The next unconfirmed milestone is credited with `earned_bps` of its amount (floored via `refract_math::split_bps`); every other unconfirmed amount, plus the rounding residue, becomes `pending_refund`. |
| Cap | At most `MAX_AMENDMENTS` (8) amendments plus extensions per grant. |

The accounting invariant `sum(milestones) + pending_refund + refunded ==
total_amount` holds after every step; a property test drives random sequences
of operations to check it.

### Refunds

Unearned funds are never sent from the grant to the treasury directly.
`execute_refund` (governance only) moves `pending_refund` through the
`TreasuryRefund` interface. State is updated before the call and rolled back
if the treasury refuses, so a failed refund stays pending and can be retried.
A per-grant in-flight flag rejects re-entry.

### Views

* `current_schedule(grant_id)` — the live schedule.
* `amendment_history(grant_id)` — immutable records of executed changes, each
  with old and new schedule, requester, approver, reason hash and refund.
* `get_request(request_id)`.

### Event examples

An amendment that drops 50 from the remaining schedule, followed by its
refund:

```text
RequestFiled     { grant_id: 0, request_id: 0, kind: "amendment", requester: <grantee> }
RequestApproved  { grant_id: 0, request_id: 0, approver: <timelock> }
GrantAmended     { grant_id: 0, request_id: 0, refund: 50 }
RefundExecuted   { grant_id: 0, amount: 50 }
```

A termination with 25% credit on the in-progress milestone:

```text
RequestFiled     { grant_id: 0, request_id: 1, kind: "termination", requester: <grantee> }
RequestApproved  { grant_id: 0, request_id: 1, approver: <timelock> }
GrantTerminated  { grant_id: 0, request_id: 1, earned: 25, refund: 175 }
RefundFailed     { grant_id: 0, amount: 175 }   // treasury paused; still pending
RefundExecuted   { grant_id: 0, amount: 175 }   // retried
GrantClosed      { grant_id: 0 }                // after the grantee claims
```

## 2. Reviewer conflicts of interest

### Policy

* Governance registers reviewers (`register_reviewer`, role `Reviewer` or
  `LeadReviewer`) and can deactivate them (`set_reviewer_active`).
* Governance assigns reviewers to grants (`assign_reviewer`). Assignment is
  refused for inactive reviewers, for the grantee themselves, and for anyone
  with an unwaived conflict on that grant.
* A reviewer must declare a conflict (`declare_conflict`) as soon as they know
  of one: financial, employment, personal or other, with an optional evidence
  hash. Details stay off-chain.
* Declaring a conflict unassigns the reviewer. If they already reviewed, the
  review is invalidated and the conflict is marked `late`; governance must
  assign a replacement.
* Governance can record an undisclosed conflict (`flag_conflict`, same
  effect) or waive a declared one (`waive_conflict`). Both are recorded as
  overrides. A waived reviewer can be reassigned and review again; the
  invalidated review stays invalid.

### Enforcement

`submit_review`, reviewer approval of lifecycle requests, and the quorum count
all go through one eligibility check: registered, active, assigned, not the
grantee, no unwaived conflict. When governance sets a review quorum
(`set_review_quorum`), `confirm_milestone` fails with `ReviewQuorumNotMet`
until enough eligible reviewers have approved, so a conflicted reviewer's
approval can never release funds.

### Views

`reviewer`, `assigned_reviewers`, `review`, `conflicts_for_grant` (including
waived conflicts, for audit), `has_conflict`, `approval_count`,
`has_review_quorum`.

### Events

```text
ReviewerRegistered    { reviewer, role }
ReviewerStatusChanged { reviewer, active }
ReviewerAssigned      { grant_id, reviewer }
ReviewSubmitted       { grant_id, reviewer, score, approve }
ConflictDeclared      { grant_id, reviewer, kind, evidence_hash }
ReviewerRecused       { grant_id, reviewer, review_invalidated }
ConflictOverride      { grant_id, reviewer, action: Flagged | Waived }
```

Out of scope: identity verification/KYC, legal adjudication of conflicts, and
bounty reviewer conflicts.
