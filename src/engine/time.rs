//! Monotonic time for the sans-I/O engine.
//!
//! The engine never reads a clock. The driver supplies the current instant to every
//! [`Engine::handle`](super::Engine::handle) — and to every application call it makes — which
//! is what makes timeouts, retry schedules and boot back-off testable in microseconds instead
//! of minutes, and what stops a deadline from ever being computed against a clock that has
//! not moved since the last timer fired.

use core::fmt;
use core::ops::Add;
use core::time::Duration;

/// A point on a monotonic clock, in milliseconds since an arbitrary origin.
///
/// Only differences are meaningful. The origin is whatever the driver chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant {
    millis: u64,
}

impl Instant {
    /// The origin of the driver's clock.
    pub const ZERO: Instant = Instant { millis: 0 };

    /// An instant `millis` milliseconds after the origin.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    /// Milliseconds since the origin.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.millis
    }

    /// How long ago `earlier` was; zero if `earlier` is in the future.
    #[must_use]
    pub const fn saturating_duration_since(self, earlier: Instant) -> Duration {
        Duration::from_millis(self.millis.saturating_sub(earlier.millis))
    }

    /// This instant advanced by `duration`, saturating at the end of the clock.
    #[must_use]
    pub fn saturating_add(self, duration: Duration) -> Self {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        Self {
            millis: self.millis.saturating_add(millis),
        }
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, duration: Duration) -> Instant {
        self.saturating_add(duration)
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "+{}ms", self.millis)
    }
}

/// The timers the engine asks the driver to run.
///
/// The driver only has to keep one deadline per variant; the engine re-arms and clears them
/// explicitly, and a spurious [`Input::Timeout`](super::Input::Timeout) is always harmless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Timer {
    /// The outstanding `CALL` must be answered before this deadline.
    CallTimeout,
    /// Time to send a `Heartbeat`.
    Heartbeat,
    /// Time to re-send `BootNotification` (B02.FR.04 / FR.07 / FR.08).
    BootRetry,
    /// Time to retry the transaction message at the head of the queue.
    TransactionRetry,
    /// A graceful shutdown must complete by this deadline.
    DrainDeadline,
}
