//! Plans of per-slot ceilings, and their conversion to a profile. See [`ChargingPlan`].

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use super::{
    CONVERSION_SCALE, DEFAULT_PHASES, Period, Profile, ProfileKind, Purpose, RateUnit, Schedule,
    Supply,
};
use crate::types::{DateTime, Decimal};

/// A cost larger than any real one, for a merge whose arithmetic overflowed.
const UNBOUNDED: Decimal = Decimal::new(i64::MAX, 0);

/// One slot's ceiling: the most the EV may draw during it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ceiling {
    /// The limit: in W the sum of all phases, in A per phase (2.1 §2.18).
    pub limit: Decimal,
    /// The unit of [`limit`](Self::limit).
    pub unit: RateUnit,
    /// How many phases may be used, 1–3; `None` lets the station assume 3 (K01.FR.49).
    pub phases: Option<u8>,
}

impl Ceiling {
    /// A ceiling in watts, summed over all phases.
    #[must_use]
    pub const fn watts(limit: Decimal) -> Self {
        Self {
            limit,
            unit: RateUnit::W,
            phases: None,
        }
    }

    /// A ceiling in amperes per phase.
    #[must_use]
    pub const fn amperes(limit: Decimal) -> Self {
        Self {
            limit,
            unit: RateUnit::A,
            phases: None,
        }
    }

    /// Limits the number of phases (1–3).
    #[must_use]
    pub const fn phases(mut self, phases: u8) -> Self {
        self.phases = Some(phases);
        self
    }
}

/// Which `chargingRateUnit`s a station accepts (K01.FR.26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateUnits {
    /// `A` is allowed.
    pub amperes: bool,
    /// `W` is allowed.
    pub watts: bool,
}

impl RateUnits {
    /// Both units.
    pub const BOTH: Self = Self {
        amperes: true,
        watts: true,
    };

    /// Reads 1.6 `ChargingScheduleAllowedChargingRateUnit` — `Current`, `Power`, or both,
    /// comma-separated.
    #[must_use]
    pub fn parse_v16(value: &str) -> Self {
        Self::parse(value, "Current", "Power")
    }

    /// Reads 2.x `SmartChargingCtrlr.RateUnit` — `A`, `W`, or both, comma-separated.
    #[must_use]
    pub fn parse_v2(value: &str) -> Self {
        Self::parse(value, "A", "W")
    }

    fn parse(value: &str, amperes: &str, watts: &str) -> Self {
        let mut units = Self {
            amperes: false,
            watts: false,
        };
        for item in value.split(',').map(str::trim) {
            units.amperes |= item.eq_ignore_ascii_case(amperes);
            units.watts |= item.eq_ignore_ascii_case(watts);
        }
        units
    }

    const fn allows(self, unit: RateUnit) -> bool {
        match unit {
            RateUnit::A => self.amperes,
            RateUnit::W => self.watts,
        }
    }
}

/// What the station allows in a profile, from its own configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StationLimits {
    /// `SmartChargingCtrlr.PeriodsPerSchedule` (1.6 `ChargingScheduleMaxPeriods`).
    pub max_periods: usize,
    /// `SmartChargingCtrlr.ProfileStackLevel` (1.6 `ChargeProfileMaxStackLevel`).
    pub max_stack_level: i32,
    /// `SmartChargingCtrlr.RateUnit` (1.6 `ChargingScheduleAllowedChargingRateUnit`).
    pub units: RateUnits,
}

impl StationLimits {
    /// The limits a station reported.
    #[must_use]
    pub const fn new(max_periods: usize, max_stack_level: i32, units: RateUnits) -> Self {
        Self {
            max_periods,
            max_stack_level,
            units,
        }
    }
}

/// What the profile is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A `TxProfile` for one running transaction (K01.FR.03).
    Transaction {
        /// The EVSE; must be > 0 (K01.FR.16).
        evse_id: u32,
        /// The transaction id.
        transaction_id: String,
    },
    /// A `TxDefaultProfile` for every transaction on one EVSE (0: every EVSE).
    EvseDefault {
        /// The EVSE.
        evse_id: u32,
    },
}

impl Target {
    /// A `TxProfile` for `transaction_id` on `evse_id`.
    #[must_use]
    pub fn transaction(evse_id: u32, transaction_id: impl Into<String>) -> Self {
        Self::Transaction {
            evse_id,
            transaction_id: transaction_id.into(),
        }
    }
}

/// A converted plan.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct Planned {
    /// The profile, ready for [`Profile::set_request_v2_1`] and its siblings.
    pub profile: Profile,
    /// Whether the plan had more distinct slots than the station allows, so neighbours were
    /// merged into their lower limit. The profile is then below the plan in places, never above.
    pub coarsened: bool,
}

/// Why a plan could not become a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanError {
    /// A plan needs at least one slot.
    Empty,
    /// More than [`ChargingPlan::MAX_SLOTS`] slots.
    TooManySlots(usize),
    /// A slot must last at least a second, and the plan at most `i32::MAX` seconds — what a
    /// schedule's `duration` can say.
    BadDuration,
    /// A ceiling below zero.
    NegativeLimit {
        /// The slot.
        slot: usize,
    },
    /// A phase count outside 1–3.
    BadPhases {
        /// The slot.
        slot: usize,
    },
    /// The stack level is above what the station allows.
    StackLevel {
        /// What was asked for.
        requested: i32,
        /// The station's `ProfileStackLevel`.
        max: i32,
    },
    /// A `TxProfile` on EVSE 0 (K01.FR.16).
    TxProfileOnEvseZero,
    /// The station allows no periods, or neither unit.
    StationAllowsNothing,
    /// A unit conversion has no representable answer (a zero voltage, or digits to spare).
    Unconvertible {
        /// The slot.
        slot: usize,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a charging plan needs at least one slot"),
            Self::TooManySlots(n) => write!(
                f,
                "{n} slots is more than a plan may hold ({})",
                ChargingPlan::MAX_SLOTS
            ),
            Self::BadDuration => {
                f.write_str("slots must last at least 1 s and the plan at most i32::MAX s")
            }
            Self::NegativeLimit { slot } => write!(f, "slot {slot} has a negative ceiling"),
            Self::BadPhases { slot } => write!(f, "slot {slot} has a phase count outside 1–3"),
            Self::StackLevel { requested, max } => write!(
                f,
                "stack level {requested} is above the station's ProfileStackLevel {max}"
            ),
            Self::TxProfileOnEvseZero => {
                f.write_str("a TxProfile needs an EVSE above 0 (K01.FR.16)")
            }
            Self::StationAllowsNothing => {
                f.write_str("the station allows no periods or no rate unit")
            }
            Self::Unconvertible { slot } => {
                write!(f, "slot {slot} cannot be converted to the station's unit")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PlanError {}

/// Ceilings for consecutive, equally long slots from a start instant.
///
/// From a plan of per-slot ceilings to a `TxProfile` the station will accept.
///
/// Every consumer that drives a charging station from a plan — a home energy manager, a site
/// load manager, a CSMS doing smart charging — writes this function, and writes it differently:
/// a quarter-hour planner emits ceilings, and somewhere they have to become one valid
/// `ChargingProfile` within the station's own limits. The rules that make it valid are spread
/// over block K, and two of them are easy to get backwards:
///
/// * **A ceiling must stay a ceiling.** Converting watts to amperes divides by `voltage ×
///   phases`, which has no exact decimal answer; rounding half-to-even rounds *up* half the time,
///   and a profile 1 mA above the plan is a profile above the plan. Here W → A rounds down.
/// * **Too many slots for the station** (`SmartChargingCtrlr.PeriodsPerSchedule`, 1.6
///   `ChargingScheduleMaxPeriods`) are merged, never truncated: after a profile's last period
///   the station falls back to whatever applied before, which may be higher than the plan. Two
///   neighbours merge into the *lower* limit and the *fewer* phases, choosing each time the pair
///   whose merge gives up the least energy — so the result is below the plan everywhere, and as
///   close to it as the station allows.
///
/// The result is one profile for the whole plan. 2.1 K01.FR.56 lets a station refuse frequent
/// `SetChargingProfile` with `RateLimitExceeded`, because profiles wear out its persistent
/// memory; send a profile when the plan changes, not once per slot.
///
/// A plan holds ceilings only. OCPP 1.6 and 2.0.1 cannot command a *floor*: `minChargingRate`
/// tells the station something about the EV (K01.FR.21), and the setpoints that can require a
/// minimum are 2.1 V2X operation modes (block Q). A plan that needs one cannot be expressed here,
/// and pretending otherwise would be a silent loss.
///
/// ```
/// use core::time::Duration;
/// use ocpp_kit::smart_charging::{
///     Ceiling, ChargingPlan, RateUnits, StationLimits, Supply, Target,
/// };
/// use ocpp_kit::types::{DateTime, Decimal};
///
/// let start: DateTime = "2026-11-20T00:00:00Z".parse()?;
/// // Four quarter hours: 11 kW, 11 kW, then 7.4 kW twice, on three phases.
/// let plan = ChargingPlan::new(
///     start,
///     Duration::from_secs(900),
///     [11_000, 11_000, 7_400, 7_400].map(|w| Ceiling::watts(Decimal::from(w)).phases(3)),
/// )?;
/// // What the station said in its configuration (1.6 keys here; `RateUnits::parse_v2` reads
/// // `SmartChargingCtrlr.RateUnit`).
/// let limits = StationLimits::new(24, 3, RateUnits::parse_v16("Current"));
/// let planned = plan.to_profile(
///     1,
///     1,
///     Target::transaction(1, "4711"),
///     &limits,
///     Supply::default(),
/// )?;
/// let schedule = &planned.profile.schedules[0];
/// assert_eq!(schedule.periods.len(), 2);
/// // 7400 W / (230 V × 3) = 10.72463… A: rounded *down*, never to 10.725.
/// assert_eq!(schedule.periods[1].limit, Decimal::from_literal("10.724"));
/// assert!(!planned.coarsened);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```

#[derive(Clone, Debug, PartialEq)]
pub struct ChargingPlan {
    start: DateTime,
    slot: i64,
    ceilings: Vec<Ceiling>,
}

impl ChargingPlan {
    /// The most slots a plan may hold: 31 days of quarter hours.
    ///
    /// Merging slots for a station with few periods is quadratic in the number of slots, and a
    /// controller's one-second loop should not find that out from a planner bug.
    pub const MAX_SLOTS: usize = 2976;

    /// A plan of `ceilings`, one per `slot`, from `start`.
    ///
    /// # Errors
    ///
    /// [`PlanError`] for an empty or oversized plan, a slot shorter than a second, a plan
    /// longer than `i32::MAX` seconds, a negative ceiling or a phase count outside 1–3.
    pub fn new(
        start: DateTime,
        slot: Duration,
        ceilings: impl IntoIterator<Item = Ceiling>,
    ) -> Result<Self, PlanError> {
        let ceilings: Vec<Ceiling> = ceilings.into_iter().collect();
        if ceilings.is_empty() {
            return Err(PlanError::Empty);
        }
        if ceilings.len() > Self::MAX_SLOTS {
            return Err(PlanError::TooManySlots(ceilings.len()));
        }
        let slot = i64::try_from(slot.as_secs()).map_err(|_| PlanError::BadDuration)?;
        let total = slot
            .checked_mul(i64::try_from(ceilings.len()).map_err(|_| PlanError::BadDuration)?)
            .ok_or(PlanError::BadDuration)?;
        if slot < 1 || total > i64::from(i32::MAX) {
            return Err(PlanError::BadDuration);
        }
        for (index, ceiling) in ceilings.iter().enumerate() {
            if ceiling.limit.signum() < 0 {
                return Err(PlanError::NegativeLimit { slot: index });
            }
            if ceiling
                .phases
                .is_some_and(|phases| !(1..=3).contains(&phases))
            {
                return Err(PlanError::BadPhases { slot: index });
            }
        }
        Ok(Self {
            start,
            slot,
            ceilings,
        })
    }

    /// When the plan starts.
    #[must_use]
    pub const fn start(&self) -> DateTime {
        self.start
    }

    /// The plan's ceilings, in order.
    #[must_use]
    pub fn ceilings(&self) -> &[Ceiling] {
        &self.ceilings
    }

    /// The ceiling `offset` seconds after the start, if the plan covers it.
    #[must_use]
    pub fn ceiling_at(&self, offset: i64) -> Option<&Ceiling> {
        if offset < 0 {
            return None;
        }
        self.ceilings.get(usize::try_from(offset / self.slot).ok()?)
    }

    /// How long the plan lasts, in seconds.
    #[must_use]
    pub fn duration(&self) -> i64 {
        // `new` checked that this fits an `i32`.
        self.slot_start(self.ceilings.len())
    }

    /// Seconds from the plan's start to the start of slot `index`.
    fn slot_start(&self, index: usize) -> i64 {
        self.slot
            .saturating_mul(i64::try_from(index).unwrap_or(i64::MAX))
    }

    /// Turns the plan into one `Absolute` profile the station will accept.
    ///
    /// The profile is a `TxProfile` for [`Target::Transaction`] and a `TxDefaultProfile` for
    /// [`Target::EvseDefault`]; it starts at the plan's start, lasts as long as the plan, has
    /// its first period at 0 (K01.FR.31) and the rest ascending (K01.FR.35), never sets
    /// `phaseToUse` (K01.FR.19/20), uses a unit the station allows (K01.FR.26) and has at most
    /// the station's number of periods — merged conservatively when the plan has more.
    ///
    /// # Errors
    ///
    /// [`PlanError`] for a stack level above the station's, a `TxProfile` on EVSE 0, a station
    /// that allows no periods or no unit, or a conversion with no answer.
    pub fn to_profile(
        &self,
        id: i32,
        stack_level: i32,
        target: Target,
        limits: &StationLimits,
        supply: Supply,
    ) -> Result<Planned, PlanError> {
        if stack_level > limits.max_stack_level {
            return Err(PlanError::StackLevel {
                requested: stack_level,
                max: limits.max_stack_level,
            });
        }
        if limits.max_periods == 0 || !(limits.units.amperes || limits.units.watts) {
            return Err(PlanError::StationAllowsNothing);
        }

        let mut steps = Vec::with_capacity(self.ceilings.len());
        for (index, ceiling) in self.ceilings.iter().enumerate() {
            let unit = if limits.units.allows(ceiling.unit) {
                ceiling.unit
            } else if ceiling.unit == RateUnit::W {
                RateUnit::A
            } else {
                RateUnit::W
            };
            let phases = ceiling.phases.map(i32::from);
            let limit = convert_down(supply, ceiling.limit, ceiling.unit, unit, phases)
                .ok_or(PlanError::Unconvertible { slot: index })?;
            steps.push(Step {
                start: self.slot_start(index),
                limit,
                unit,
                phases,
            });
        }

        // One unit for the whole schedule: if slots came in both, settle on the station's
        // preference order — the plan's first unit when allowed, else the other.
        let unit = steps[0].unit;
        for (index, step) in steps.iter_mut().enumerate() {
            if step.unit != unit {
                step.limit = convert_down(supply, step.limit, step.unit, unit, step.phases)
                    .ok_or(PlanError::Unconvertible { slot: index })?;
                step.unit = unit;
            }
        }

        let mut periods = merge_equal(&steps);
        let mut coarsened = false;
        while periods.len() > limits.max_periods {
            coarsen_once(&mut periods, self.duration());
            coarsened = true;
        }

        let schedule = Schedule::new(id, unit, periods)
            .starting(self.start)
            .lasting(self.duration());
        let (purpose, evse_id, transaction) = match target {
            Target::Transaction {
                evse_id,
                transaction_id,
            } => {
                if evse_id == 0 {
                    return Err(PlanError::TxProfileOnEvseZero);
                }
                (Purpose::TxProfile, evse_id, Some(transaction_id))
            }
            Target::EvseDefault { evse_id } => (Purpose::TxDefaultProfile, evse_id, None),
        };
        let mut profile = Profile::new(id, stack_level, purpose, ProfileKind::Absolute, schedule)
            .on_evse(evse_id);
        profile.transaction_id = transaction;
        Ok(Planned { profile, coarsened })
    }
}

/// One slot on its way to becoming a period.
#[derive(Clone, Copy)]
struct Step {
    start: i64,
    limit: Decimal,
    unit: RateUnit,
    phases: Option<i32>,
}

/// Converts a ceiling so that the result is never above it: W → A rounds down to a milliampere.
fn convert_down(
    supply: Supply,
    limit: Decimal,
    from: RateUnit,
    to: RateUnit,
    phases: Option<i32>,
) -> Option<Decimal> {
    let converted = supply.convert(limit, from, to, phases)?;
    if from != RateUnit::W || to != RateUnit::A {
        return Some(converted);
    }
    // `convert` rounds half to even; step down one milliampere if that rounded up.
    let back = supply.convert(converted, RateUnit::A, RateUnit::W, phases)?;
    if back > limit {
        converted.checked_sub(Decimal::new(1, CONVERSION_SCALE))
    } else {
        Some(converted)
    }
}

/// One period per run of equal (limit, phases) slots.
fn merge_equal(steps: &[Step]) -> Vec<Period> {
    let mut periods: Vec<Period> = Vec::new();
    for step in steps {
        if let Some(last) = periods.last() {
            if last.limit == step.limit && last.number_phases == step.phases {
                continue;
            }
        }
        periods.push(Period {
            start_period: step.start,
            limit: step.limit,
            number_phases: step.phases,
            phase_to_use: None,
        });
    }
    periods
}

/// Merges the adjacent pair whose merge gives up the least energy, into the lower limit and the
/// fewer phases.
fn coarsen_once(periods: &mut Vec<Period>, total: i64) {
    let end = |index: usize, periods: &[Period]| {
        periods
            .get(index + 1)
            .map_or(total, |next| next.start_period)
    };
    let mut best: Option<(usize, Decimal)> = None;
    for index in 0..periods.len() - 1 {
        let (a, b) = (&periods[index], &periods[index + 1]);
        let low = a.limit.min(b.limit);
        let lost = |period: &Period, from: i64, to: i64| {
            period
                .limit
                .checked_sub(low)
                .and_then(|excess| excess.checked_mul(Decimal::from(to - from)))
                .unwrap_or(UNBOUNDED)
        };
        let cost = lost(a, a.start_period, b.start_period)
            .checked_add(lost(b, b.start_period, end(index + 1, periods)))
            .unwrap_or(UNBOUNDED);
        if best.is_none_or(|(_, seen)| cost < seen) {
            best = Some((index, cost));
        }
    }
    let Some((index, _)) = best else { return };
    let next = periods.remove(index + 1);
    let merged = &mut periods[index];
    merged.limit = merged.limit.min(next.limit);
    merged.number_phases = match (merged.number_phases, next.number_phases) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(DEFAULT_PHASES).min(b.unwrap_or(DEFAULT_PHASES))),
    };
    // Merging may have made it equal to a neighbour; fold those too.
    let folded = periods.windows(2).position(|pair| {
        pair[0].limit == pair[1].limit && pair[0].number_phases == pair[1].number_phases
    });
    if let Some(at) = folded {
        periods.remove(at + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> DateTime {
        "2026-11-20T00:00:00Z".parse().unwrap()
    }

    fn w(value: i64) -> Ceiling {
        Ceiling::watts(Decimal::from(value))
    }

    fn quarter_hours(ceilings: impl IntoIterator<Item = Ceiling>) -> ChargingPlan {
        ChargingPlan::new(start(), Duration::from_secs(900), ceilings).unwrap()
    }

    const BOTH: StationLimits = StationLimits::new(48, 3, RateUnits::BOTH);

    fn target() -> Target {
        Target::transaction(1, "tx")
    }

    #[test]
    fn equal_neighbours_become_one_period() {
        let planned = quarter_hours([w(11_000), w(11_000), w(7_000), w(7_000)])
            .to_profile(1, 1, target(), &BOTH, Supply::default())
            .unwrap();
        let schedule = &planned.profile.schedules[0];
        let periods: Vec<(i64, Decimal)> = schedule
            .periods
            .iter()
            .map(|p| (p.start_period, p.limit))
            .collect();
        assert_eq!(
            periods,
            [(0, Decimal::from(11_000)), (1800, Decimal::from(7_000))]
        );
        assert_eq!(schedule.rate_unit, RateUnit::W);
        assert_eq!(schedule.start, Some(start()));
        assert_eq!(schedule.duration, Some(3600));
        assert_eq!(planned.profile.kind, ProfileKind::Absolute);
        assert!(!planned.coarsened);
    }

    #[test]
    fn watts_to_amperes_rounds_down_never_half_to_even() {
        let amperes_only = StationLimits::new(48, 3, RateUnits::parse_v16("Current"));
        let planned = quarter_hours([w(7_400).phases(3)])
            .to_profile(1, 1, target(), &amperes_only, Supply::default())
            .unwrap();
        let period = planned.profile.schedules[0].periods[0];
        // 7400 / 690 = 10.724637…: half to even gives 10.725, which is above the plan.
        assert_eq!(period.limit, Decimal::from_literal("10.724"));
        assert_eq!(planned.profile.schedules[0].rate_unit, RateUnit::A);
        assert_eq!(period.number_phases, Some(3));
    }

    #[test]
    fn too_many_periods_are_merged_below_the_plan() {
        let plan = quarter_hours([w(5_000), w(11_000), w(4_000), w(9_000), w(3_000)]);
        let limits = StationLimits::new(2, 3, RateUnits::BOTH);
        let planned = plan
            .to_profile(1, 1, target(), &limits, Supply::default())
            .unwrap();
        assert!(planned.coarsened);
        let schedule = &planned.profile.schedules[0];
        assert!(schedule.periods.len() <= 2);
        for second in 0..plan.duration() {
            let planned_limit = plan.ceiling_at(second).unwrap().limit;
            let profile_limit = schedule.period_at(second).unwrap().limit;
            assert!(profile_limit <= planned_limit, "at {second}s");
        }
    }

    #[test]
    fn one_period_allowed_means_the_plan_minimum() {
        let limits = StationLimits::new(1, 3, RateUnits::BOTH);
        let planned = quarter_hours([w(5_000), w(11_000), w(4_000)])
            .to_profile(1, 1, target(), &limits, Supply::default())
            .unwrap();
        let periods = &planned.profile.schedules[0].periods;
        assert_eq!(periods.len(), 1);
        assert_eq!(periods[0].limit, Decimal::from(4_000));
    }

    #[test]
    fn a_tx_profile_is_shaped_by_block_k() {
        let planned = quarter_hours([w(5_000).phases(1), w(11_000)])
            .to_profile(
                7,
                2,
                Target::transaction(2, "tx-9"),
                &BOTH,
                Supply::default(),
            )
            .unwrap();
        let profile = planned.profile;
        assert_eq!(profile.purpose, Purpose::TxProfile);
        assert_eq!(profile.evse_id, 2);
        assert_eq!(profile.transaction_id.as_deref(), Some("tx-9"));
        assert_eq!(profile.stack_level, 2);
        assert_eq!(profile.schedules[0].periods[0].start_period, 0);
        assert!(
            profile.schedules[0]
                .periods
                .iter()
                .all(|p| p.phase_to_use.is_none())
        );

        let default = quarter_hours([w(5_000)])
            .to_profile(
                8,
                0,
                Target::EvseDefault { evse_id: 0 },
                &BOTH,
                Supply::default(),
            )
            .unwrap();
        assert_eq!(default.profile.purpose, Purpose::TxDefaultProfile);
        assert_eq!(default.profile.transaction_id, None);
    }

    #[test]
    fn the_station_limits_are_refused_not_ignored() {
        let plan = quarter_hours([w(5_000)]);
        assert_eq!(
            plan.to_profile(1, 4, target(), &BOTH, Supply::default()),
            Err(PlanError::StackLevel {
                requested: 4,
                max: 3
            })
        );
        assert_eq!(
            plan.to_profile(1, 1, Target::transaction(0, "tx"), &BOTH, Supply::default()),
            Err(PlanError::TxProfileOnEvseZero)
        );
        let nothing = StationLimits::new(10, 3, RateUnits::parse_v2(""));
        assert_eq!(
            plan.to_profile(1, 1, target(), &nothing, Supply::default()),
            Err(PlanError::StationAllowsNothing)
        );
    }

    #[test]
    fn bad_plans_are_refused() {
        let empty: [Ceiling; 0] = [];
        assert_eq!(
            ChargingPlan::new(start(), Duration::from_secs(900), empty),
            Err(PlanError::Empty)
        );
        assert_eq!(
            ChargingPlan::new(
                start(),
                Duration::from_secs(900),
                core::iter::repeat_n(w(1), ChargingPlan::MAX_SLOTS + 1)
            ),
            Err(PlanError::TooManySlots(ChargingPlan::MAX_SLOTS + 1))
        );
        assert_eq!(
            ChargingPlan::new(start(), Duration::from_millis(500), [w(1)]),
            Err(PlanError::BadDuration)
        );
        assert_eq!(
            ChargingPlan::new(start(), Duration::from_secs(u64::from(u32::MAX)), [w(1)]),
            Err(PlanError::BadDuration)
        );
        assert_eq!(
            ChargingPlan::new(start(), Duration::from_secs(900), [w(-1)]),
            Err(PlanError::NegativeLimit { slot: 0 })
        );
        assert_eq!(
            ChargingPlan::new(start(), Duration::from_secs(900), [w(1).phases(4)]),
            Err(PlanError::BadPhases { slot: 0 })
        );
    }

    #[test]
    fn rate_units_read_both_spellings() {
        assert_eq!(RateUnits::parse_v16("Current,Power"), RateUnits::BOTH);
        assert_eq!(
            RateUnits::parse_v16("Power"),
            RateUnits {
                amperes: false,
                watts: true
            }
        );
        assert_eq!(RateUnits::parse_v2("A, W"), RateUnits::BOTH);
        assert_eq!(
            RateUnits::parse_v2("A"),
            RateUnits {
                amperes: true,
                watts: false
            }
        );
    }
}
