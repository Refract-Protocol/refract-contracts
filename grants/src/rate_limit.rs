//! Per-caller, per-entrypoint fixed-window rate limiting.
//!
//! A caller may make at most `max_calls` calls to one entrypoint in each window
//! of `window_ledgers` ledgers. The check runs before any state is changed, so
//! a refused call leaves nothing behind. Counters reset lazily when a caller
//! next calls in a new window, so no cleanup loop is needed.

use std::collections::BTreeMap;

use crate::Address;

/// Identifier for a public entrypoint that is rate limited.
pub type EntrypointId = u32;

/// At most `max_calls` calls per `window_ledgers` ledgers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max_calls: u32,
    pub window_ledgers: u32,
}

/// Why a call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitError {
    /// The caller has used up this window's calls to this entrypoint.
    Limited,
    /// A limit must allow at least one call per non-empty window.
    InvalidLimit,
}

/// Fixed-window limiter keyed by `(caller, entrypoint)`.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    /// Used for any entrypoint without its own limit.
    default_limit: Limit,
    /// Per-entrypoint overrides. Governance sets these.
    limits: BTreeMap<EntrypointId, Limit>,
    /// Window index and calls used so far in that window.
    counters: BTreeMap<(Address, EntrypointId), (u32, u32)>,
}

impl RateLimiter {
    /// Creates a limiter where every entrypoint uses `default_limit` until it is
    /// overridden with [`RateLimiter::set_limit`].
    pub fn new(default_limit: Limit) -> Self {
        Self {
            default_limit,
            limits: BTreeMap::new(),
            counters: BTreeMap::new(),
        }
    }

    /// Sets the limit for one entrypoint.
    pub fn set_limit(&mut self, entrypoint: EntrypointId, limit: Limit) -> Result<(), RateLimitError> {
        validate(limit)?;
        self.limits.insert(entrypoint, limit);
        Ok(())
    }

    /// The limit that applies to `entrypoint`.
    pub fn limit_for(&self, entrypoint: EntrypointId) -> Limit {
        self.limits.get(&entrypoint).copied().unwrap_or(self.default_limit)
    }

    /// Records one call by `caller` to `entrypoint` at `ledger`. Returns
    /// `Limited` without recording the call when the caller is over the limit.
    pub fn check_and_record(
        &mut self,
        caller: Address,
        entrypoint: EntrypointId,
        ledger: u32,
    ) -> Result<(), RateLimitError> {
        let limit = self.limit_for(entrypoint);
        validate(limit)?;
        let window = ledger / limit.window_ledgers;

        let counter = self
            .counters
            .entry((caller, entrypoint))
            .or_insert((window, 0));
        if counter.0 != window {
            *counter = (window, 0);
        }
        if counter.1 >= limit.max_calls {
            return Err(RateLimitError::Limited);
        }
        counter.1 += 1;
        Ok(())
    }
}

fn validate(limit: Limit) -> Result<(), RateLimitError> {
    if limit.max_calls == 0 || limit.window_ledgers == 0 {
        return Err(RateLimitError::InvalidLimit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Address = 1;
    const BOB: Address = 2;
    const SUBMIT: EntrypointId = 10;
    const APPLY: EntrypointId = 11;
    const LIMIT: Limit = Limit {
        max_calls: 2,
        window_ledgers: 100,
    };

    #[test]
    fn refuses_calls_over_the_limit_within_a_window() {
        let mut limiter = RateLimiter::new(LIMIT);
        assert_eq!(limiter.check_and_record(ALICE, SUBMIT, 0), Ok(()));
        assert_eq!(limiter.check_and_record(ALICE, SUBMIT, 50), Ok(()));
        assert_eq!(
            limiter.check_and_record(ALICE, SUBMIT, 99),
            Err(RateLimitError::Limited)
        );
    }

    #[test]
    fn a_new_window_resets_the_count() {
        let mut limiter = RateLimiter::new(LIMIT);
        limiter.check_and_record(ALICE, SUBMIT, 0).unwrap();
        limiter.check_and_record(ALICE, SUBMIT, 0).unwrap();
        assert_eq!(
            limiter.check_and_record(ALICE, SUBMIT, 0),
            Err(RateLimitError::Limited)
        );
        assert_eq!(limiter.check_and_record(ALICE, SUBMIT, 100), Ok(()));
    }

    #[test]
    fn callers_and_entrypoints_are_counted_separately() {
        let mut limiter = RateLimiter::new(LIMIT);
        limiter.check_and_record(ALICE, SUBMIT, 0).unwrap();
        limiter.check_and_record(ALICE, SUBMIT, 0).unwrap();

        // Another caller is unaffected.
        assert_eq!(limiter.check_and_record(BOB, SUBMIT, 0), Ok(()));
        // The same caller is unaffected on another entrypoint.
        assert_eq!(limiter.check_and_record(ALICE, APPLY, 0), Ok(()));
    }

    #[test]
    fn a_refused_call_is_not_counted() {
        let mut limiter = RateLimiter::new(LIMIT);
        limiter.check_and_record(ALICE, SUBMIT, 0).unwrap();
        limiter.check_and_record(ALICE, SUBMIT, 0).unwrap();
        assert_eq!(
            limiter.check_and_record(ALICE, SUBMIT, 0),
            Err(RateLimitError::Limited)
        );
        // Still refused, and the refusal did not move the window forward.
        assert_eq!(
            limiter.check_and_record(ALICE, SUBMIT, 1),
            Err(RateLimitError::Limited)
        );
    }

    #[test]
    fn per_entrypoint_limits_override_the_default() {
        let mut limiter = RateLimiter::new(LIMIT);
        limiter
            .set_limit(
                APPLY,
                Limit {
                    max_calls: 1,
                    window_ledgers: 100,
                },
            )
            .unwrap();
        assert_eq!(limiter.limit_for(APPLY).max_calls, 1);
        assert_eq!(limiter.limit_for(SUBMIT).max_calls, 2);

        limiter.check_and_record(ALICE, APPLY, 0).unwrap();
        assert_eq!(
            limiter.check_and_record(ALICE, APPLY, 0),
            Err(RateLimitError::Limited)
        );
    }

    #[test]
    fn zero_limits_are_rejected() {
        let mut limiter = RateLimiter::new(LIMIT);
        assert_eq!(
            limiter.set_limit(
                APPLY,
                Limit {
                    max_calls: 0,
                    window_ledgers: 100,
                }
            ),
            Err(RateLimitError::InvalidLimit)
        );
        assert_eq!(
            RateLimiter::new(Limit {
                max_calls: 1,
                window_ledgers: 0,
            })
            .check_and_record(ALICE, SUBMIT, 0),
            Err(RateLimitError::InvalidLimit)
        );
    }
}
