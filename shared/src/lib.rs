//! Pure logic shared by the Refract governance-adjacent contracts.
//!
//! Nothing here depends on the Soroban SDK, so each module can be unit tested
//! directly. Contracts map their own address type onto the `u64` handles used
//! here.

pub mod allowlist;
pub mod facility;
pub mod guardians;
pub mod loss_event;
pub mod version;
pub mod withdrawal_queue;

#[cfg(test)]
mod withdrawal_queue_tests {
    use crate::withdrawal_queue::{WithdrawalQueue, WithdrawalQueueError};

    #[test]
    fn empty_batch_is_rejected() {
        let mut queue = WithdrawalQueue::new();
        let result = queue.process_batch(&[]);
        assert_eq!(result, Err(WithdrawalQueueError::EmptyBatch));
    }
}
