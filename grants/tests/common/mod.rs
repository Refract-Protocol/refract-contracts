#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.
#![allow(dead_code)]

use refract_grants::*;

pub const GOV: Address = 1;
pub const GRANTEE: Address = 2;
pub const STRANGER: Address = 3;
pub const REVIEWER_A: Address = 10;
pub const REVIEWER_B: Address = 11;
pub const REVIEWER_C: Address = 12;
pub const REASON: [u8; 32] = [7; 32];

/// In-memory treasury ledger that can be told to refuse refunds.
#[derive(Default)]
pub struct Treasury {
    pub balance: i128,
    pub fail: bool,
}

impl TreasuryRefund for Treasury {
    fn receive_refund(&mut self, _grant_id: GrantId, amount: i128) -> Result<(), RefundFailure> {
        if self.fail {
            return Err(RefundFailure);
        }
        self.balance += amount;
        Ok(())
    }
}

pub fn term(amount: i128, due: u64) -> MilestoneTerm {
    MilestoneTerm { amount, due }
}

/// 300 split 100/100/100, due at 10/20/30.
pub fn funded() -> (RefractGrants, GrantId) {
    let mut g = RefractGrants::new(GOV);
    let id = g
        .create_grant_with_schedule(
            GOV,
            GRANTEE,
            300,
            vec![term(100, 10), term(100, 20), term(100, 30)],
        )
        .unwrap();
    (g, id)
}

pub fn status(g: &RefractGrants, id: GrantId) -> GrantStatus {
    g.get_grant(id).unwrap().status
}
