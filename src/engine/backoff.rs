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
    /// The caller supplies the [`Jitter`] so that the schedule stays deterministic under test
    /// and the engine needs no entropy source of its own.
    #[must_use]
    pub const fn delay(&self, attempt: u32, jitter: Jitter) -> Duration {
        let doublings = if attempt < self.repeat_times {
            attempt
        } else {
            self.repeat_times
        };
        let factor = 1u64 << if doublings < 32 { doublings } else { 32 };
        // The doubling is capped at 32, so `factor` fits a u32 long before the guard below
        // is reached; the guard is what makes that a fact rather than an assumption.
        #[allow(clippy::cast_possible_truncation)]
        let base = self
            .wait_minimum
            .saturating_mul(if factor > u32::MAX as u64 {
                u32::MAX
            } else {
                factor as u32
            });
        let spread = jitter.of_millis(self.random_range.as_millis());
        base.saturating_add(Duration::from_millis(spread))
    }
}

/// How much of a back-off's random range to add, as a fraction of `u32::MAX`.
///
/// Integer rather than floating point, so the schedule a test asserts is the schedule that
/// runs: `Jitter::from_ratio(1, 2)` is exactly half the range on every platform, which
/// `0.5f64` multiplied into a duration is not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Jitter(u32);

impl Jitter {
    /// No jitter — the shortest wait §5.4 allows.
    pub const NONE: Self = Self(0);
    /// The whole random range.
    pub const FULL: Self = Self(u32::MAX);

    /// A uniformly drawn `u32` — four bytes straight from an entropy source — as a jitter.
    #[must_use]
    pub const fn from_random_u32(value: u32) -> Self {
        Self(value)
    }

    /// `numerator / denominator` of the range, clamped to `0 ..= 1`.
    ///
    /// # Panics
    ///
    /// If `denominator` is zero.
    #[must_use]
    pub const fn from_ratio(numerator: u32, denominator: u32) -> Self {
        assert!(denominator != 0, "a jitter ratio needs a denominator");
        if numerator >= denominator {
            return Self::FULL;
        }
        // Rounded, so `from_ratio(1, 2)` is exactly half a range of any width.
        let scaled =
            (numerator as u64 * u32::MAX as u64 + denominator as u64 / 2) / denominator as u64;
        // `numerator < denominator` by the branch above, so the quotient is below `u32::MAX`.
        #[allow(clippy::cast_possible_truncation)]
        Self(scaled as u32)
    }

    /// This fraction of `millis`, rounded to the nearest millisecond.
    const fn of_millis(self, millis: u128) -> u64 {
        let whole = u32::MAX as u128;
        let scaled = (millis * self.0 as u128 + whole / 2) / whole;
        #[allow(clippy::cast_possible_truncation)]
        if scaled > u64::MAX as u128 {
            u64::MAX
        } else {
            scaled as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_then_plateaus_per_part_4_5_4() {
        let backoff = Backoff::default();
        let seconds = |attempt| backoff.delay(attempt, Jitter::NONE).as_secs();
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
        assert_eq!(backoff.delay(0, Jitter::FULL).as_secs(), 20);
        assert_eq!(
            backoff.delay(0, Jitter::from_ratio(1, 2)).as_millis(),
            15_000
        );
        assert_eq!(backoff.delay(3, Jitter::FULL).as_secs(), 90);
    }
}
