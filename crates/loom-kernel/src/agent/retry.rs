//! Model-request retry policy (architecture.md §6.3).
//!
//! Model requests are side-effect free, so a transient failure may be
//! retried even after streaming has begun. Retryability is decided from the
//! [`ModelErrorKind`](crate::model::ModelErrorKind) classification and the
//! request's state — the attempts used and whether the task was cancelled —
//! never from whether reasoning or content deltas were already observed.
//! Thinking models stream reasoning before the final answer, so a
//! first-token retry barrier would turn most mid-stream transport drops into
//! failed tasks.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::ModelError;

/// Bounded, classification-based retry policy for one model request (§6.3).
///
/// The policy is consulted once per failed attempt: [`should_retry`] decides
/// whether another attempt is allowed, and [`delay`] returns the backoff to
/// wait first, honouring a provider `retry_after` hint within `max_backoff`.
///
/// [`should_retry`]: RetryPolicy::should_retry
/// [`delay`]: RetryPolicy::delay
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    max_attempts: u32,
    initial_backoff: Duration,
    backoff_factor: u32,
    max_backoff: Duration,
    jitter_fraction: f64,
}

impl Default for RetryPolicy {
    /// The Iteration 1 defaults from §6.3: four total attempts per request;
    /// exponential backoff from 2 s with factor 2, capped at 30 s; ±25 %
    /// jitter.
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff: Duration::from_secs(2),
            backoff_factor: 2,
            max_backoff: Duration::from_secs(30),
            jitter_fraction: 0.25,
        }
    }
}

impl RetryPolicy {
    /// Creates the default policy (see [`RetryPolicy::default`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the total number of attempts allowed per request (minimum 1).
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Sets the backoff before the first retry.
    #[must_use]
    pub fn with_initial_backoff(mut self, initial_backoff: Duration) -> Self {
        self.initial_backoff = initial_backoff;
        self
    }

    /// Sets the exponential growth factor between consecutive retries
    /// (minimum 1).
    #[must_use]
    pub fn with_backoff_factor(mut self, backoff_factor: u32) -> Self {
        self.backoff_factor = backoff_factor.max(1);
        self
    }

    /// Sets the upper bound on the computed backoff.
    #[must_use]
    pub fn with_max_backoff(mut self, max_backoff: Duration) -> Self {
        self.max_backoff = max_backoff;
        self
    }

    /// Sets the jitter as a fraction of the computed backoff. `0.0` disables
    /// jitter, which tests use for deterministic timing.
    #[must_use]
    pub fn with_jitter_fraction(mut self, jitter_fraction: f64) -> Self {
        self.jitter_fraction = if jitter_fraction.is_finite() {
            jitter_fraction.max(0.0)
        } else {
            0.0
        };
        self
    }

    /// Total attempts allowed per request.
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Whether `error` is retryable and `attempt` (1-based, the attempt that
    /// just failed) leaves budget for another try.
    ///
    /// Request state is part of the decision (§6.3): a non-retryable kind is
    /// never retried, and the final allowed attempt never starts another —
    /// the loop owns the separate check for a fired cancellation token.
    #[must_use]
    pub fn should_retry(&self, attempt: u32, error: &ModelError) -> bool {
        attempt < self.max_attempts && error.kind.is_retryable()
    }

    /// The backoff to wait after `attempt` failed, capped and jittered.
    ///
    /// A provider `retry_after` hint overrides the computed backoff and is
    /// used unjittered: it is the server telling us when to come back
    /// (§6.3). The hint is still clamped to [`RetryPolicy::with_max_backoff`]
    /// so a throttling provider cannot park a request indefinitely: an
    /// uncapped hint can otherwise sleep past the caller's whole budget
    /// (observed as 30-minute eval stalls, 2026-10-16).
    #[must_use]
    pub fn delay(&self, attempt: u32, error: &ModelError) -> Duration {
        if let Some(retry_after) = error.retry_after {
            return retry_after.min(self.max_backoff);
        }
        self.jitter(self.backoff(attempt))
    }

    /// The growth curve before jitter and the `retry_after` override, for
    /// the retry following a failed `attempt`.
    ///
    /// Starts at [`RetryPolicy::with_initial_backoff`] and multiplies by
    /// [`RetryPolicy::with_backoff_factor`] per attempt, clamped to
    /// [`RetryPolicy::with_max_backoff`]. Exposed so tests can assert the
    /// curve exactly while `delay` remains random.
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let mut millis = self.initial_backoff.as_millis();
        let cap = self.max_backoff.as_millis();
        for _ in 1..attempt {
            millis = millis.saturating_mul(u128::from(self.backoff_factor));
            if millis >= cap {
                return self.max_backoff;
            }
        }
        let computed = Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX));
        computed.min(self.max_backoff)
    }

    /// Applies multiplicative jitter uniformly in
    /// `[1 - fraction, 1 + fraction)` to `base`.
    fn jitter(&self, base: Duration) -> Duration {
        if self.jitter_fraction <= 0.0 {
            return base;
        }
        let base_millis = base.as_secs_f64() * 1_000.0;
        // Uniform in [-1, 1).
        let offset = jitter_unit() * 2.0 - 1.0;
        let scaled = base_millis * (1.0 + offset * self.jitter_fraction);
        if !scaled.is_finite() || scaled <= 0.0 {
            return base;
        }
        Duration::from_secs_f64(scaled / 1_000.0)
    }
}

/// A pseudorandom value in `[0, 1)` derived from the system clock.
///
/// Jitter only needs to decorrelate concurrent retriers, not be
/// cryptographic, so a cheap mix of the clock's subsecond nanoseconds is
/// enough — and it avoids pulling an RNG dependency into the core.
fn jitter_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let mixed = nanos.wrapping_mul(2_654_435_761);
    f64::from(mixed % 1_000_003) / 1_000_003.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelError, ModelErrorKind};
    use std::time::Duration;

    fn error(kind: ModelErrorKind) -> ModelError {
        ModelError::new(kind, "test")
    }

    #[test]
    fn defaults_match_the_architecture() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts(), 4);
        assert_eq!(policy.backoff(1), Duration::from_secs(2));
        assert_eq!(policy.backoff(2), Duration::from_secs(4));
        assert_eq!(policy.backoff(3), Duration::from_secs(8));
        assert_eq!(policy.backoff(4), Duration::from_secs(16));
        // Clamped: 2s * 2^4 = 32s would exceed the 30s cap.
        assert_eq!(policy.backoff(5), Duration::from_secs(30));
        assert_eq!(policy.backoff(50), Duration::from_secs(30));
    }

    #[test]
    fn should_retry_respects_kind_and_attempt_budget() {
        let policy = RetryPolicy::default();
        assert!(policy.should_retry(1, &error(ModelErrorKind::RateLimited)));
        assert!(policy.should_retry(3, &error(ModelErrorKind::Transport)));
        // Last allowed attempt: no budget left.
        assert!(!policy.should_retry(4, &error(ModelErrorKind::Transport)));
        assert!(!policy.should_retry(5, &error(ModelErrorKind::Transport)));
        // Non-retryable kinds never retry.
        assert!(!policy.should_retry(1, &error(ModelErrorKind::Auth)));
        assert!(!policy.should_retry(1, &error(ModelErrorKind::InvalidRequest)));
        assert!(!policy.should_retry(1, &error(ModelErrorKind::ContextOverflow)));
        assert!(!policy.should_retry(1, &error(ModelErrorKind::QuotaExhausted)));
        assert!(!policy.should_retry(1, &error(ModelErrorKind::Protocol)));
        assert!(!policy.should_retry(1, &error(ModelErrorKind::Cancelled)));
    }

    #[test]
    fn retry_after_overrides_the_computed_backoff() {
        let policy = RetryPolicy::default();
        let with_hint =
            error(ModelErrorKind::RateLimited).with_retry_after(Duration::from_millis(123));
        assert_eq!(policy.delay(1, &with_hint), Duration::from_millis(123));
    }

    #[test]
    fn retry_after_is_clamped_to_max_backoff() {
        // A throttling provider must not be able to park a request past the
        // caller's budget: an uncapped hint caused 30-minute eval stalls.
        let policy = RetryPolicy::default(); // max_backoff = 30 s
        let huge = error(ModelErrorKind::RateLimited).with_retry_after(Duration::from_secs(3600));
        assert_eq!(policy.delay(1, &huge), Duration::from_secs(30));
    }

    #[test]
    fn delay_stays_within_the_jitter_band() {
        let policy = RetryPolicy::default();
        let base = policy.backoff(2); // 4 s
        let base_millis = base.as_millis() as f64;
        let low = base_millis * 0.75;
        let high = base_millis * 1.25;
        for _ in 0..64 {
            let delay = policy.delay(2, &error(ModelErrorKind::Server));
            let millis = delay.as_secs_f64() * 1_000.0;
            assert!(
                millis >= low && millis <= high,
                "delay {millis} ms outside [{low}, {high}]"
            );
        }
    }

    #[test]
    fn zero_jitter_is_deterministic() {
        let policy = RetryPolicy::default().with_jitter_fraction(0.0);
        let a = policy.delay(3, &error(ModelErrorKind::Overloaded));
        let b = policy.delay(3, &error(ModelErrorKind::Overloaded));
        assert_eq!(a, Duration::from_secs(8));
        assert_eq!(a, b);
    }

    #[test]
    fn builders_clamp_invalid_values() {
        let policy = RetryPolicy::new()
            .with_max_attempts(0)
            .with_backoff_factor(0)
            .with_jitter_fraction(-1.0);
        assert_eq!(policy.max_attempts(), 1);
        // Factor clamps to 1, so the backoff is flat at the initial value.
        assert_eq!(policy.backoff(1), Duration::from_secs(2));
        assert_eq!(policy.backoff(5), Duration::from_secs(2));
        // Jitter clamps to 0, so delay equals the backoff.
        assert_eq!(
            policy.delay(1, &error(ModelErrorKind::Server)),
            Duration::from_secs(2)
        );
    }
}
