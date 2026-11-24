//! Budget enforcement — O(1) check using the running total on the program
//! record rather than a full scan of all requests.

use soroban_sdk::Env;

use crate::{DataKey, GrantsError, Program};

/// Check whether adding `amount` to the program's committed total would
/// exceed its budget.
///
/// Returns `Ok(new_committed)` with the value that should be written back
/// to storage if the request is accepted, or `Err(GrantsError::BudgetExceeded)`
/// if it would overflow.
///
/// This is O(1): it reads the program record (a single storage entry) and
/// does arithmetic — no iteration over requests.
pub fn check_budget(
    env: &Env,
    program_id: u64,
    amount: i128,
) -> Result<i128, crate::GrantsError> {
    let program: Program = env
        .storage()
        .persistent()
        .get(&DataKey::Program(program_id))
        .ok_or(GrantsError::ProgramNotFound)?;

    let new_committed = program.committed + amount;
    if new_committed > program.budget {
        return Err(GrantsError::BudgetExceeded);
    }
    Ok(new_committed)
}
