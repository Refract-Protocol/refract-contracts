# Grant Rejection Appeals and Re-Review Escalation (issue #282)

`RefractGrants` is now a Soroban contract (it was previously a plain-Rust
model with no token, storage or auth). It adds applications, an initial
review, and an appeals path for rejected applications. The milestone rules
are unchanged: amounts sum to the total, there is at least one milestone,
and confirmation happens strictly in order.

## Application lifecycle

```text
Pending ──(all reviewers voted)──▶ Approved ──fund_application──▶ Funded
   │                                   
   └──────────────────────────────▶ Rejected ──submit_appeal──▶ AppealSubmitted
                                       ▲                              │ assign_appeal_panel
                                       │ reopen_appeal (timelock)     ▼
                                    Upheld ◀── panel majority ── UnderReReview
                                       ▲                              │ tie / escalate votes
                                       │                              ▼ or stall timeout
                     final_decision ───┴── Escalated ──final_decision──▶ Overturned ──▶ Funded
                       (timelock)                         (timelock)
```

`Withdrawn` (applicant) and `Funded` are terminal.

## Rules

| Rule | Value / behaviour |
|---|---|
| Initial decision | strict majority approves; a tie or majority against rejects |
| Appeal window | `APPEAL_WINDOW_SECS` = 14 days from the decision; the last second is inclusive |
| Appeals per application | 1; the timelock can grant another with `reopen_appeal`, which opens a fresh window |
| Panel independence | no original reviewer and not the applicant; 1–7 members, no duplicates |
| Panel outcome | strict majority overturn → `Overturned`; strict majority uphold → `Upheld`; otherwise → `Escalated` |
| Reviewer unavailability | governance can replace the panel mid-review (bumps `panel_version`, discards earlier votes); after `REREVIEW_TIMEOUT_SECS` (21 days) anyone can `escalate_stalled` |
| Binding decision | `final_decision(overturn, reason_hash)`, registry `Timelock` only. Resolves `Escalated` or overrides a panel outcome |
| Timelock delay | `final_decision` and `reopen_appeal` accept only the timelock, so escalation can never bypass the delay |

## Audit trail

The original decision (`Application` plus per-reviewer `ReviewRecord`) and
each appeal round (`Appeal` plus per-member `AppealVoteRecord`, keyed by
round and panel version) are stored under separate keys. Nothing is
overwritten when an appeal is filed, re-reviewed, escalated or decided.

## Events

| Topic | When |
|---|---|
| `app_new`, `rev_asgn`, `review`, `decided`, `app_wdrn`, `funded` | application lifecycle |
| `appeal` | appeal submitted (round, evidence hash) |
| `ap_panel` | panel assigned or replaced (round, version, members) |
| `ap_vote` | panel vote |
| `ap_result` | panel upheld / overturned |
| `ap_escal` | escalated (split panel or stall) |
| `ap_final` | binding timelock decision (round, outcome, reason hash) |
| `ap_reopen` | timelock granted another appeal |

Views: `get_application`, `get_review`, `get_appeal`, `get_appeal_vote`, and
`appeal_status` (status, round, appeals used / allowed, window end,
finalized).

## Tests

* `grants/src/test.rs`: window boundaries (last second accepted, +1
  rejected), ineligible panel members, duplicate appeals, reopen,
  governance override, split-panel escalation, panel replacement and stall
  escalation, terminal-state invariants, and separation of the audit trail.
* `grants/tests/appeal_e2e.rs`: rejection → appeal → independent panel
  escalates → direct governance decision refused → timelock-queued
  `final_decision` refused before ETA and applied after → funded → milestone
  release to a frozen recipient → paid on unfreeze.

Out of scope: bounty dispute resolution, legal appeals, and re-opening
applications the applicant withdrew.
