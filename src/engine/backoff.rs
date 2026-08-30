//! Reconnect back-off, exactly as OCPP 2.x Part 4 §5.4 defines it.
//!
//! The algorithm is *not* plain exponential back-off: the wait is
//! `RetryBackOffWaitMinimum + random(0 … RetryBackOffRandomRange)`, doubled after each
//! failed attempt, and the doubling stops after `RetryBackOffRepeatTimes` attempts — from
//! which point the (still randomised) maximum wait is reused. 1.6J leaves the schedule
//! implementation-defined, so the same defaults are used there.

use core::time::Duration;

/// The three `OCPPCommCtrlr` variables that drive reconnect timing (Part 4 §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// `RetryBackOffWaitMinimum` — the wait before the first retry.
    pub wait_minimum: Duration,
    /// `RetryBackOffRandomRange` — the width of the uniform jitter added to every wait.
    pub random_range: Duration,
    /// `RetryBackOffRepeatTimes` — how many times the wait is doubled before it stops
    /// growing.
    pub repeat_times: u32,
}

impl Default for Backoff {
    /// The values OCPP 2.x suggests: 10 s minimum, 10 s of jitter, doubling 3 times.
    fn default() -> Self {
        Self {
            wait_minimum: Duration::from_secs(10),
            random_range: Duration::from_secs(10),
            repeat_times: 3,
        }
    }
}

impl Backoff {
    /// Never waits — for tests and for local development.
    #[must_use]
    pub const fn immediate() -> Self {
        Self {
            wait_minimum: Duration::ZERO,
            random_range: Duration::ZERO,
            repeat_times: 0,
        }
    }

    /// The wait before retry number `attempt` (`0` is the first retry).
    ///
    /// `jitter` must be a uniformly distributed value in `0.0 ..= 1.0`; the caller supplies
    /// it so that the schedule stays deterministic under test and the engine needs no
    /// entropy source of its own.
    #[must_use]
    pub fn delay(&self, attempt: u32, jitter: f64) -> Duration {
        let doublings = attempt.min(self.repeat_times);
        let factor = 1u64 << doublings.min(32);
        let base = self
            .wait_minimum
            .saturating_mul(u32::try_from(factor).unwrap_or(u32::MAX));
        let jitter = jitter.clamp(0.0, 1.0);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let spread = Duration::from_millis((self.random_range.as_millis() as f64 * jitter) as u64);
        base.saturating_add(spread)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_then_plateaus_per_part_4_5_4() {
        let backoff = Backoff::default();
        let seconds = |attempt| backoff.delay(attempt, 0.0).as_secs();
        assert_eq!(seconds(0), 10);
        assert_eq!(seconds(1), 20);
        assert_eq!(seconds(2), 40);
        assert_eq!(seconds(3), 80);
        // RetryBackOffRepeatTimes = 3: no further doubling.
        assert_eq!(seconds(4), 80);
        assert_eq!(seconds(50), 80);
    }

    #[test]
    fn jitter_is_added_on_top_of_every_wait() {
        let backoff = Backoff::default();
        assert_eq!(backoff.delay(0, 1.0).as_secs(), 20);
        assert_eq!(backoff.delay(0, 0.5).as_millis(), 15_000);
        assert_eq!(backoff.delay(3, 1.0).as_secs(), 90);
    }
}
