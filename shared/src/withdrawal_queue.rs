//! Withdrawal queue with pro-rata fills under capital lockdown.
//!
//! Stakers request to withdraw shares. The request is recorded in order, with
//! the time it was made. When the pool cannot cover every request in full, the
//! available capital is split pro rata across the queue. Each request is filled
//! by its share of the total requested, rounded down, so the fills never add up
//! to more than the capital available. The unfilled part stays queued.

/// A staker's handle.
pub type StakerId = u64;

/// One queued withdrawal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub staker: StakerId,
    /// Shares still waiting to be filled.
    pub shares: i128,
    /// Ledger timestamp when the request was made.
    pub requested_at: u64,
}

/// A part of a request that was filled in this round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub id: u64,
    pub shares: i128,
}

/// Why a queue operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// Shares must be positive.
    InvalidShares,
    /// No queued request has this id.
    NotFound,
    /// Only the staker who made a request can cancel it.
    NotOwner,
    /// An intermediate value did not fit in an i128.
    Overflow,
}

/// Withdrawal requests in the order they were made.
#[derive(Debug, Clone, Default)]
pub struct WithdrawalQueue {
    next_id: u64,
    requests: Vec<Request>,
}

impl WithdrawalQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a withdrawal of `shares` and returns its id.
    pub fn request(&mut self, staker: StakerId, shares: i128, now: u64) -> Result<u64, QueueError> {
        if shares <= 0 {
            return Err(QueueError::InvalidShares);
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(QueueError::Overflow)?;
        self.requests.push(Request {
            id,
            staker,
            shares,
            requested_at: now,
        });
        Ok(id)
    }

    /// Queued requests, oldest first.
    pub fn pending(&self) -> &[Request] {
        &self.requests
    }

    /// Total shares still waiting across the queue.
    pub fn total_requested(&self) -> i128 {
        self.requests
            .iter()
            .fold(0i128, |total, request| total.saturating_add(request.shares))
    }

    /// Withdraws a queued request. Only the staker who made it may cancel it.
    pub fn cancel(&mut self, id: u64, staker: StakerId) -> Result<(), QueueError> {
        let index = self
            .requests
            .iter()
            .position(|request| request.id == id)
            .ok_or(QueueError::NotFound)?;
        if self.requests[index].staker != staker {
            return Err(QueueError::NotOwner);
        }
        self.requests.remove(index);
        Ok(())
    }

    /// Fills the queue from `available` shares' worth of capital.
    ///
    /// If `available` covers the whole queue, every request is filled in full
    /// and the queue empties. Otherwise each request is filled by
    /// `shares * available / total_requested`, rounded down, and the rest stays
    /// queued. Rounding down means the fills never add up to more than
    /// `available`. Returns the fills made in this round.
    pub fn fill_pro_rata(&mut self, available: i128) -> Result<Vec<Fill>, QueueError> {
        let total = self.total_requested();
        if available <= 0 || total == 0 {
            return Ok(Vec::new());
        }

        if available >= total {
            let fills = self
                .requests
                .drain(..)
                .map(|request| Fill {
                    id: request.id,
                    shares: request.shares,
                })
                .collect();
            return Ok(fills);
        }

        let mut fills = Vec::new();
        for request in self.requests.iter_mut() {
            let share = request
                .shares
                .checked_mul(available)
                .ok_or(QueueError::Overflow)?
                / total;
            if share > 0 {
                fills.push(Fill {
                    id: request.id,
                    shares: share,
                });
                request.shares -= share;
            }
        }
        Ok(fills)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: StakerId = 1;
    const BOB: StakerId = 2;

    #[test]
    fn requests_get_sequential_ids_and_zero_shares_are_refused() {
        let mut queue = WithdrawalQueue::new();
        assert_eq!(queue.request(ALICE, 100, 10), Ok(0));
        assert_eq!(queue.request(BOB, 300, 11), Ok(1));
        assert_eq!(queue.request(BOB, 0, 12), Err(QueueError::InvalidShares));
        assert_eq!(queue.pending()[0].requested_at, 10);
    }

    #[test]
    fn only_the_owner_can_cancel() {
        let mut queue = WithdrawalQueue::new();
        let id = queue.request(ALICE, 100, 0).unwrap();
        assert_eq!(queue.cancel(id, BOB), Err(QueueError::NotOwner));
        assert_eq!(queue.cancel(99, ALICE), Err(QueueError::NotFound));
        queue.cancel(id, ALICE).unwrap();
        assert!(queue.pending().is_empty());
    }

    #[test]
    fn full_capital_fills_every_request_in_full() {
        let mut queue = WithdrawalQueue::new();
        queue.request(ALICE, 100, 0).unwrap();
        queue.request(BOB, 300, 0).unwrap();

        let fills = queue.fill_pro_rata(1_000).unwrap();
        assert_eq!(
            fills,
            vec![
                Fill { id: 0, shares: 100 },
                Fill { id: 1, shares: 300 },
            ]
        );
        assert!(queue.pending().is_empty());
    }

    #[test]
    fn locked_capital_is_split_pro_rata_and_the_rest_stays_queued() {
        let mut queue = WithdrawalQueue::new();
        queue.request(ALICE, 100, 0).unwrap();
        queue.request(BOB, 300, 0).unwrap();

        // Only 200 of 400 is available, so each request gets half of its shares.
        let fills = queue.fill_pro_rata(200).unwrap();
        assert_eq!(
            fills,
            vec![
                Fill { id: 0, shares: 50 },
                Fill { id: 1, shares: 150 },
            ]
        );
        assert_eq!(queue.pending()[0].shares, 50);
        assert_eq!(queue.pending()[1].shares, 150);
    }

    #[test]
    fn rounding_never_fills_more_than_is_available() {
        let mut queue = WithdrawalQueue::new();
        queue.request(ALICE, 1, 0).unwrap();
        queue.request(BOB, 1, 0).unwrap();
        queue.request(ALICE, 1, 0).unwrap();

        // Two of three shares can be paid, but 2 / 3 rounds down to 0 for each
        // request, so nothing is filled rather than something over-filled.
        let fills = queue.fill_pro_rata(2).unwrap();
        let paid: i128 = fills.iter().map(|fill| fill.shares).sum();
        assert!(paid <= 2);
        assert_eq!(queue.total_requested(), 3);
    }

    #[test]
    fn nothing_is_filled_without_capital() {
        let mut queue = WithdrawalQueue::new();
        queue.request(ALICE, 100, 0).unwrap();
        assert!(queue.fill_pro_rata(0).unwrap().is_empty());
        assert_eq!(queue.total_requested(), 100);
    }
}
