//! End-to-end flow: governance proposal -> timelock -> grants -> treasury.
//!
//! The grants crate is a plain-Rust state model, so the timelock is modelled
//! here as a queue of delayed actions executed with the timelock's own
//! address, which is what `RefractGrants::governance` is set to in
//! production.

#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

mod common;

use common::*;
use refract_grants::*;

const TIMELOCK: Address = GOV;
const DELAY: u64 = 172_800; // 48h

enum Action {
    Approve(RequestId),
    Execute(RequestId),
    Refund(GrantId),
}

struct Timelock {
    now: u64,
    queue: Vec<(u64, Action)>,
}

impl Timelock {
    /// A passed governance proposal queues `action` behind the delay.
    fn queue(&mut self, action: Action) {
        self.queue.push((self.now + DELAY, action));
    }

    /// Execute every action whose delay has elapsed, in order.
    fn execute_ready(
        &mut self,
        grants: &mut RefractGrants,
        treasury: &mut Treasury,
    ) -> Vec<Result<(), GrantsError>> {
        let now = self.now;
        let (ready, waiting): (Vec<_>, Vec<_>) =
            self.queue.drain(..).partition(|(eta, _)| *eta <= now);
        self.queue = waiting;
        ready
            .into_iter()
            .map(|(_, action)| match action {
                Action::Approve(r) => grants.approve_request(TIMELOCK, r),
                Action::Execute(r) => grants.execute_request(TIMELOCK, r),
                Action::Refund(id) => grants.execute_refund(TIMELOCK, id, treasury).map(|_| ()),
            })
            .collect()
    }
}

#[test]
fn termination_flows_from_governance_through_timelock_to_treasury() {
    let (mut grants, id) = funded();
    let mut treasury = Treasury::default();
    let mut timelock = Timelock {
        now: 0,
        queue: Vec::new(),
    };

    grants.confirm_milestone(TIMELOCK, id, 0).unwrap();
    assert_eq!(grants.claim(GRANTEE, id), Ok(100));

    // Grantee asks to wind down; governance votes to approve and execute.
    let r = grants
        .request_change(
            GRANTEE,
            id,
            RequestKind::Termination { earned_bps: 5_000 },
            REASON,
        )
        .unwrap();
    timelock.queue(Action::Approve(r));
    timelock.queue(Action::Execute(r));
    timelock.queue(Action::Refund(id));

    // Nothing happens before the delay.
    assert!(timelock
        .execute_ready(&mut grants, &mut treasury)
        .is_empty());
    assert_eq!(
        grants.get_grant(id).unwrap().status,
        GrantStatus::TerminationRequested
    );

    // First attempt: the treasury is paused and refuses the refund.
    treasury.fail = true;
    timelock.now += DELAY;
    let results = timelock.execute_ready(&mut grants, &mut treasury);
    assert_eq!(
        results,
        vec![Ok(()), Ok(()), Err(GrantsError::RefundFailed)]
    );
    let grant = grants.get_grant(id).unwrap();
    assert_eq!(grant.status, GrantStatus::Terminated);
    assert_eq!(grant.pending_refund, 150);
    assert_eq!(grant.claimable, 50);

    // Governance re-queues the refund once the treasury is back.
    treasury.fail = false;
    timelock.queue(Action::Refund(id));
    timelock.now += DELAY;
    assert_eq!(
        timelock.execute_ready(&mut grants, &mut treasury),
        vec![Ok(())]
    );
    assert_eq!(treasury.balance, 150);

    assert_eq!(grants.claim(GRANTEE, id), Ok(50));
    let grant = grants.get_grant(id).unwrap();
    assert_eq!(grant.status, GrantStatus::Closed);
    assert!(grant.is_conserved());
    // 100 + 50 to the grantee, 150 back to the treasury.
    assert_eq!(100 + 50 + treasury.balance, grant.total_amount);
    assert_eq!(grants.amendment_history(id).len(), 1);
}
