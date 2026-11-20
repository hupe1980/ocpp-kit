//! A per-connection inbound message rate.
//!
//! OCPP defines no message rate. A CSMS that is open to the internet still needs one: the
//! concurrency bounds ([`EngineConfig::max_peer_requests`](super::EngineConfig::max_peer_requests),
//! the transport's handler slots) limit how much is *in flight*, not how *often* an
//! authenticated station may ask, and every admitted call reaches the application's handler
//! and its database.
//!
//! The limiter is the generic cell rate algorithm (GCRA), which is a token bucket kept as a
//! single timestamp: integer microseconds, no floating point (principle III applies to every
//! number the crate decides with), and no allocation.

use core::fmt;
use core::time::Duration;

use super::Instant;

/// How often a peer may send unprompted messages: `count` per `period`, with bursts of up to
/// `burst` at once.
///
/// Every constructor refuses a zero, so a `RateLimit` that exists is one that admits
/// something — a count of zero is a configuration mistake, not a request to refuse every
/// station.
///
/// ```
/// # use ocpp_kit::engine::RateLimit;
/// // 120 a minute, sustained; up to 20 back to back (a boot's StatusNotifications).
/// let rate = RateLimit::per_minute(120)?.with_burst(20)?;
/// assert_eq!(rate.burst(), 20);
/// assert!(RateLimit::per_minute(0).is_err());
/// # Ok::<(), ocpp_kit::engine::InvalidRateLimit>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    count: u32,
    period: Duration,
    burst: u32,
}

impl RateLimit {
    /// `count` messages per `period`, in bursts of up to `count`.
    ///
    /// # Errors
    ///
    /// [`InvalidRateLimit`] when `count` is zero or `period` is shorter than a microsecond.
    pub const fn new(count: u32, period: Duration) -> Result<Self, InvalidRateLimit> {
        if count == 0 {
            return Err(InvalidRateLimit::ZeroCount);
        }
        if period.as_micros() == 0 {
            return Err(InvalidRateLimit::ZeroPeriod);
        }
        Ok(Self {
            count,
            period,
            burst: count,
        })
    }

    /// `count` messages per second.
    ///
    /// # Errors
    ///
    /// [`InvalidRateLimit::ZeroCount`] when `count` is zero.
    pub const fn per_second(count: u32) -> Result<Self, InvalidRateLimit> {
        Self::new(count, Duration::from_secs(1))
    }

    /// `count` messages per minute.
    ///
    /// # Errors
    ///
    /// [`InvalidRateLimit::ZeroCount`] when `count` is zero.
    pub const fn per_minute(count: u32) -> Result<Self, InvalidRateLimit> {
        Self::new(count, Duration::from_secs(60))
    }

    /// How many messages may arrive back to back before the sustained rate applies.
    ///
    /// Defaults to `count`. Set it to at least what a station sends unprompted right after
    /// booting — one `StatusNotification` per connector, and whatever else its firmware
    /// flushes — or a conforming station is refused at every boot.
    ///
    /// # Errors
    ///
    /// [`InvalidRateLimit::ZeroBurst`] when `burst` is zero.
    pub const fn with_burst(mut self, burst: u32) -> Result<Self, InvalidRateLimit> {
        if burst == 0 {
            return Err(InvalidRateLimit::ZeroBurst);
        }
        self.burst = burst;
        Ok(self)
    }

    /// The sustained count per [`period`](Self::period).
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// The period [`count`](Self::count) is measured over.
    #[must_use]
    pub const fn period(&self) -> Duration {
        self.period
    }

    /// The burst.
    #[must_use]
    pub const fn burst(&self) -> u32 {
        self.burst
    }

    /// The spacing between messages at the sustained rate, in microseconds — never zero, so
    /// a rate above a million a second is a million a second.
    fn interval_micros(&self) -> u64 {
        let period = u64::try_from(self.period.as_micros()).unwrap_or(u64::MAX);
        (period / u64::from(self.count)).max(1)
    }
}

impl fmt::Display for RateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let period = self.period;
        if period == Duration::from_secs(60) {
            write!(f, "{}/min", self.count)?;
        } else if period == Duration::from_secs(1) {
            write!(f, "{}/s", self.count)?;
        } else {
            write!(f, "{} per {}ms", self.count, period.as_millis())?;
        }
        write!(f, " (burst {})", self.burst)
    }
}

/// Why a [`RateLimit`] could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidRateLimit {
    /// A count of zero would refuse everything.
    ZeroCount,
    /// A period shorter than a microsecond.
    ZeroPeriod,
    /// A burst of zero would refuse everything.
    ZeroBurst,
}

impl fmt::Display for InvalidRateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroCount => "a message rate needs a count of at least 1",
            Self::ZeroPeriod => "a message rate needs a period of at least 1µs",
            Self::ZeroBurst => "a message rate needs a burst of at least 1",
        })
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InvalidRateLimit {}

/// The limiter's state: the *theoretical arrival time* of the next message, in microseconds
/// on the driver's clock.
#[derive(Clone, Copy, Debug)]
pub(super) struct Gcra {
    interval: u64,
    tolerance: u64,
    tat: u64,
    /// Whether the last counted message was refused — an *episode* is reported once.
    limited: bool,
}

/// What the limiter decided about one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Admission {
    /// Within the rate.
    Admitted,
    /// Over the rate, and the first message of a new episode.
    Refused { new_episode: bool },
}

impl Gcra {
    pub(super) fn new(limit: RateLimit) -> Self {
        let interval = limit.interval_micros();
        Self {
            interval,
            tolerance: interval.saturating_mul(u64::from(limit.burst)),
            tat: 0,
            limited: false,
        }
    }

    /// Counts one message arriving at `now`.
    ///
    /// A refused message does not move the arrival time, so a flood leaves the peer no
    /// deeper in debt than its burst: it is admitted again one interval after it stops.
    pub(super) fn admit(&mut self, now: Instant) -> Admission {
        let now = now.as_millis().saturating_mul(1000);
        let tat = self.tat.max(now);
        if tat.saturating_add(self.interval) - now > self.tolerance {
            let new_episode = !self.limited;
            self.limited = true;
            return Admission::Refused { new_episode };
        }
        self.tat = tat.saturating_add(self.interval);
        self.limited = false;
        Admission::Admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Instant {
        Instant::from_millis(millis)
    }

    #[test]
    fn a_burst_is_admitted_and_the_next_one_is_not() {
        let mut gcra = Gcra::new(RateLimit::per_minute(60).unwrap().with_burst(5).unwrap());
        for _ in 0..5 {
            assert_eq!(gcra.admit(at(0)), Admission::Admitted);
        }
        assert_eq!(gcra.admit(at(0)), Admission::Refused { new_episode: true });
        assert_eq!(gcra.admit(at(0)), Admission::Refused { new_episode: false });
    }

    #[test]
    fn one_interval_later_one_more_is_admitted() {
        let mut gcra = Gcra::new(RateLimit::per_minute(60).unwrap().with_burst(1).unwrap());
        assert_eq!(gcra.admit(at(0)), Admission::Admitted);
        assert!(matches!(gcra.admit(at(999)), Admission::Refused { .. }));
        assert_eq!(gcra.admit(at(1000)), Admission::Admitted);
        // And the next refusal is a new episode.
        assert_eq!(
            gcra.admit(at(1000)),
            Admission::Refused { new_episode: true }
        );
    }

    #[test]
    fn a_flood_does_not_push_the_peer_into_debt() {
        let mut gcra = Gcra::new(RateLimit::per_second(1).unwrap());
        assert_eq!(gcra.admit(at(0)), Admission::Admitted);
        for _ in 0..10_000 {
            let _ = gcra.admit(at(10));
        }
        assert_eq!(gcra.admit(at(1000)), Admission::Admitted);
    }

    #[test]
    fn the_sustained_rate_holds_over_time() {
        let mut gcra = Gcra::new(RateLimit::per_second(10).unwrap().with_burst(1).unwrap());
        let admitted = (0..10_000u64)
            .filter(|ms| gcra.admit(at(*ms)) == Admission::Admitted)
            .count();
        // Ten seconds at ten a second.
        assert_eq!(admitted, 100);
    }

    #[test]
    fn zeros_are_refused_and_extremes_do_not_overflow() {
        assert_eq!(RateLimit::per_second(0), Err(InvalidRateLimit::ZeroCount));
        assert_eq!(
            RateLimit::new(1, Duration::from_nanos(999)),
            Err(InvalidRateLimit::ZeroPeriod)
        );
        assert_eq!(
            RateLimit::per_second(1).unwrap().with_burst(0),
            Err(InvalidRateLimit::ZeroBurst)
        );
        let mut huge = Gcra::new(
            RateLimit::new(u32::MAX, Duration::MAX)
                .unwrap()
                .with_burst(u32::MAX)
                .unwrap(),
        );
        assert_eq!(huge.admit(at(u64::MAX)), Admission::Admitted);
        let mut fast = Gcra::new(RateLimit::new(u32::MAX, Duration::from_micros(1)).unwrap());
        assert_eq!(fast.admit(at(0)), Admission::Admitted);
    }

    #[test]
    fn it_renders_for_a_log() {
        let rate = RateLimit::per_minute(120).unwrap().with_burst(20).unwrap();
        assert_eq!(rate.to_string(), "120/min (burst 20)");
    }
}
