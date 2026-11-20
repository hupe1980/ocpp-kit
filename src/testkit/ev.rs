//! A vehicle to test a charging controller against.

use std::collections::VecDeque;
use std::time::Duration;

use crate::types::{DateTime, Decimal};

/// One watt-hour in the model's internal unit, milliwatt-milliseconds. Integer arithmetic in
/// that unit is exact; a figure in Wh is rounded to the milliwatt-hour only when it is read.
const WH: i128 = 3_600_000_000;

/// What the vehicle does when the station changes the number of phases mid-session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseSwitching {
    /// It follows after drawing nothing for `pause` — the contactors open, the vehicle wakes up
    /// again on the new phase count.
    Follows {
        /// How long it draws nothing.
        pause: Duration,
    },
    /// It stops charging until it is unplugged — the vehicles that do not recover from a switch.
    StopsCharging,
}

/// A ceiling on what the vehicle may draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// Amperes per phase — what a pilot signal says.
    Amps(Decimal),
    /// Watts over all phases in use (K01.FR.50).
    Watts(Decimal),
}

/// A power target for a bidirectional vehicle, as a magnitude with its direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    /// Charge with this many watts.
    Charge(Decimal),
    /// Discharge with this many watts.
    Discharge(Decimal),
}

/// What the station allows or asks of the vehicle: a ceiling, a phase count, and for a
/// bidirectional vehicle a power target.
///
/// ```
/// use ocpp_kit::testkit::{Command, Power};
/// use ocpp_kit::types::Decimal;
///
/// let six_amps_one_phase = Command::amps(6).phases(1);
/// let discharge = Command::unlimited().target(Power::Discharge(Decimal::from(3000)));
/// # let _ = (six_amps_one_phase, discharge);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Command {
    ceiling: Option<Limit>,
    phases: Option<u8>,
    target: Option<Power>,
    discharge_ceiling: Option<Decimal>,
}

fn magnitude(value: impl Into<Decimal>, what: &str) -> Decimal {
    let value = value.into();
    assert!(value.signum() >= 0, "{what} is a magnitude, got {value}");
    value
}

impl Command {
    /// No ceiling: the vehicle draws what it can.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// At most `amps` per phase.
    ///
    /// # Panics
    ///
    /// If `amps` is negative.
    #[must_use]
    pub fn amps(amps: impl Into<Decimal>) -> Self {
        Self {
            ceiling: Some(Limit::Amps(magnitude(amps, "a current"))),
            ..Self::default()
        }
    }

    /// At most `watts` over all phases in use.
    ///
    /// # Panics
    ///
    /// If `watts` is negative.
    #[must_use]
    pub fn watts(watts: impl Into<Decimal>) -> Self {
        Self {
            ceiling: Some(Limit::Watts(magnitude(watts, "a power"))),
            ..Self::default()
        }
    }

    /// On at most `phases` phases; three when not said (K01.FR.49).
    ///
    /// # Panics
    ///
    /// Unless `phases` is 1, 2 or 3.
    #[must_use]
    pub fn phases(mut self, phases: u8) -> Self {
        assert!((1..=3).contains(&phases), "phases must be 1, 2 or 3");
        self.phases = Some(phases);
        self
    }

    /// Follow a power target — a 2.1 setpoint — under the ceiling.
    ///
    /// # Panics
    ///
    /// If the magnitude is negative.
    #[must_use]
    pub fn target(mut self, target: Power) -> Self {
        let (Power::Charge(w) | Power::Discharge(w)) = target;
        magnitude(w, "a power target");
        self.target = Some(target);
        self
    }

    /// Discharge with at most `watts` — a 2.1 `dischargeLimit`, as a magnitude.
    ///
    /// # Panics
    ///
    /// If `watts` is negative.
    #[must_use]
    pub fn discharge_at_most(mut self, watts: impl Into<Decimal>) -> Self {
        self.discharge_ceiling = Some(magnitude(watts, "a discharge limit"));
        self
    }

    /// The ceiling.
    #[must_use]
    pub fn ceiling(&self) -> Option<Limit> {
        self.ceiling
    }

    /// The phase count allowed.
    #[must_use]
    pub fn phase_count(&self) -> Option<u8> {
        self.phases
    }

    /// The power target.
    #[must_use]
    pub fn power_target(&self) -> Option<Power> {
        self.target
    }

    /// The discharge ceiling.
    #[must_use]
    pub fn discharge_ceiling(&self) -> Option<Decimal> {
        self.discharge_ceiling
    }
}

/// What the vehicle is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EvState {
    /// Not plugged in.
    Unplugged,
    /// Plugged in, no command from the station yet.
    Connected,
    /// Drawing power.
    Charging,
    /// Feeding power back.
    Discharging,
    /// Drawing nothing by its own choice: full, at its target, at its discharge floor, or asked to
    /// discharge without being able to.
    SuspendedEv,
    /// Drawing nothing because the station allows too little: below the minimum current, or a
    /// zero target.
    SuspendedEvse,
    /// Drawing nothing while it changes the number of phases.
    SwitchingPhases,
    /// Stopped after a phase switch it does not support, until it is unplugged.
    Faulted,
}

/// What the vehicle draws, at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reading {
    /// Active power in W: positive while charging, negative while discharging.
    pub power: Decimal,
    /// Current per phase in use, in A (a magnitude).
    pub current: Decimal,
    /// Phases in use.
    pub phases: u8,
    /// State of charge in %, to one decimal.
    pub soc: Decimal,
    /// Energy taken since it was built, in Wh, to the milliwatt-hour.
    pub imported: Decimal,
    /// Energy fed back since it was built, in Wh, to the milliwatt-hour.
    pub exported: Decimal,
    /// What it is doing.
    pub state: EvState,
}

/// Why a plug action was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvError {
    /// Plugged in already.
    AlreadyPlugged,
    /// Not plugged in.
    NotPlugged,
}

impl core::fmt::Display for EvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::AlreadyPlugged => "the vehicle is plugged in already",
            Self::NotPlugged => "the vehicle is not plugged in",
        })
    }
}

impl std::error::Error for EvError {}

/// A vehicle on virtual time: it follows the station's [`Command`]s the way a real one does.
///
/// It reacts after a delay; it draws nothing below its minimum current; a phase switch costs it a
/// pause, or stops it for good; it stops at its target state of charge, tapers if told to, and a
/// bidirectional vehicle stops discharging at its floor. Energy is integrated exactly, so an hour
/// at 11 040 W is 11 040 Wh however many steps it took.
///
/// ```
/// use std::time::Duration;
/// use ocpp_kit::testkit::{Command, Ev, EvState};
/// use ocpp_kit::types::Decimal;
///
/// let mut ev = Ev::builder().battery_wh(60_000).soc(20).build();
/// ev.plug_in()?;
/// ev.command(Command::amps(16));
/// ev.advance(Duration::from_secs(2)); // the reaction delay
/// assert_eq!(ev.reading().power, Decimal::from(11_040));
///
/// ev.command(Command::amps(4)); // below the 6 A it needs
/// ev.advance(Duration::from_secs(2));
/// assert_eq!(ev.state(), EvState::SuspendedEvse);
/// # Ok::<(), ocpp_kit::testkit::EvError>(())
/// ```
#[derive(Clone, Debug)]
pub struct Ev {
    capacity: i128,
    target: i128,
    floor: i128,
    phases: u8,
    min_current: Decimal,
    max_current: Decimal,
    voltage: Decimal,
    reaction: Duration,
    switching: PhaseSwitching,
    taper_above: Option<Decimal>,
    max_discharge: Option<Decimal>,
    departure: Option<DateTime>,

    plugged: bool,
    stored: i128,
    imported: i128,
    exported: i128,
    elapsed: u64,
    applied: Option<Command>,
    pending: VecDeque<(u64, Command)>,
    phases_in_use: u8,
    switching_until: Option<u64>,
    faulted: bool,
}

/// Builds an [`Ev`]. Every value has a default; see the methods.
#[derive(Clone, Debug)]
pub struct EvBuilder {
    battery_wh: Decimal,
    soc: Decimal,
    target_soc: Decimal,
    floor_soc: Decimal,
    phases: u8,
    min_current: Decimal,
    max_current: Decimal,
    voltage: Decimal,
    reaction: Duration,
    switching: PhaseSwitching,
    taper_above: Option<Decimal>,
    max_discharge: Option<Decimal>,
    departure: Option<DateTime>,
}

impl Default for EvBuilder {
    fn default() -> Self {
        Self {
            battery_wh: Decimal::from(60_000),
            soc: Decimal::from(20),
            target_soc: Decimal::from(100),
            floor_soc: Decimal::from(20),
            phases: 3,
            min_current: Decimal::from(6),
            max_current: Decimal::from(16),
            voltage: Decimal::from(230),
            reaction: Duration::from_secs(2),
            switching: PhaseSwitching::Follows {
                pause: Duration::from_secs(5),
            },
            taper_above: None,
            max_discharge: None,
            departure: None,
        }
    }
}

impl EvBuilder {
    /// Battery capacity in Wh. Default 60 000.
    #[must_use]
    pub fn battery_wh(mut self, wh: impl Into<Decimal>) -> Self {
        self.battery_wh = wh.into();
        self
    }

    /// State of charge when built, in %. Default 20.
    #[must_use]
    pub fn soc(mut self, percent: impl Into<Decimal>) -> Self {
        self.soc = percent.into();
        self
    }

    /// The state of charge it stops charging at, in %. Default 100.
    #[must_use]
    pub fn target_soc(mut self, percent: impl Into<Decimal>) -> Self {
        self.target_soc = percent.into();
        self
    }

    /// Phases its charger uses, 1–3. Default 3.
    #[must_use]
    pub fn phases(mut self, phases: u8) -> Self {
        self.phases = phases;
        self
    }

    /// Least and most current per phase, in A. Default 6–16. Below the least it draws nothing.
    #[must_use]
    pub fn current(mut self, min: impl Into<Decimal>, max: impl Into<Decimal>) -> Self {
        self.min_current = min.into();
        self.max_current = max.into();
        self
    }

    /// Line voltage, in V, for converting between A and W. Default 230.
    #[must_use]
    pub fn voltage(mut self, volts: impl Into<Decimal>) -> Self {
        self.voltage = volts.into();
        self
    }

    /// How long after a command it follows it. Default 2 s.
    #[must_use]
    pub fn reaction(mut self, delay: Duration) -> Self {
        self.reaction = delay;
        self
    }

    /// What it does when the phase count changes. Default: follows after a 5 s pause.
    #[must_use]
    pub fn phase_switching(mut self, behaviour: PhaseSwitching) -> Self {
        self.switching = behaviour;
        self
    }

    /// Above this state of charge, in %, its maximum current falls linearly to its minimum at
    /// 100 %. Default: no taper.
    #[must_use]
    pub fn taper_above(mut self, percent: impl Into<Decimal>) -> Self {
        self.taper_above = Some(percent.into());
        self
    }

    /// Makes it bidirectional: it discharges with at most `watts`. Default: charge only.
    #[must_use]
    pub fn bidirectional(mut self, watts: impl Into<Decimal>) -> Self {
        self.max_discharge = Some(watts.into());
        self
    }

    /// The state of charge it never discharges below, in %. Default 20.
    #[must_use]
    pub fn floor_soc(mut self, percent: impl Into<Decimal>) -> Self {
        self.floor_soc = percent.into();
        self
    }

    /// When it leaves, as it tells the station.
    #[must_use]
    pub fn departure(mut self, at: DateTime) -> Self {
        self.departure = Some(at);
        self
    }

    /// The vehicle, unplugged.
    ///
    /// # Panics
    ///
    /// On a configuration no vehicle has: phases outside 1–3, a minimum current above the
    /// maximum, a percentage outside 0–100, a non-positive capacity or voltage, a negative
    /// discharge power.
    #[must_use]
    pub fn build(self) -> Ev {
        let percent = |value: Decimal, what: &str| {
            assert!(
                (Decimal::ZERO..=Decimal::from(100)).contains(&value),
                "{what} must be 0–100 %, got {value}"
            );
            value
        };
        assert!((1..=3).contains(&self.phases), "phases must be 1, 2 or 3");
        assert!(
            self.min_current.signum() >= 0 && self.min_current <= self.max_current,
            "the minimum current must be between 0 and the maximum"
        );
        assert!(self.voltage.signum() > 0, "the voltage must be positive");
        assert!(
            self.battery_wh.signum() > 0,
            "the capacity must be positive"
        );
        if let Some(watts) = self.max_discharge {
            magnitude(watts, "a discharge power");
        }
        if let Some(taper) = self.taper_above {
            percent(taper, "the taper start");
        }
        let capacity = units(self.battery_wh, WH);
        let share = |pct: Decimal| capacity * units(pct, 1_000_000) / 100_000_000;
        Ev {
            capacity,
            target: share(percent(self.target_soc, "the target")),
            floor: share(percent(self.floor_soc, "the floor")),
            stored: share(percent(self.soc, "the state of charge")),
            phases: self.phases,
            min_current: self.min_current,
            max_current: self.max_current,
            voltage: self.voltage,
            reaction: self.reaction,
            switching: self.switching,
            taper_above: self.taper_above,
            max_discharge: self.max_discharge,
            departure: self.departure,
            plugged: false,
            imported: 0,
            exported: 0,
            elapsed: 0,
            applied: None,
            pending: VecDeque::new(),
            phases_in_use: self.phases,
            switching_until: None,
            faulted: false,
        }
    }
}

/// `value × per` as an integer, rounded to the nearest unit.
fn units(value: Decimal, per: i128) -> i128 {
    let scale = u32::from(value.scale());
    let mantissa = i128::from(value.mantissa()) * per;
    let divisor = 10_i128.pow(scale);
    (mantissa + divisor / 2 * mantissa.signum()) / divisor
}

/// An internal amount as a `Decimal` with three decimals: `amount / per` rounded to the
/// thousandth.
fn thousandths(amount: i128, per: i128) -> Decimal {
    let scaled = (amount * 1000 + per / 2 * amount.signum()) / per;
    Decimal::new(i64::try_from(scaled).unwrap_or(i64::MAX), 3)
}

/// `a × b` in W, or `None` on overflow.
fn product(values: &[Decimal]) -> Option<Decimal> {
    values
        .iter()
        .try_fold(Decimal::from(1), |acc, value| acc.checked_mul(*value))
}

impl Default for Ev {
    fn default() -> Self {
        EvBuilder::default().build()
    }
}

impl Ev {
    /// A builder with the defaults: 60 kWh at 20 %, target 100 %, three phases, 6–16 A, 230 V,
    /// a 2 s reaction, phase switching followed after a 5 s pause, no taper, charge only.
    #[must_use]
    pub fn builder() -> EvBuilder {
        EvBuilder::default()
    }

    /// Plugs it in. It waits for a command before it draws anything.
    ///
    /// # Errors
    ///
    /// [`EvError::AlreadyPlugged`].
    pub fn plug_in(&mut self) -> Result<(), EvError> {
        if self.plugged {
            return Err(EvError::AlreadyPlugged);
        }
        self.plugged = true;
        self.applied = None;
        self.pending.clear();
        self.phases_in_use = self.phases;
        self.switching_until = None;
        self.faulted = false;
        Ok(())
    }

    /// Unplugs it. A fault from a phase switch clears.
    ///
    /// # Errors
    ///
    /// [`EvError::NotPlugged`].
    pub fn unplug(&mut self) -> Result<(), EvError> {
        if !self.plugged {
            return Err(EvError::NotPlugged);
        }
        self.plugged = false;
        self.applied = None;
        self.pending.clear();
        self.switching_until = None;
        self.faulted = false;
        Ok(())
    }

    /// Whether it is plugged in.
    #[must_use]
    pub fn is_plugged(&self) -> bool {
        self.plugged
    }

    /// Tells it what the station allows. It follows after its reaction delay; a later command
    /// that falls due at the same instant replaces this one. Ignored while unplugged.
    pub fn command(&mut self, command: Command) {
        if !self.plugged {
            return;
        }
        let due = self.elapsed + millis(self.reaction);
        self.pending.retain(|(at, _)| *at < due);
        self.pending.push_back((due, command));
        self.apply_due();
    }

    /// The command it is following, once one has taken effect.
    #[must_use]
    pub fn applied(&self) -> Option<Command> {
        self.applied
    }

    /// Lets `by` pass.
    pub fn advance(&mut self, by: Duration) {
        let end = self.elapsed + millis(by);
        while self.elapsed < end {
            self.apply_due();
            let mut until = end;
            if let Some((at, _)) = self.pending.front() {
                until = until.min(*at);
            }
            if let Some(at) = self.switching_until.filter(|at| *at > self.elapsed) {
                until = until.min(at);
            }
            let (power, _, _) = self.operating_point();
            // While tapering, power depends on the state of charge: re-evaluate every second.
            if power > 0 && self.taper_above.is_some() {
                until = until.min(self.elapsed + 1000);
            }
            let mut step = i128::from(until - self.elapsed);
            let room = match power.signum() {
                1 => self.target - self.stored,
                -1 => self.stored - self.floor,
                _ => 0,
            };
            if power != 0 {
                // End the step where the target or floor is reached, so nothing overshoots.
                step = step.min((room + power.abs() - 1) / power.abs()).max(1);
            }
            let energy = (power * step).clamp(-room.max(0), room.max(0));
            self.stored += energy;
            if energy > 0 {
                self.imported += energy;
            } else {
                self.exported -= energy;
            }
            self.elapsed += u64::try_from(step).unwrap_or(u64::MAX);
            if self.switching_until.is_some_and(|at| at <= self.elapsed) {
                self.switching_until = None;
            }
        }
        self.apply_due();
    }

    /// What it draws now.
    #[must_use]
    pub fn reading(&self) -> Reading {
        let (power, current, state) = self.operating_point();
        let soc = Decimal::new(
            i64::try_from(self.stored * 1000 / self.capacity).unwrap_or(0),
            1,
        )
        .round_to(1)
        .unwrap_or(Decimal::ZERO);
        Reading {
            power: thousandths(power, 1000).normalized(),
            current,
            phases: self.phases_in_use,
            soc,
            imported: thousandths(self.imported, WH),
            exported: thousandths(self.exported, WH),
            state,
        }
    }

    /// What it is doing now.
    #[must_use]
    pub fn state(&self) -> EvState {
        self.operating_point().2
    }

    /// State of charge in %, to one decimal.
    #[must_use]
    pub fn soc(&self) -> Decimal {
        self.reading().soc
    }

    /// The energy it still wants, in Wh: up to its target state of charge.
    #[must_use]
    pub fn energy_request(&self) -> Decimal {
        thousandths((self.target - self.stored).max(0), WH).normalized()
    }

    /// Phases its charger uses.
    #[must_use]
    pub fn phases(&self) -> u8 {
        self.phases
    }

    /// Least and most current per phase, in A.
    #[must_use]
    pub fn current_range(&self) -> (Decimal, Decimal) {
        (self.min_current, self.max_current)
    }

    /// Its line voltage, in V.
    #[must_use]
    pub fn voltage(&self) -> Decimal {
        self.voltage
    }

    /// Most it charges with, in W: maximum current × voltage × phases.
    #[must_use]
    pub fn max_charge_power(&self) -> Decimal {
        product(&[self.max_current, self.voltage, Decimal::from(self.phases)])
            .unwrap_or(Decimal::ZERO)
    }

    /// Most it discharges with, in W, when bidirectional.
    #[must_use]
    pub fn max_discharge_power(&self) -> Option<Decimal> {
        self.max_discharge
    }

    /// Its target state of charge, in %, to one decimal.
    #[must_use]
    pub fn target_soc(&self) -> Decimal {
        Decimal::new(
            i64::try_from(self.target * 1000 / self.capacity).unwrap_or(0),
            1,
        )
    }

    /// When it leaves.
    #[must_use]
    pub fn departure(&self) -> Option<DateTime> {
        self.departure
    }

    fn apply_due(&mut self) {
        while let Some((at, command)) = self.pending.front().copied() {
            if at > self.elapsed {
                break;
            }
            self.pending.pop_front();
            let wanted = command.phases.unwrap_or(3).min(self.phases);
            if self.applied.is_none() {
                // The first command after plugging in: it starts on what it is offered.
                self.phases_in_use = wanted;
            } else if wanted != self.phases_in_use {
                match self.switching {
                    PhaseSwitching::Follows { pause } => {
                        self.phases_in_use = wanted;
                        self.switching_until = Some(at + millis(pause)).filter(|until| *until > at);
                    }
                    PhaseSwitching::StopsCharging => self.faulted = true,
                }
            }
            self.applied = Some(command);
        }
    }

    /// Power in mW (signed), current per phase in A, and state.
    fn operating_point(&self) -> (i128, Decimal, EvState) {
        let idle = |state| (0, Decimal::ZERO, state);
        if !self.plugged {
            return idle(EvState::Unplugged);
        }
        if self.faulted {
            return idle(EvState::Faulted);
        }
        let Some(command) = self.applied else {
            return idle(EvState::Connected);
        };
        if self.switching_until.is_some_and(|at| self.elapsed < at) {
            return idle(EvState::SwitchingPhases);
        }
        let phases = Decimal::from(self.phases_in_use);
        let per_phase = self.voltage.checked_mul(phases).unwrap_or(Decimal::ZERO);
        let ceiling_watts = match command.ceiling {
            None => None,
            Some(Limit::Watts(w)) => Some(w),
            Some(Limit::Amps(a)) => a.checked_mul(per_phase),
        };
        let max_current = self.tapered_max_current();
        let max_charge = max_current.checked_mul(per_phase).unwrap_or(Decimal::ZERO);
        let watts_to_amps = |watts: Decimal| {
            watts
                .checked_div(per_phase, 3)
                .unwrap_or(Decimal::ZERO)
                .normalized()
        };
        let draw = |watts: Decimal, state| {
            let mw = units(watts, 1000);
            (
                mw,
                watts_to_amps(watts.checked_abs().unwrap_or(watts)),
                state,
            )
        };

        match command.target {
            Some(Power::Discharge(target)) => {
                let Some(max) = self.max_discharge else {
                    return idle(EvState::SuspendedEv);
                };
                if self.stored <= self.floor {
                    return idle(EvState::SuspendedEv);
                }
                let mut watts = target.min(max);
                if let Some(cap) = command.discharge_ceiling {
                    watts = watts.min(cap);
                }
                if watts.is_zero() {
                    return idle(EvState::SuspendedEvse);
                }
                draw(-watts, EvState::Discharging)
            }
            Some(Power::Charge(target)) => {
                if self.stored >= self.target {
                    return idle(EvState::SuspendedEv);
                }
                let mut watts = target.min(max_charge);
                if let Some(cap) = ceiling_watts {
                    watts = watts.min(cap);
                }
                if watts.is_zero() {
                    return idle(EvState::SuspendedEvse);
                }
                draw(watts, EvState::Charging)
            }
            None => {
                if self.stored >= self.target {
                    return idle(EvState::SuspendedEv);
                }
                let offered = match command.ceiling {
                    None => max_current,
                    Some(Limit::Amps(a)) => a,
                    Some(Limit::Watts(w)) => watts_to_amps(w),
                };
                let current = offered.min(max_current);
                if current < self.min_current || current.is_zero() {
                    return idle(EvState::SuspendedEvse);
                }
                let watts = current.checked_mul(per_phase).unwrap_or(Decimal::ZERO);
                (units(watts, 1000), current.normalized(), EvState::Charging)
            }
        }
    }

    /// The maximum current, lowered linearly above the taper start down to the minimum at 100 %.
    fn tapered_max_current(&self) -> Decimal {
        let Some(start) = self.taper_above else {
            return self.max_current;
        };
        let start = units(start, 1_000_000) * self.capacity / 100_000_000;
        if self.stored <= start || self.capacity <= start {
            return self.max_current;
        }
        let span = units(self.max_current - self.min_current, 1000);
        let cut = span * (self.stored - start) / (self.capacity - start);
        thousandths(units(self.max_current, 1000) - cut, 1000).normalized()
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(value: i64) -> Decimal {
        Decimal::from(value)
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn plugged(builder: EvBuilder) -> Ev {
        let mut ev = builder.build();
        ev.plug_in().unwrap();
        ev
    }

    #[test]
    fn it_follows_a_ceiling_after_its_reaction_delay() {
        let mut ev = plugged(Ev::builder());
        assert_eq!(ev.state(), EvState::Connected);
        ev.command(Command::amps(16));
        ev.advance(Duration::from_millis(1999));
        assert_eq!(ev.reading().power, Decimal::ZERO, "not yet");
        ev.advance(Duration::from_millis(1));
        let reading = ev.reading();
        assert_eq!(reading.power, d(11_040));
        assert_eq!((reading.current, reading.phases), (d(16), 3));
        assert_eq!(reading.state, EvState::Charging);
    }

    #[test]
    fn an_hour_is_exact_however_many_steps_it_took() {
        let mut ev = plugged(Ev::builder().reaction(Duration::ZERO));
        ev.command(Command::amps(16));
        for _ in 0..3600 {
            ev.advance(secs(1));
        }
        assert_eq!(ev.reading().imported, Decimal::new(11_040_000, 3));
        let mut once = plugged(Ev::builder().reaction(Duration::ZERO));
        once.command(Command::amps(16));
        once.advance(secs(3600));
        assert_eq!(once.reading().imported, ev.reading().imported);
        // 20 % of 60 kWh plus 11.04 kWh is 38.4 %.
        assert_eq!(ev.soc(), Decimal::new(384, 1));
    }

    #[test]
    fn below_its_minimum_current_it_draws_nothing() {
        let mut ev = plugged(Ev::builder().reaction(secs(2)));
        ev.command(Command::amps(16));
        ev.advance(secs(2));
        ev.command(Command::amps(4));
        ev.advance(secs(1));
        assert_eq!(ev.reading().power, d(11_040), "still the old ceiling");
        ev.advance(secs(1));
        assert_eq!(ev.reading().power, Decimal::ZERO);
        assert_eq!(ev.state(), EvState::SuspendedEvse);
        // A watt ceiling converts with the phases in use: 4 140 W over 3 × 230 V is 6 A.
        ev.command(Command::watts(4140));
        ev.advance(secs(2));
        assert_eq!(ev.reading().current, d(6));
    }

    #[test]
    fn a_phase_switch_costs_a_pause_or_stops_it() {
        let mut ev = plugged(Ev::builder().reaction(Duration::ZERO));
        ev.command(Command::amps(16));
        ev.advance(secs(1));
        ev.command(Command::amps(16).phases(1));
        ev.advance(secs(4));
        assert_eq!(ev.state(), EvState::SwitchingPhases);
        assert_eq!(ev.reading().power, Decimal::ZERO);
        ev.advance(secs(1));
        assert_eq!((ev.reading().phases, ev.reading().power), (1, d(3680)));

        let mut stuck = plugged(
            Ev::builder()
                .reaction(Duration::ZERO)
                .phase_switching(PhaseSwitching::StopsCharging),
        );
        stuck.command(Command::amps(16));
        stuck.advance(secs(1));
        stuck.command(Command::amps(16).phases(1));
        stuck.advance(secs(600));
        assert_eq!(stuck.state(), EvState::Faulted);
        stuck.command(Command::amps(16));
        stuck.advance(secs(10));
        assert_eq!(stuck.state(), EvState::Faulted, "until unplugged");
        stuck.unplug().unwrap();
        stuck.plug_in().unwrap();
        stuck.command(Command::amps(16).phases(1));
        stuck.advance(secs(1));
        assert_eq!(
            stuck.state(),
            EvState::Charging,
            "it starts on what it is offered"
        );

        // A single-phase vehicle offered three phases uses its one.
        let mut single = plugged(Ev::builder().phases(1).reaction(Duration::ZERO));
        single.command(Command::amps(32));
        single.advance(secs(1));
        assert_eq!(
            (single.reading().phases, single.reading().current),
            (1, d(16))
        );
    }

    #[test]
    fn it_stops_at_its_target_exactly() {
        let mut ev = plugged(
            Ev::builder()
                .battery_wh(10_000)
                .soc(79)
                .target_soc(80)
                .reaction(Duration::ZERO),
        );
        ev.command(Command::unlimited());
        ev.advance(secs(3600));
        assert_eq!(ev.state(), EvState::SuspendedEv);
        assert_eq!(ev.soc(), d(80));
        assert_eq!(ev.reading().imported, d(100), "exactly 1 % of 10 kWh");
        assert_eq!(ev.energy_request(), Decimal::ZERO);
    }

    #[test]
    fn it_tapers_above_the_taper_start() {
        let mut ev = plugged(
            Ev::builder()
                .battery_wh(10_000)
                .soc(90)
                .taper_above(80)
                .reaction(Duration::ZERO),
        );
        ev.command(Command::unlimited());
        ev.advance(Duration::from_millis(1));
        // Halfway from 80 % to 100 %: halfway from 16 A to 6 A.
        assert_eq!(ev.reading().current, d(11));
    }

    #[test]
    fn a_bidirectional_vehicle_discharges_to_its_floor() {
        let mut ev = plugged(
            Ev::builder()
                .battery_wh(10_000)
                .soc(25)
                .floor_soc(20)
                .bidirectional(5000)
                .reaction(Duration::ZERO),
        );
        ev.command(Command::unlimited().target(Power::Discharge(d(3000))));
        ev.advance(secs(1));
        let reading = ev.reading();
        assert_eq!(
            (reading.power, reading.state),
            (d(-3000), EvState::Discharging)
        );
        ev.advance(secs(3600));
        assert_eq!(ev.state(), EvState::SuspendedEv);
        assert_eq!(ev.soc(), d(20));
        assert_eq!(ev.reading().exported, d(500));

        ev.command(Command::unlimited().target(Power::Charge(d(2000))));
        ev.advance(secs(1));
        assert_eq!(ev.reading().power, d(2000));
        ev.command(
            Command::unlimited()
                .target(Power::Discharge(d(3000)))
                .discharge_at_most(1000),
        );
        ev.advance(secs(1));
        assert_eq!(ev.reading().power, d(-1000));

        let mut charge_only = plugged(Ev::builder().reaction(Duration::ZERO));
        charge_only.command(Command::unlimited().target(Power::Discharge(d(3000))));
        charge_only.advance(secs(1));
        assert_eq!(charge_only.state(), EvState::SuspendedEv);
    }

    #[test]
    fn plugging_is_checked_and_time_zero_changes_nothing() {
        let mut ev = Ev::default();
        assert_eq!(ev.unplug(), Err(EvError::NotPlugged));
        ev.command(Command::amps(16));
        assert!(
            ev.applied().is_none(),
            "a command to an unplugged vehicle is ignored"
        );
        ev.plug_in().unwrap();
        assert_eq!(ev.plug_in(), Err(EvError::AlreadyPlugged));
        let before = ev.reading();
        ev.advance(Duration::ZERO);
        assert_eq!(ev.reading(), before);
    }

    #[test]
    #[should_panic(expected = "magnitude")]
    fn a_negative_ceiling_is_a_programming_error() {
        let _ = Command::amps(-1);
    }
}
