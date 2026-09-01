//! Charging profiles and the composite schedule (functional block K).
//!
//! Smart charging is the one part of OCPP where the protocol hands you a *calculation*
//! rather than a state machine: several profiles apply at once, they stack by purpose and by
//! stack level, they can recur daily or weekly, and `GetCompositeSchedule` asks for the
//! single schedule that results. That calculation is what this module does.
//!
//! The rules it implements are Part 2 §3.5 "Stacking charging profiles" and §3.6 "Combining
//! Charging Profile Purposes", which say different things and are easy to conflate:
//!
//! * **Within one purpose**, the *leading* schedule is the one that "has a schedule period
//!   defined for that time and … belongs to a charging profile with the highest stack level
//!   that is valid at that time". Both qualifications matter: a stack level 3 profile that
//!   is outside its validity window, or whose schedule has nothing to say about this
//!   instant, does not shadow stack level 2 — it is simply not leading here.
//! * **Across purposes**, the composite is "the lowest charging limit … among the leading
//!   profiles of the different purposes". `ChargingStationMaxProfile` and
//!   `ChargingStationExternalConstraints` are two of those purposes, not post-hoc ceilings.
//! * `TxProfile` **replaces** `TxDefaultProfile` for the transaction it names, and
//!   `PriorityCharging` overrules both — the three occupy one purpose slot between them.
//! * `LocalGeneration` is **added on top** of the result, not minimised with it. It is
//!   capacity the site produces; treating it as a limit would turn a solar array into a cap.
//! * A profile outside its `validFrom` / `validTo`, or past its schedule's `duration`,
//!   contributes nothing.
//!
//! One thing it deliberately does not do: §3.6's note that "the limit value of
//! `ChargingStationMaxProfile` for EVSE 0 is the limit for all EVSEs **combined**". That is an
//! allocation problem across simultaneously charging EVSEs, not a property of one EVSE's
//! schedule, and it belongs to whatever in the station arbitrates between them.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::types::{DateTime, Decimal};

/// The scale a limit converted between amperes and watts is rounded to.
///
/// Amperes and watts convert by multiplying or dividing by `voltage × phases`, and division
/// is the one operation with no exact answer in base ten — 1000 W over 230 V and three phases
/// is 1.449275… A. Three decimals is a milliampere, which is four orders of magnitude finer
/// than any charging station resolves, and rounding there is what keeps the calculation
/// exact everywhere else.
pub const CONVERSION_SCALE: u8 = 3;

/// The unit a limit is expressed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateUnit {
    /// Watts.
    W,
    /// Amperes, per phase.
    A,
}

impl fmt::Display for RateUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RateUnit::W => "W",
            RateUnit::A => "A",
        })
    }
}

/// What a profile is for, in the order that decides precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Purpose {
    /// The station's own ceiling, installed on EVSE 0. Never exceeded.
    ChargingStationMaxProfile,
    /// The default for any transaction at this EVSE.
    TxDefaultProfile,
    /// A profile for one specific transaction; replaces the default while it applies.
    TxProfile,
    /// A limit imposed from outside the CSMS — a DSO, or a local controller.
    ChargingStationExternalConstraints,
    /// Priority charging for one transaction (2.1).
    PriorityCharging,
    /// Local generation (2.1).
    LocalGeneration,
}

/// How a profile's schedule is anchored in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileKind {
    /// `startSchedule` is an absolute instant.
    Absolute,
    /// `startSchedule` is the first occurrence; it repeats.
    Recurring,
    /// The schedule starts when the transaction starts.
    Relative,
}

/// How often a recurring profile repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recurrency {
    /// Every 24 hours.
    Daily,
    /// Every 7 days.
    Weekly,
}

impl Recurrency {
    const fn seconds(self) -> i64 {
        match self {
            Recurrency::Daily => 86_400,
            Recurrency::Weekly => 604_800,
        }
    }
}

/// One step of a schedule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Period {
    /// Seconds after the schedule's start at which this step begins.
    pub start_period: i64,
    /// The limit, in the schedule's `RateUnit`.
    pub limit: Decimal,
    /// How many phases may be used.
    pub number_phases: Option<i32>,
    /// Which phase to use when only one is allowed.
    pub phase_to_use: Option<i32>,
}

impl Period {
    /// A step with no phase constraints.
    #[must_use]
    pub const fn new(start_period: i64, limit: Decimal) -> Self {
        Self {
            start_period,
            limit,
            number_phases: None,
            phase_to_use: None,
        }
    }

    /// Sets the phase count.
    #[must_use]
    pub const fn phases(mut self, phases: i32) -> Self {
        self.number_phases = Some(phases);
        self
    }
}

/// A profile's schedule.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    /// The schedule id.
    pub id: i32,
    /// When the schedule starts. Required for `Absolute` and `Recurring`.
    pub start: Option<DateTime>,
    /// How long the schedule lasts, in seconds. `None` means "until the profile stops being
    /// valid".
    pub duration: Option<i64>,
    /// The unit the limits are in.
    pub rate_unit: RateUnit,
    /// The steps, which must be ordered by `start_period`.
    pub periods: Vec<Period>,
    /// The lowest rate the EV can usefully take.
    pub min_charging_rate: Option<Decimal>,
}

impl Schedule {
    /// A schedule with the given steps.
    #[must_use]
    pub fn new(id: i32, rate_unit: RateUnit, mut periods: Vec<Period>) -> Self {
        periods.sort_by_key(|period| period.start_period);
        Self {
            id,
            start: None,
            duration: None,
            rate_unit,
            periods,
            min_charging_rate: None,
        }
    }

    /// Sets the absolute start.
    #[must_use]
    pub fn starting(mut self, start: DateTime) -> Self {
        self.start = Some(start);
        self
    }

    /// Sets the duration in seconds.
    #[must_use]
    pub fn lasting(mut self, seconds: i64) -> Self {
        self.duration = Some(seconds);
        self
    }

    /// The step in effect `offset` seconds after the schedule started.
    #[must_use]
    pub fn period_at(&self, offset: i64) -> Option<&Period> {
        if offset < 0 || self.duration.is_some_and(|duration| offset >= duration) {
            return None;
        }
        self.periods
            .iter()
            .rev()
            .find(|period| period.start_period <= offset)
    }
}

/// A charging profile as installed by `SetChargingProfile`.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// `chargingProfileId`.
    pub id: i32,
    /// Higher wins within a purpose.
    pub stack_level: i32,
    /// What the profile is for.
    pub purpose: Purpose,
    /// How its schedule is anchored.
    pub kind: ProfileKind,
    /// How often it repeats, for `Recurring`.
    pub recurrency: Option<Recurrency>,
    /// Not in effect before this instant.
    pub valid_from: Option<DateTime>,
    /// Not in effect after this instant.
    pub valid_to: Option<DateTime>,
    /// The transaction a `TxProfile` belongs to.
    pub transaction_id: Option<String>,
    /// The EVSE it was installed on; `0` means the whole Charging Station.
    pub evse_id: u32,
    /// Its schedules. Only the first is used by the composite calculation, as 1.6 and the
    /// common 2.x case have exactly one.
    pub schedules: Vec<Schedule>,
}

impl Profile {
    /// A profile with one schedule.
    #[must_use]
    pub fn new(
        id: i32,
        stack_level: i32,
        purpose: Purpose,
        kind: ProfileKind,
        schedule: Schedule,
    ) -> Self {
        Self {
            id,
            stack_level,
            purpose,
            kind,
            recurrency: None,
            valid_from: None,
            valid_to: None,
            transaction_id: None,
            evse_id: 0,
            schedules: alloc::vec![schedule],
        }
    }

    /// Installs the profile on one EVSE.
    #[must_use]
    pub fn on_evse(mut self, evse_id: u32) -> Self {
        self.evse_id = evse_id;
        self
    }

    /// Binds the profile to one transaction.
    #[must_use]
    pub fn for_transaction(mut self, transaction_id: impl Into<String>) -> Self {
        self.transaction_id = Some(transaction_id.into());
        self
    }

    /// Sets the validity window.
    #[must_use]
    pub fn valid(mut self, from: Option<DateTime>, to: Option<DateTime>) -> Self {
        self.valid_from = from;
        self.valid_to = to;
        self
    }

    /// Makes the profile repeat.
    #[must_use]
    pub fn recurring(mut self, recurrency: Recurrency) -> Self {
        self.kind = ProfileKind::Recurring;
        self.recurrency = Some(recurrency);
        self
    }

    fn valid_at(&self, at: i64) -> bool {
        self.valid_from.is_none_or(|from| epoch(from) <= at)
            && self.valid_to.is_none_or(|to| at < epoch(to))
    }

    /// The offset into the schedule at `at`, honouring recurrence — or `None` when the
    /// profile says nothing about that instant.
    fn offset_at(&self, at: i64, transaction_start: Option<i64>) -> Option<i64> {
        let schedule = self.schedules.first()?;
        match self.kind {
            ProfileKind::Absolute => {
                let start = schedule.start.map(epoch)?;
                (at >= start).then_some(at - start)
            }
            ProfileKind::Relative => {
                let start = transaction_start?;
                (at >= start).then_some(at - start)
            }
            ProfileKind::Recurring => {
                let start = schedule.start.map(epoch)?;
                let period = self.recurrency?.seconds();
                if at < start {
                    return None;
                }
                Some((at - start) % period)
            }
        }
    }

    /// The limit this profile imposes at `at`, in its own unit.
    fn limit_at(
        &self,
        at: i64,
        transaction_start: Option<i64>,
    ) -> Option<(Decimal, RateUnit, Option<i32>)> {
        if !self.valid_at(at) {
            return None;
        }
        let schedule = self.schedules.first()?;
        let offset = self.offset_at(at, transaction_start)?;
        let period = schedule.period_at(offset)?;
        Some((period.limit, schedule.rate_unit, period.number_phases))
    }
}

/// The supply parameters needed to convert between amperes and watts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supply {
    /// Nominal line voltage, in volts.
    pub voltage: Decimal,
    /// The number of phases available.
    pub phases: i32,
}

impl Default for Supply {
    /// 230 V, three phases — the European default.
    fn default() -> Self {
        Self {
            voltage: Decimal::from(230),
            phases: 3,
        }
    }
}

impl Supply {
    /// Converts a limit into `to`.
    ///
    /// Amperes to watts is exact. Watts to amperes is a division, so it is rounded to
    /// [`CONVERSION_SCALE`] — a milliampere — half to even.
    ///
    /// `None` means the conversion has no representable answer: a zero voltage or phase
    /// count, or a limit so large that `limit × voltage × phases` needs more than 19 digits.
    /// Neither happens for a real supply, and a caller that hits one is better off seeing it
    /// than being handed a number that is wrong.
    #[must_use]
    pub fn convert(
        self,
        limit: Decimal,
        from: RateUnit,
        to: RateUnit,
        phases: Option<i32>,
    ) -> Option<Decimal> {
        if from == to {
            return Some(limit);
        }
        let phases = Decimal::from(phases.unwrap_or(self.phases).max(1));
        let factor = self.voltage.checked_mul(phases)?;
        match (from, to) {
            (RateUnit::A, RateUnit::W) => limit.checked_mul(factor),
            (RateUnit::W, RateUnit::A) => limit.checked_div(factor, CONVERSION_SCALE),
            _ => Some(limit),
        }
    }
}

/// One step of a computed composite schedule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositePeriod {
    /// Seconds after the requested start.
    pub start_period: i64,
    /// The effective limit, or `None` when no profile constrains this stretch at all.
    ///
    /// The gap is a real state and has to be representable. Carrying the previous step's
    /// limit through it — the obvious shortcut — reports a constraint that no installed
    /// profile imposes, and it does so for exactly the stretch where the EVSE is free to
    /// draw its rated maximum. `ChargingSchedulePeriodType.limit` is mandatory in the
    /// schemas, so a caller answering `GetCompositeSchedule` substitutes the EVSE's rated
    /// maximum here; see [`CompositeSchedule::fill_gaps`].
    pub limit: Option<Decimal>,
    /// The phase count the winning profile asked for.
    pub number_phases: Option<i32>,
}

/// The answer to `GetCompositeSchedule`.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositeSchedule {
    /// The EVSE it was computed for.
    pub evse_id: u32,
    /// When the schedule starts.
    pub start: DateTime,
    /// How long it covers, in seconds.
    pub duration: i64,
    /// The unit the limits are in.
    pub rate_unit: RateUnit,
    /// The steps, merged so that no two consecutive steps carry the same limit.
    pub periods: Vec<CompositePeriod>,
}

impl CompositeSchedule {
    /// Replaces every unconstrained stretch with `rated_maximum`, in this schedule's
    /// [`rate_unit`](Self::rate_unit).
    ///
    /// `ChargingSchedulePeriodType.limit` is mandatory, so a `GetCompositeScheduleResponse`
    /// has to name a number for a stretch that no profile constrains. The only honest number
    /// is what the EVSE can actually deliver, which the protocol never tells the station —
    /// hence a parameter rather than a guess.
    #[must_use]
    pub fn fill_gaps(mut self, rated_maximum: impl Into<Decimal>) -> Self {
        let rated_maximum = rated_maximum.into();
        for period in &mut self.periods {
            if period.limit.is_none() {
                period.limit = Some(rated_maximum);
            }
        }
        self.periods
            .dedup_by(|next, previous| same_step(*previous, *next));
        self
    }

    /// Whether any stretch of this schedule is left unconstrained.
    #[must_use]
    pub fn has_gaps(&self) -> bool {
        self.periods.iter().any(|period| period.limit.is_none())
    }
}

/// The installed profiles, and the composite calculation over them.
#[derive(Clone, Debug, Default)]
pub struct ProfileStore {
    profiles: Vec<Profile>,
    /// The supply parameters used when a unit conversion is needed.
    pub supply: Supply,
}

impl ProfileStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a profile, replacing any profile with the same id.
    ///
    /// K01: a `SetChargingProfile` with an id that already exists replaces it.
    pub fn install(&mut self, profile: Profile) {
        self.profiles.retain(|existing| existing.id != profile.id);
        self.profiles.push(profile);
    }

    /// Every installed profile.
    #[must_use]
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    /// Removes profiles matching the `ClearChargingProfile` criteria. Returns how many went.
    ///
    /// Passing no criteria at all clears everything, as the message defines.
    pub fn clear(
        &mut self,
        id: Option<i32>,
        evse_id: Option<u32>,
        purpose: Option<Purpose>,
        stack_level: Option<i32>,
    ) -> usize {
        let before = self.profiles.len();
        self.profiles.retain(|profile| {
            let matches = id.is_none_or(|id| profile.id == id)
                && evse_id.is_none_or(|evse| profile.evse_id == evse)
                && purpose.is_none_or(|purpose| profile.purpose == purpose)
                && stack_level.is_none_or(|level| profile.stack_level == level);
            !matches
        });
        before - self.profiles.len()
    }

    /// Removes every profile bound to a transaction that has ended (K08).
    pub fn clear_transaction(&mut self, transaction_id: &str) -> usize {
        let before = self.profiles.len();
        self.profiles
            .retain(|profile| profile.transaction_id.as_deref() != Some(transaction_id));
        before - self.profiles.len()
    }

    /// Computes the composite schedule for `evse_id`.
    ///
    /// `transaction` names the transaction running at the EVSE, which decides which
    /// `TxProfile` applies and where a `Relative` schedule starts.
    #[must_use]
    pub fn composite(
        &self,
        evse_id: u32,
        from: DateTime,
        duration: i64,
        rate_unit: RateUnit,
        transaction: Option<(&str, DateTime)>,
    ) -> CompositeSchedule {
        let start = epoch(from);
        let end = start + duration.max(0);
        let transaction_start = transaction.map(|(_, at)| epoch(at));

        // Profiles on EVSE 0 apply to every EVSE; the ceiling always lives there.
        let applicable: Vec<&Profile> = self
            .profiles
            .iter()
            .filter(|profile| profile.evse_id == 0 || profile.evse_id == evse_id)
            .filter(|profile| match profile.purpose {
                // A TxProfile only applies to the transaction it names.
                Purpose::TxProfile | Purpose::PriorityCharging => {
                    match (&profile.transaction_id, transaction) {
                        (Some(id), Some((running, _))) => id == running,
                        (None, Some(_)) => true,
                        _ => false,
                    }
                }
                _ => true,
            })
            .collect();

        let boundaries = Self::boundaries(&applicable, start, end, transaction_start);

        let mut periods: Vec<CompositePeriod> = Vec::new();
        for at in boundaries {
            // `None` here means no installed profile says anything about this instant. It
            // opens a step of its own: skipping it would silently extend the previous
            // limit across a stretch nothing constrains.
            let effective = self.limit_at(&applicable, at, rate_unit, transaction_start);
            let step = CompositePeriod {
                start_period: at - start,
                limit: effective.map(|(limit, _)| limit),
                number_phases: effective.and_then(|(_, phases)| phases),
            };
            match periods.last() {
                Some(previous) if same_step(*previous, step) => {}
                // A leading gap is not a step: the schedule simply starts where the first
                // constraint does.
                None if step.limit.is_none() => {}
                _ => periods.push(step),
            }
        }

        CompositeSchedule {
            evse_id,
            start: from,
            duration,
            rate_unit,
            periods,
        }
    }

    /// Every instant in `[start, end)` at which the effective limit could change.
    fn boundaries(
        profiles: &[&Profile],
        start: i64,
        end: i64,
        transaction_start: Option<i64>,
    ) -> Vec<i64> {
        let mut points: BTreeSet<i64> = BTreeSet::new();
        points.insert(start);
        for profile in profiles {
            for at in [profile.valid_from.map(epoch), profile.valid_to.map(epoch)]
                .into_iter()
                .flatten()
            {
                if (start..end).contains(&at) {
                    points.insert(at);
                }
            }
            let Some(schedule) = profile.schedules.first() else {
                continue;
            };
            let anchors: Vec<i64> = match profile.kind {
                ProfileKind::Absolute => schedule.start.map(epoch).into_iter().collect(),
                ProfileKind::Relative => transaction_start.into_iter().collect(),
                ProfileKind::Recurring => {
                    let Some(first) = schedule.start.map(epoch) else {
                        continue;
                    };
                    let Some(period) = profile.recurrency.map(Recurrency::seconds) else {
                        continue;
                    };
                    // Walk the repetitions that overlap the window, bounded so a tiny period
                    // cannot make this loop forever.
                    let mut anchors = Vec::new();
                    let skipped = (start - first).div_euclid(period);
                    let mut at = first + skipped * period;
                    while at < end && anchors.len() < 4096 {
                        anchors.push(at);
                        at += period;
                    }
                    anchors
                }
            };
            for anchor in anchors {
                for step in &schedule.periods {
                    let at = anchor + step.start_period;
                    if (start..end).contains(&at) {
                        points.insert(at);
                    }
                }
                if let Some(duration) = schedule.duration {
                    let at = anchor + duration;
                    if (start..end).contains(&at) {
                        points.insert(at);
                    }
                }
            }
        }
        points.into_iter().collect()
    }

    /// The effective limit at one instant, applying the stacking rules.
    /// The effective limit at one instant, per Part 2 §3.6 "Combining Charging Profile
    /// Purposes".
    fn limit_at(
        &self,
        profiles: &[&Profile],
        at: i64,
        rate_unit: RateUnit,
        transaction_start: Option<i64>,
    ) -> Option<(Decimal, Option<i32>)> {
        // §3.6: "the leading charging schedule for that purpose is the charging schedule that
        // **has a schedule period defined for that time** and that belongs to a charging
        // profile with the highest stack level **that is valid at that time**". Both
        // qualifications are part of the selection, so a higher stack level that says nothing
        // about this instant does not shadow a lower one that does — it simply is not
        // leading here.
        let leading = |purpose: Purpose| -> Option<(Decimal, Option<i32>)> {
            profiles
                .iter()
                .filter(|profile| profile.purpose == purpose)
                .filter_map(|profile| {
                    profile
                        .limit_at(at, transaction_start)
                        .map(|limit| (profile.stack_level, limit))
                })
                .max_by_key(|(stack_level, _)| *stack_level)
                .and_then(|(_, (limit, unit, phases))| {
                    // A limit with no representable value in the requested unit constrains
                    // nothing it can be trusted to constrain; see `Supply::convert`.
                    Some((self.supply.convert(limit, unit, rate_unit, phases)?, phases))
                })
        };

        // §3.6: a PriorityCharging profile "will overrule the TxDefaultProfile or TxProfile",
        // and a TxProfile in turn overrules the TxDefaultProfile (figure 122). These three
        // are one purpose slot, not three.
        let session = leading(Purpose::PriorityCharging)
            .or_else(|| leading(Purpose::TxProfile))
            .or_else(|| leading(Purpose::TxDefaultProfile));

        // §3.6: "the lowest charging limit … among the leading profiles of the different
        // purposes". The station maximum and external constraints are the other two.
        let mut effective = session;
        for purpose in [
            Purpose::ChargingStationMaxProfile,
            Purpose::ChargingStationExternalConstraints,
        ] {
            if let Some((limit, phases)) = leading(purpose) {
                effective = Some(match effective {
                    Some((current, current_phases)) if current <= limit => {
                        (current, current_phases)
                    }
                    // Either nothing constrained this instant yet, or this purpose is lower.
                    _ => (limit, phases),
                });
            }
        }

        // §3.6: "If a charging profile of chargingProfilePurpose = LocalGeneration is active
        // for the EVSE, then this capacity is **added on top** of the calculated composite
        // schedule." It is generation, not a constraint — taking the minimum with it would
        // turn a solar array into a limit.
        if let Some((generation, phases)) = leading(Purpose::LocalGeneration) {
            effective = match effective {
                // The sum overflows only past 19 digits, at which point the honest answer is
                // the constraint that was already there rather than a wrapped one.
                Some((current, current_phases)) => Some((
                    current.checked_add(generation).unwrap_or(current),
                    current_phases,
                )),
                None => Some((generation, phases)),
            };
        }

        effective
    }
}

/// Whether two consecutive steps say the same thing, and so merge into one.
///
/// The limits are exact decimals, so this is an exact comparison — not the epsilon that a
/// float would need, and that is wrong at both ends of the range it has to cover.
fn same_step(previous: CompositePeriod, next: CompositePeriod) -> bool {
    previous.limit == next.limit && previous.number_phases == next.number_phases
}

/// Seconds since the Unix epoch.
fn epoch(at: DateTime) -> i64 {
    at.timestamp().as_second()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal;

    fn at(text: &str) -> DateTime {
        DateTime::parse(text).unwrap()
    }

    fn schedule(rate_unit: RateUnit, steps: &[(i64, Decimal)]) -> Schedule {
        Schedule::new(
            1,
            rate_unit,
            steps
                .iter()
                .map(|(start, limit)| Period::new(*start, *limit))
                .collect(),
        )
    }

    #[test]
    fn a_single_absolute_profile_becomes_its_own_composite() {
        let mut store = ProfileStore::new();
        store.install(Profile::new(
            1,
            0,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(16.0)), (3600, decimal!(10.0))])
                .starting(at("2024-01-01T00:00:00Z")),
        ));

        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 7200, RateUnit::A, None);
        assert_eq!(
            composite.periods,
            alloc::vec![
                CompositePeriod {
                    start_period: 0,
                    limit: Some(decimal!(16.0)),
                    number_phases: None
                },
                CompositePeriod {
                    start_period: 3600,
                    limit: Some(decimal!(10.0)),
                    number_phases: None
                },
            ]
        );
    }

    #[test]
    fn the_highest_stack_level_wins_within_a_purpose() {
        let mut store = ProfileStore::new();
        store.install(Profile::new(
            1,
            0,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(32.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));
        store.install(Profile::new(
            2,
            5,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(6.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));

        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 60, RateUnit::A, None);
        assert_eq!(composite.periods[0].limit, Some(decimal!(6.0)));
    }

    #[test]
    fn the_station_maximum_is_a_ceiling_over_the_session_profile() {
        let mut store = ProfileStore::new();
        store.install(Profile::new(
            1,
            0,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(32.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));
        store.install(Profile::new(
            2,
            0,
            Purpose::ChargingStationMaxProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(20.0)), (1800, decimal!(40.0))])
                .starting(at("2024-01-01T00:00:00Z")),
        ));

        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 3600, RateUnit::A, None);
        // First the ceiling binds, then the session profile does.
        assert_eq!(composite.periods[0].limit, Some(decimal!(20.0)));
        assert_eq!(composite.periods[1].start_period, 1800);
        assert_eq!(composite.periods[1].limit, Some(decimal!(32.0)));
    }

    /// §3.6: the leading schedule is the highest stack level "**that is valid at that
    /// time**" and "**has a schedule period defined for that time**". A higher stack level
    /// that says nothing about this instant is not leading, and must not shadow one that
    /// does — otherwise a holiday exception profile would blank out the weekly default it
    /// was layered on top of, and the EVSE would be left unconstrained.
    #[test]
    fn a_higher_stack_level_outside_its_window_does_not_shadow_a_lower_one() {
        let mut store = ProfileStore::new();
        // The weekly default: always applicable.
        store.install(Profile::new(
            1,
            0,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(32.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));
        // The exception: higher stack level, but only in effect for one hour.
        store.install(
            Profile::new(
                2,
                5,
                Purpose::TxDefaultProfile,
                ProfileKind::Absolute,
                schedule(RateUnit::A, &[(0, decimal!(6.0))]).starting(at("2024-01-01T00:00:00Z")),
            )
            .valid(None, Some(at("2024-01-01T01:00:00Z"))),
        );

        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 7200, RateUnit::A, None);
        assert_eq!(
            composite.periods[0].limit,
            Some(decimal!(6.0)),
            "inside the exception's window it leads"
        );
        assert_eq!(
            composite.periods.last().and_then(|period| period.limit),
            Some(decimal!(32.0)),
            "outside it, the weekly default leads again: {:?}",
            composite.periods
        );
    }

    /// §3.6: "If a charging profile of `chargingProfilePurpose` = `LocalGeneration` is active
    /// the EVSE, then this capacity is **added on top** of the calculated composite
    /// schedule." Minimising with it instead would let a solar array *reduce* what the EVSE
    /// may draw.
    #[test]
    fn local_generation_is_added_on_top_rather_than_minimised() {
        let mut store = ProfileStore::new();
        store.install(Profile::new(
            1,
            0,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::W, &[(0, decimal!(11_000.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));
        store.install(Profile::new(
            2,
            0,
            Purpose::LocalGeneration,
            ProfileKind::Absolute,
            schedule(RateUnit::W, &[(0, decimal!(4_000.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));

        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 60, RateUnit::W, None);
        assert_eq!(composite.periods[0].limit, Some(decimal!(15_000.0)));
    }

    #[test]
    fn a_tx_profile_replaces_the_default_rather_than_combining_with_it() {
        let mut store = ProfileStore::new();
        store.install(Profile::new(
            1,
            9,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(6.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));
        store.install(
            Profile::new(
                2,
                0,
                Purpose::TxProfile,
                ProfileKind::Absolute,
                schedule(RateUnit::A, &[(0, decimal!(32.0))]).starting(at("2024-01-01T00:00:00Z")),
            )
            .for_transaction("tx-1"),
        );

        // Without the transaction, the TxProfile does not apply at all.
        let without = store.composite(1, at("2024-01-01T00:00:00Z"), 60, RateUnit::A, None);
        assert_eq!(without.periods[0].limit, Some(decimal!(6.0)));

        // With it, it replaces the default even though its stack level is lower.
        let with = store.composite(
            1,
            at("2024-01-01T00:00:00Z"),
            60,
            RateUnit::A,
            Some(("tx-1", at("2024-01-01T00:00:00Z"))),
        );
        assert_eq!(with.periods[0].limit, Some(decimal!(32.0)));
    }

    #[test]
    fn a_recurring_profile_repeats_within_the_window() {
        let mut store = ProfileStore::new();
        store.install(
            Profile::new(
                1,
                0,
                Purpose::TxDefaultProfile,
                ProfileKind::Recurring,
                schedule(RateUnit::A, &[(0, decimal!(6.0)), (43_200, decimal!(32.0))])
                    .starting(at("2024-01-01T00:00:00Z")),
            )
            .recurring(Recurrency::Daily),
        );

        let composite = store.composite(1, at("2024-01-03T00:00:00Z"), 172_800, RateUnit::A, None);
        let limits: Vec<Option<Decimal>> = composite.periods.iter().map(|p| p.limit).collect();
        // Cheap overnight, expensive by day, twice over two days.
        assert_eq!(
            limits,
            alloc::vec![
                Some(decimal!(6.0)),
                Some(decimal!(32.0)),
                Some(decimal!(6.0)),
                Some(decimal!(32.0))
            ]
        );
        assert_eq!(composite.periods[1].start_period, 43_200);
        assert_eq!(composite.periods[2].start_period, 86_400);
    }

    #[test]
    fn a_validity_window_bounds_a_profile() {
        let mut store = ProfileStore::new();
        store.install(
            Profile::new(
                1,
                0,
                Purpose::TxDefaultProfile,
                ProfileKind::Absolute,
                schedule(RateUnit::A, &[(0, decimal!(16.0))]).starting(at("2024-01-01T00:00:00Z")),
            )
            .valid(None, Some(at("2024-01-01T01:00:00Z"))),
        );
        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 7200, RateUnit::A, None);
        // One step while the profile is valid, then an explicit gap. The second step is the
        // point: after `validTo` nothing constrains the EVSE, and carrying 16 A through the
        // rest of the window would report a limit that no installed profile imposes.
        assert_eq!(composite.periods.len(), 2, "{:?}", composite.periods);
        assert_eq!(composite.periods[0].limit, Some(decimal!(16.0)));
        assert_eq!(composite.periods[1].start_period, 3600);
        assert_eq!(composite.periods[1].limit, None);
        assert!(composite.has_gaps());

        // `GetCompositeSchedule` has to name a number, so the caller supplies the EVSE's
        // rated maximum for the stretch nothing constrains.
        let filled = composite.fill_gaps(decimal!(32.0));
        assert_eq!(filled.periods[1].limit, Some(decimal!(32.0)));
        assert!(!filled.has_gaps());
    }

    #[test]
    fn limits_convert_between_amperes_and_watts() {
        let mut store = ProfileStore::new();
        store.supply = Supply {
            voltage: decimal!(230.0),
            phases: 3,
        };
        store.install(Profile::new(
            1,
            0,
            Purpose::TxDefaultProfile,
            ProfileKind::Absolute,
            schedule(RateUnit::A, &[(0, decimal!(16.0))]).starting(at("2024-01-01T00:00:00Z")),
        ));
        let composite = store.composite(1, at("2024-01-01T00:00:00Z"), 60, RateUnit::W, None);
        // Exactly 11040 W, not 11039.999999999998: the conversion is a decimal
        // multiplication, so there is nothing to compare with a tolerance.
        assert_eq!(composite.periods[0].limit, Some(decimal!(11040)));
    }

    #[test]
    fn clearing_by_criteria_removes_only_what_matches() {
        let mut store = ProfileStore::new();
        for id in 1..=3 {
            store.install(
                Profile::new(
                    id,
                    id,
                    Purpose::TxDefaultProfile,
                    ProfileKind::Absolute,
                    schedule(RateUnit::A, &[(0, decimal!(16.0))])
                        .starting(at("2024-01-01T00:00:00Z")),
                )
                .on_evse(u32::try_from(id).unwrap()),
            );
        }
        assert_eq!(store.clear(None, Some(2), None, None), 1);
        assert_eq!(store.profiles().len(), 2);
        assert_eq!(
            store.clear(None, None, None, None),
            2,
            "no criteria clears everything"
        );
    }

    #[test]
    fn an_ended_transaction_takes_its_profile_with_it() {
        let mut store = ProfileStore::new();
        store.install(
            Profile::new(
                1,
                0,
                Purpose::TxProfile,
                ProfileKind::Absolute,
                schedule(RateUnit::A, &[(0, decimal!(32.0))]).starting(at("2024-01-01T00:00:00Z")),
            )
            .for_transaction("tx-1"),
        );
        assert_eq!(store.clear_transaction("tx-2"), 0);
        assert_eq!(store.clear_transaction("tx-1"), 1);
        assert!(store.profiles().is_empty());
    }
}
