//! A driver that retries transient failures of another driver.
//!
//! Jev's API docs ask clients to back off on `429 Too Many Requests` and
//! `529 Overloaded`, honouring `retry-after` when the server sends one.
//! Rather than teach every concrete driver that dance, [`Retry`] *wraps* any
//! driver and is itself a [`Driver`] (the decorator pattern), so callers can
//! stack it on top of whatever [`Driver`] they built without knowing it is there.

use std::time::Duration;

use super::{Driver, DriverError};
use crate::wire::{Evaluation, Request};

/// How hard [`Retry`] tries before giving up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total number of calls to the inner driver, *including* the first one.
    /// `1` disables retrying; `0` is treated as `1` (we always try once).
    pub max_attempts: u32,
    /// Delay before the first retry. Doubles on every further retry.
    pub base_delay: Duration,
    /// Upper bound for any single delay, including a server's `retry-after`,
    /// so a misbehaving server cannot make hunch hang for minutes.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// 3 attempts with 500ms and then 1s of backoff: enough to ride out a
    /// short burst without making an interactive CLI feel stuck.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// How long to wait after failed attempt number `attempt` (1-based)
    /// before trying again.
    ///
    /// The server's `retry_after` wins when present, because it knows its own
    /// load better than we do; otherwise back off exponentially
    /// (`base * 2^(attempt-1)`). Either way the result is capped at
    /// `max_delay`. Saturating arithmetic keeps huge attempt counts from
    /// overflowing: they simply hit the cap.
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let delay = retry_after.unwrap_or_else(|| {
            let factor = 1u32
                .checked_shl(attempt.saturating_sub(1))
                .unwrap_or(u32::MAX);
            self.base_delay.saturating_mul(factor)
        });
        delay.min(self.max_delay)
    }
}

/// Wraps a driver `D` and retries rate-limit and overload errors with backoff.
///
/// `S` is the function used to wait between attempts. It defaults to a plain
/// function pointer so the everyday type is just `Retry<D>`; tests swap in a
/// closure via [`Retry::with_sleep`] that records delays instead of sleeping.
pub struct Retry<D, S = fn(Duration)> {
    inner: D,
    policy: RetryPolicy,
    sleep: S,
}

impl<D: Driver> Retry<D> {
    /// Wrap `inner` with the default [`RetryPolicy`], really sleeping between
    /// attempts.
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            policy: RetryPolicy::default(),
            sleep: std::thread::sleep,
        }
    }
}

impl<D: Driver, S: Fn(Duration)> Retry<D, S> {
    /// Replace the retry policy (builder style: consumes and returns `self`).
    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replace how we wait between attempts. Note the return type: swapping
    /// the sleeper changes the `S` type parameter, so this builds a new
    /// `Retry` rather than mutating the old one in place.
    pub fn with_sleep<S2: Fn(Duration)>(self, sleep: S2) -> Retry<D, S2> {
        Retry {
            inner: self.inner,
            policy: self.policy,
            sleep,
        }
    }
}

/// Only errors that mean "try again later" are worth retrying. Anything else
/// (bad key, invalid request, decode error) would fail identically on the
/// next attempt, so retrying would only make the user wait for the same error.
///
/// Returns `None` for "do not retry", or `Some(hint)` where `hint` is the
/// server's optional `retry-after`. The nested `Option` keeps "retryable?"
/// and "did the server say how long?" as two separate questions.
fn retry_hint(error: &DriverError) -> Option<Option<Duration>> {
    match error {
        DriverError::RateLimited { retry_after } => Some(*retry_after),
        DriverError::Overloaded => Some(None),
        _ => None,
    }
}

impl<D: Driver, S: Fn(Duration)> Driver for Retry<D, S> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn default_model(&self) -> &str {
        self.inner.default_model()
    }

    fn evaluate(&self, request: &Request) -> Result<Evaluation, DriverError> {
        let max_attempts = self.policy.max_attempts.max(1);
        let mut attempt = 1;
        loop {
            let error = match self.inner.evaluate(request) {
                Ok(evaluation) => return Ok(evaluation),
                Err(error) => error,
            };
            match retry_hint(&error) {
                Some(server_hint) if attempt < max_attempts => {
                    (self.sleep)(self.policy.delay(attempt, server_hint));
                    attempt += 1;
                }
                // Not retryable, or out of attempts: surface the last error.
                _ => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, VecDeque};

    use super::*;
    use crate::wire::Response;

    /// A driver that replays a fixed script of results, one per call.
    ///
    /// `Driver::evaluate` takes `&self`, yet the fake needs to pop from its
    /// queue and count calls. `RefCell` and `Cell` provide that *interior
    /// mutability*: mutation through a shared reference, checked at runtime
    /// (`RefCell`) or made safe by only ever copying values in and out (`Cell`).
    struct Scripted {
        results: RefCell<VecDeque<Result<Evaluation, DriverError>>>,
        calls: Cell<u32>,
    }

    impl Scripted {
        fn new(results: impl IntoIterator<Item = Result<Evaluation, DriverError>>) -> Self {
            Self {
                results: RefCell::new(results.into_iter().collect()),
                calls: Cell::new(0),
            }
        }
    }

    impl Driver for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn default_model(&self) -> &str {
            "jev-test"
        }

        fn evaluate(&self, _request: &Request) -> Result<Evaluation, DriverError> {
            self.calls.set(self.calls.get() + 1);
            self.results
                .borrow_mut()
                .pop_front()
                .expect("Scripted driver called more often than scripted")
        }
    }

    fn evaluation() -> Evaluation {
        let raw = r#"{
            "model": "jev-1.13.0",
            "answers": { "ok": { "type": "noul", "noul": 1.0 } },
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }"#;
        let response: Response = serde_json::from_str(raw).unwrap();
        Evaluation {
            response,
            raw: raw.to_string(),
        }
    }

    fn request() -> Request {
        Request {
            state: "hello".into(),
            model: "jev-test".into(),
            questions: BTreeMap::new(),
        }
    }

    fn rate_limited(retry_after: Option<Duration>) -> Result<Evaluation, DriverError> {
        Err(DriverError::RateLimited { retry_after })
    }

    fn policy(max_attempts: u32, base_ms: u64, max_ms: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(base_ms),
            max_delay: Duration::from_millis(max_ms),
        }
    }

    /// Run `script` through a `Retry` that records sleeps instead of sleeping.
    /// Returns the outcome, how many times the inner driver was called, and
    /// every delay that would have been slept.
    fn run(
        policy: RetryPolicy,
        script: impl IntoIterator<Item = Result<Evaluation, DriverError>>,
    ) -> (Result<Evaluation, DriverError>, u32, Vec<Duration>) {
        // The sleeper is called through `&self` too, so it records into a
        // `RefCell` it borrows rather than owning a `Vec` it would mutate.
        let slept = RefCell::new(Vec::new());
        let retry = Retry::new(Scripted::new(script))
            .with_policy(policy)
            .with_sleep(|delay| slept.borrow_mut().push(delay));

        let result = retry.evaluate(&request());
        let calls = retry.inner.calls.get();
        drop(retry); // release the closure's borrow of `slept`
        (result, calls, slept.into_inner())
    }

    #[test]
    fn succeeds_first_try_without_sleeping() {
        let (result, calls, slept) = run(RetryPolicy::default(), [Ok(evaluation())]);
        assert_eq!(result, Ok(evaluation()));
        assert_eq!(calls, 1);
        assert!(slept.is_empty());
    }

    #[test]
    fn retries_rate_limit_then_succeeds() {
        let (result, calls, slept) = run(
            policy(3, 100, 1_000),
            [rate_limited(None), Ok(evaluation())],
        );
        assert_eq!(result, Ok(evaluation()));
        assert_eq!(calls, 2);
        assert_eq!(slept, [Duration::from_millis(100)]);
    }

    #[test]
    fn retries_overloaded() {
        let (result, calls, _) = run(
            policy(3, 100, 1_000),
            [Err(DriverError::Overloaded), Ok(evaluation())],
        );
        assert_eq!(result, Ok(evaluation()));
        assert_eq!(calls, 2);
    }

    #[test]
    fn honors_retry_after() {
        let (_, _, slept) = run(
            policy(3, 100, 10_000),
            [rate_limited(Some(Duration::from_secs(2))), Ok(evaluation())],
        );
        assert_eq!(slept, [Duration::from_secs(2)]);
    }

    #[test]
    fn caps_delays_at_max_delay() {
        let (_, _, slept) = run(
            policy(4, 300, 1_000),
            [
                rate_limited(Some(Duration::from_secs(60))),
                rate_limited(None), // 300ms * 2 = 600ms, under the cap
                rate_limited(None), // 300ms * 4 = 1200ms, capped
                Ok(evaluation()),
            ],
        );
        assert_eq!(
            slept,
            [
                Duration::from_millis(1_000),
                Duration::from_millis(600),
                Duration::from_millis(1_000),
            ]
        );
    }

    #[test]
    fn backs_off_exponentially() {
        let (_, _, slept) = run(
            policy(5, 100, 10_000),
            [
                Err(DriverError::Overloaded),
                Err(DriverError::Overloaded),
                Err(DriverError::Overloaded),
                Err(DriverError::Overloaded),
                Ok(evaluation()),
            ],
        );
        assert_eq!(slept, [100, 200, 400, 800].map(Duration::from_millis));
    }

    #[test]
    fn stops_after_max_attempts_with_last_error() {
        let (result, calls, slept) = run(
            policy(3, 100, 1_000),
            [
                Err(DriverError::Overloaded),
                Err(DriverError::Overloaded),
                rate_limited(Some(Duration::from_millis(5))),
            ],
        );
        assert_eq!(
            result,
            Err(DriverError::RateLimited {
                retry_after: Some(Duration::from_millis(5))
            })
        );
        assert_eq!(calls, 3);
        assert_eq!(slept.len(), 2, "no sleep after the final attempt");
    }

    #[test]
    fn does_not_retry_unauthorized() {
        let unauthorized = DriverError::Unauthorized {
            message: "bad key".into(),
        };
        let (result, calls, slept) = run(RetryPolicy::default(), [Err(unauthorized.clone())]);
        assert_eq!(result, Err(unauthorized));
        assert_eq!(calls, 1);
        assert!(slept.is_empty());
    }

    #[test]
    fn zero_max_attempts_still_tries_once() {
        let (result, calls, _) = run(policy(0, 100, 1_000), [rate_limited(None)]);
        assert_eq!(result, rate_limited(None));
        assert_eq!(calls, 1);
    }

    #[test]
    fn exponential_delay_saturates_instead_of_overflowing() {
        let policy = policy(u32::MAX, 500, 8_000);
        assert_eq!(policy.delay(1_000, None), Duration::from_secs(8));
    }

    #[test]
    fn delegates_name_and_model_and_wraps_boxed_drivers() {
        let boxed: Box<dyn Driver> = Box::new(Scripted::new([Ok(evaluation())]));
        let retry = Retry::new(boxed);
        assert_eq!(retry.name(), "scripted");
        assert_eq!(retry.default_model(), "jev-test");
        assert_eq!(retry.evaluate(&request()), Ok(evaluation()));
    }
}
