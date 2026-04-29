//! Retry policy with exponential backoff for the solve dispatch.
//!
//! Configure once on the builder; applies to every [`crate::Antibot::execute`]
//! call. Retries do not re-run the cache or coalescer — they retry only the
//! provider round-trip.

use crate::error::AntibotError;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first try. `1` disables retries.
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    /// Exponential multiplier applied between attempts.
    pub multiplier: f64,
    /// Add ±25% jitter to the computed backoff.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            multiplier: 2.0,
            jitter: true,
        }
    }
}

impl RetryPolicy {
    pub fn no_retries() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// Backoff before attempt `n` (1-indexed). Returns `Duration::ZERO` for n=1.
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let exp = (attempt - 1) as i32 - 1;
        let scale = self.multiplier.powi(exp.max(0));
        let nanos = self.initial_delay.as_nanos() as f64 * scale;
        let mut delay = Duration::from_nanos(nanos.min(u64::MAX as f64) as u64);
        if delay > self.max_delay {
            delay = self.max_delay;
        }
        if self.jitter {
            delay = apply_jitter(delay);
        }
        delay
    }

    /// Whether `err` should trigger a retry.
    pub fn is_retryable(&self, err: &AntibotError) -> bool {
        matches!(
            err,
            AntibotError::Http(_)
                | AntibotError::UnexpectedResponse(_)
                | AntibotError::ChallengeFailed { .. }
        )
    }
}

fn apply_jitter(d: Duration) -> Duration {
    // Cheap deterministic-ish jitter without bringing in `rand`.
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0) as u64;
    let frac = (seed % 1000) as f64 / 1000.0; // 0.0..1.0
    let factor = 0.75 + 0.5 * frac; // 0.75..1.25
    let nanos = (d.as_nanos() as f64 * factor) as u64;
    Duration::from_nanos(nanos)
}
