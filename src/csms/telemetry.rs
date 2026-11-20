//! What a station is drawing, per phase — the telemetry half of a meter value.
//!
//! [`events`](super::events) turns meter values into the *settlement* half, an exact energy
//! register. A controller — a home energy manager's arbiter, a site load manager — runs on the
//! other half: current, power and voltage per phase, with where and when they were measured.
//! Each version spells it slightly differently, and two drivers written against the same
//! specification routinely disagree about what a sample means. These are the rules both should
//! have shared, all from the specification's own defaults (2.x Part 2 §2.87
//! `SampledValueType`, 1.6 §7.45):
//!
//! * **No `phase` is an overall value** — "When phase is absent, the measured value is
//!   interpreted as an overall value." Not phase 1.
//! * **`L1` and `L1-N` are line 1.** A current is measured on a line, and a power between a line
//!   and neutral is that line's power. A line-to-line value (`L1-L2`) is not a per-phase value,
//!   and `N` is the neutral conductor.
//! * **`location` defaults to `Outlet`** — what reaches the EV, not what the station draws from
//!   the grid (`Inlet`).
//! * **Units are normalized exactly** to the SI base unit — W, Wh, A, V, var, varh, VA, `VAh`, Hz,
//!   % — by moving the decimal point for a `k`/`M` prefix and the 2.x `multiplier`. A sample
//!   with no unit takes its measurand's natural one (`Power.*` is W, `Current.*` is A); the
//!   schema's literal default, `Wh`, is only right for energy. A unit the kit does not know is
//!   kept verbatim, never guessed.
//! * **Only power and energy add up.** Three phase powers make a total; three phase currents do
//!   not, and neither do voltages.
//!
//! ```
//! use ocpp_kit::csms::events::observe_v21;
//! use ocpp_kit::csms::telemetry::{measurand, Location, Total};
//! use ocpp_kit::types::Decimal;
//! use ocpp_kit::v2_1;
//!
//! let request: v2_1::MeterValuesRequest = serde_json::from_str(r#"{
//!   "evseId": 1,
//!   "meterValue": [{ "timestamp": "2026-11-20T12:00:00Z", "sampledValue": [
//!     { "value": 16.0, "measurand": "Current.Import", "phase": "L1" },
//!     { "value": 15.8, "measurand": "Current.Import", "phase": "L2" },
//!     { "value": 0.1,  "measurand": "Current.Import", "phase": "L3" },
//!     { "value": 7.3,  "measurand": "Power.Active.Import", "unitOfMeasure": { "unit": "kW" } }
//!   ]}]
//! }"#)?;
//! let observed = observe_v21(&v2_1::CsRequest::MeterValues(request));
//! let measurements = observed.event.measurements();
//!
//! let current = measurements.per_phase(measurand::CURRENT_IMPORT);
//! assert_eq!(current.lines[1], Some(Decimal::from_literal("15.8")));
//! assert_eq!(current.total, None, "currents are not summed");
//! assert_eq!(current.phases_in_use(Decimal::from(1)), Some(2));
//!
//! let power = measurements.per_phase(measurand::POWER_ACTIVE_IMPORT);
//! assert_eq!(power.total, Some(Total::Reported(Decimal::from(7300))));
//! # Ok::<(), serde_json::Error>(())
//! ```

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Deref;

use crate::types::{DateTime, Decimal};

/// The measurands a controller reads most, spelled as every version spells them.
pub mod measurand {
    /// The default measurand: the active import energy register.
    pub const ENERGY_ACTIVE_IMPORT_REGISTER: &str = "Energy.Active.Import.Register";
    /// Active import power.
    pub const POWER_ACTIVE_IMPORT: &str = "Power.Active.Import";
    /// Active export power.
    pub const POWER_ACTIVE_EXPORT: &str = "Power.Active.Export";
    /// The power the station offers the EV.
    pub const POWER_OFFERED: &str = "Power.Offered";
    /// Import current.
    pub const CURRENT_IMPORT: &str = "Current.Import";
    /// Export current.
    pub const CURRENT_EXPORT: &str = "Current.Export";
    /// The current the station offers the EV.
    pub const CURRENT_OFFERED: &str = "Current.Offered";
    /// Voltage.
    pub const VOLTAGE: &str = "Voltage";
    /// Grid frequency.
    pub const FREQUENCY: &str = "Frequency";
    /// The EV's state of charge.
    pub const SOC: &str = "SoC";
    /// The setpoint the station is following (2.1 block Q §2.4).
    pub const POWER_ACTIVE_SETPOINT: &str = "Power.Active.Setpoint";
    /// How far the measured power is from the setpoint: positive when charging or discharging
    /// harder than asked, negative when softer (2.1 block Q §2.4).
    pub const POWER_ACTIVE_RESIDUAL: &str = "Power.Active.Residual";
}

/// How a sampled value is to be read — the same ten values in every version.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Line 1.
    L1,
    /// Line 2.
    L2,
    /// Line 3.
    L3,
    /// The neutral conductor.
    N,
    /// Line 1 to neutral.
    L1N,
    /// Line 2 to neutral.
    L2N,
    /// Line 3 to neutral.
    L3N,
    /// Line 1 to line 2.
    L1L2,
    /// Line 2 to line 3.
    L2L3,
    /// Line 3 to line 1.
    L3L1,
    /// A value no version defines, kept verbatim.
    Other(String),
}

impl Phase {
    /// Reads the wire spelling.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "L1" => Self::L1,
            "L2" => Self::L2,
            "L3" => Self::L3,
            "N" => Self::N,
            "L1-N" => Self::L1N,
            "L2-N" => Self::L2N,
            "L3-N" => Self::L3N,
            "L1-L2" => Self::L1L2,
            "L2-L3" => Self::L2L3,
            "L3-L1" => Self::L3L1,
            other => Self::Other(other.into()),
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
            Self::N => "N",
            Self::L1N => "L1-N",
            Self::L2N => "L2-N",
            Self::L3N => "L3-N",
            Self::L1L2 => "L1-L2",
            Self::L2L3 => "L2-L3",
            Self::L3L1 => "L3-L1",
            Self::Other(other) => other,
        }
    }

    /// Which line this is a per-phase value of: `L1` and `L1-N` are line 1, and so on.
    /// `None` for the neutral, a line-to-line value and anything unknown.
    #[must_use]
    pub const fn line(&self) -> Option<u8> {
        match self {
            Self::L1 | Self::L1N => Some(1),
            Self::L2 | Self::L2N => Some(2),
            Self::L3 | Self::L3N => Some(3),
            _ => None,
        }
    }
}

/// Where a value was measured. `Outlet` when the station does not say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Location {
    /// At the connector the EV is plugged into — what the EV gets. The default.
    #[default]
    Outlet,
    /// Where the station is connected to the grid.
    Inlet,
    /// In the cable.
    Cable,
    /// Reported by the EV.
    Ev,
    /// Inside the station.
    Body,
    /// Upstream of the station (2.1).
    Upstream,
    /// A value no version defines, kept verbatim.
    Other(String),
}

impl Location {
    /// Reads the wire spelling; `None` is the default, `Outlet`.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            None | Some("Outlet") => Self::Outlet,
            Some("Inlet") => Self::Inlet,
            Some("Cable") => Self::Cable,
            Some("EV") => Self::Ev,
            Some("Body") => Self::Body,
            Some("Upstream") => Self::Upstream,
            Some(other) => Self::Other(other.into()),
        }
    }
}

/// The unit a [`Measurement`] is in, after normalization: always the SI base unit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Unit {
    /// Watt-hours (from `Wh`, `kWh`, `MWh`).
    Wh,
    /// Watts.
    W,
    /// Reactive volt-ampere hours.
    Varh,
    /// Reactive volt-amperes.
    Var,
    /// Apparent volt-ampere hours.
    VAh,
    /// Apparent volt-amperes.
    VA,
    /// Amperes.
    A,
    /// Volts.
    V,
    /// Hertz.
    Hz,
    /// Percent (`SoC`).
    Percent,
    /// Degrees Celsius.
    Celsius,
    /// Degrees Fahrenheit.
    Fahrenheit,
    /// Kelvin.
    Kelvin,
    /// No unit was sent, and the measurand has no natural one (`Power.Factor`, `RPM`).
    Unspecified,
    /// A unit the kit does not know, verbatim — the value is *not* converted.
    Other(String),
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wh => "Wh",
            Self::W => "W",
            Self::Varh => "varh",
            Self::Var => "var",
            Self::VAh => "VAh",
            Self::VA => "VA",
            Self::A => "A",
            Self::V => "V",
            Self::Hz => "Hz",
            Self::Percent => "%",
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
            Self::Kelvin => "K",
            Self::Unspecified => "",
            Self::Other(other) => other,
        })
    }
}

#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
/// The unit a sample of `measurand` means when it names none.
fn natural_unit(measurand: &str) -> Unit {
    let starts = |prefix| measurand.starts_with(prefix);
    if starts("Energy.Active") {
        Unit::Wh
    } else if starts("Energy.Reactive") {
        Unit::Varh
    } else if starts("Energy.Apparent") {
        Unit::VAh
    } else if starts("Power.Active") || starts("Power.Offered") {
        Unit::W
    } else if starts("Power.Reactive") {
        Unit::Var
    } else if starts("Power.Apparent") {
        Unit::VA
    } else if starts("Current") {
        Unit::A
    } else if starts("Voltage") {
        Unit::V
    } else if starts("Frequency") {
        Unit::Hz
    } else if starts("SoC") {
        Unit::Percent
    } else if starts("Temperature") {
        Unit::Celsius
    } else {
        Unit::Unspecified
    }
}

#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
/// A unit as sent, and how many powers of ten its prefix carries.
fn parse_unit(unit: &str) -> Option<(Unit, i32)> {
    let same = |a: &str| unit.eq_ignore_ascii_case(a);
    Some(if same("Wh") {
        (Unit::Wh, 0)
    } else if same("kWh") {
        (Unit::Wh, 3)
    } else if same("MWh") {
        (Unit::Wh, 6)
    } else if same("W") {
        (Unit::W, 0)
    } else if same("kW") {
        (Unit::W, 3)
    } else if same("MW") {
        (Unit::W, 6)
    } else if same("varh") {
        (Unit::Varh, 0)
    } else if same("kvarh") {
        (Unit::Varh, 3)
    } else if same("var") {
        (Unit::Var, 0)
    } else if same("kvar") {
        (Unit::Var, 3)
    } else if same("VAh") {
        (Unit::VAh, 0)
    } else if same("kVAh") {
        (Unit::VAh, 3)
    } else if same("VA") {
        (Unit::VA, 0)
    } else if same("kVA") {
        (Unit::VA, 3)
    } else if same("A") {
        (Unit::A, 0)
    } else if same("V") {
        (Unit::V, 0)
    } else if same("Hz") {
        (Unit::Hz, 0)
    } else if same("Percent") || same("%") {
        (Unit::Percent, 0)
    } else if same("Celsius") {
        (Unit::Celsius, 0)
    } else if same("Fahrenheit") {
        (Unit::Fahrenheit, 0)
    } else if same("K") {
        (Unit::Kelvin, 0)
    } else {
        return None;
    })
}

#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
/// Why a sample could not be normalized.
pub(crate) enum NormalizeProblem {
    /// The unit is not one the kit knows; the measurement is still carried, in that unit.
    UnknownUnit,
    /// The multiplier or prefix pushes the value out of range.
    Unrepresentable,
}

#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
/// Normalizes `value` into its SI base unit. On an unknown unit the value is returned with the
/// multiplier applied and the unit verbatim, alongside the problem to report.
pub(crate) fn normalize(
    value: Decimal,
    unit: Option<&str>,
    multiplier: Option<i32>,
    measurand: &str,
) -> Result<(Decimal, Unit, Option<NormalizeProblem>), NormalizeProblem> {
    let (unit, exponent, problem) = match unit {
        None => (natural_unit(measurand), 0, None),
        Some(text) => match parse_unit(text) {
            Some((unit, exponent)) => (unit, exponent, None),
            None => (
                Unit::Other(text.into()),
                0,
                Some(NormalizeProblem::UnknownUnit),
            ),
        },
    };
    let value = value
        .checked_pow10(multiplier.unwrap_or(0))
        .and_then(|scaled| scaled.checked_pow10(exponent))
        .ok_or(NormalizeProblem::Unrepresentable)?;
    Ok((value, unit, problem))
}

/// One sampled value, with every default the specification gives applied.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Measurement {
    /// What was measured, as the station spelled it — `Energy.Active.Import.Register` when it
    /// named nothing. See [`measurand`] for the common ones.
    pub measurand: String,
    /// How to read it; `None` is an overall value, not phase 1.
    pub phase: Option<Phase>,
    /// Where it was measured.
    pub location: Location,
    /// Why it was taken — `Sample.Periodic` when the station did not say.
    pub context: String,
    /// The value in [`unit`](Self::unit), exact.
    pub value: Decimal,
    /// The SI base unit the value was normalized to.
    pub unit: Unit,
    /// When the group of samples it came with was taken.
    pub timestamp: Option<DateTime>,
}

/// A total over all phases, and how it was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Total {
    /// The station reported it — a sample without a `phase`.
    Reported(Decimal),
    /// The station reported all three lines and no total; this is their sum. Only for power and
    /// energy, which add up.
    Summed(Decimal),
}

impl Total {
    /// The value, however it was obtained.
    #[must_use]
    pub const fn value(self) -> Decimal {
        match self {
            Self::Reported(value) | Self::Summed(value) => value,
        }
    }
}

/// One measurand at one location, per phase. Each slot holds the latest sample for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PerPhase {
    /// Lines 1–3 (`lines[0]` is line 1).
    pub lines: [Option<Decimal>; 3],
    /// The neutral conductor.
    pub neutral: Option<Decimal>,
    /// The total over all phases.
    pub total: Option<Total>,
    /// The unit of every value here.
    pub unit: Option<Unit>,
}

impl PerPhase {
    /// How many lines carry at least `minimum` — for a current, how many phases the EV is
    /// actually using, which "based on received `MeterValues` the CSMS can determine"
    /// (K01.FR.43 note). `None` when no line was reported.
    #[must_use]
    pub fn phases_in_use(&self, minimum: Decimal) -> Option<u8> {
        if self.lines.iter().all(Option::is_none) {
            return None;
        }
        let in_use = self
            .lines
            .iter()
            .flatten()
            .filter(|value| **value >= minimum)
            .count();
        u8::try_from(in_use).ok()
    }
}

/// Every measurement a message carried, in arrival order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Measurements(pub(crate) Vec<Measurement>);

impl Deref for Measurements {
    type Target = [Measurement];

    fn deref(&self) -> &[Measurement] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a Measurements {
    type Item = &'a Measurement;
    type IntoIter = core::slice::Iter<'a, Measurement>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl From<Vec<Measurement>> for Measurements {
    fn from(measurements: Vec<Measurement>) -> Self {
        Self(measurements)
    }
}

impl Measurements {
    /// `measurand` per phase, at the `Outlet`.
    #[must_use]
    pub fn per_phase(&self, measurand: &str) -> PerPhase {
        self.per_phase_at(measurand, &Location::Outlet)
    }

    /// `measurand` per phase, at `location`.
    #[must_use]
    pub fn per_phase_at(&self, measurand: &str, location: &Location) -> PerPhase {
        let mut view = PerPhase::default();
        let mut seen: [Option<Option<DateTime>>; 5] = [None; 5];
        for measurement in &self.0 {
            if measurement.measurand != measurand || measurement.location != *location {
                continue;
            }
            let slot = match &measurement.phase {
                None => 4,
                Some(Phase::N) => 3,
                Some(phase) => match phase.line() {
                    Some(line) => usize::from(line - 1),
                    None => continue,
                },
            };
            // The latest sample wins; among equal timestamps, the one that arrived last.
            if seen[slot].is_some_and(|at| at > measurement.timestamp) {
                continue;
            }
            seen[slot] = Some(measurement.timestamp);
            view.unit = Some(measurement.unit.clone());
            match slot {
                0..=2 => view.lines[slot] = Some(measurement.value),
                3 => view.neutral = Some(measurement.value),
                _ => view.total = Some(Total::Reported(measurement.value)),
            }
        }
        if view.total.is_none() && is_additive(measurand) {
            if let [Some(a), Some(b), Some(c)] = view.lines {
                view.total = a
                    .checked_add(b)
                    .and_then(|ab| ab.checked_add(c))
                    .map(Total::Summed);
            }
        }
        view
    }

    /// The latest measurement of `measurand` with `phase`, at the `Outlet`.
    #[must_use]
    pub fn latest(&self, measurand: &str, phase: Option<&Phase>) -> Option<&Measurement> {
        self.0
            .iter()
            .filter(|m| {
                m.measurand == measurand
                    && m.phase.as_ref() == phase
                    && m.location == Location::Outlet
            })
            .fold(None, |best: Option<&Measurement>, m| match best {
                Some(seen) if seen.timestamp > m.timestamp => Some(seen),
                _ => Some(m),
            })
    }
}

/// Whether a measurand's phase values add up to a total: active and reactive power and every
/// energy register. Not currents, voltages, frequency, power factor or apparent power, whose
/// three-phase total is not the arithmetic sum.
fn is_additive(measurand: &str) -> bool {
    measurand.starts_with("Power.Active")
        || measurand.starts_with("Power.Reactive")
        || measurand.starts_with("Energy.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str, phase: Option<Phase>) -> Measurement {
        Measurement {
            measurand: measurand::POWER_ACTIVE_IMPORT.into(),
            phase,
            location: Location::Outlet,
            context: "Sample.Periodic".into(),
            value: Decimal::from_literal(value),
            unit: Unit::W,
            timestamp: None,
        }
    }

    #[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
    #[test]
    fn units_are_normalized_exactly_and_unknown_ones_kept() {
        let ok = |v: &str, unit: Option<&str>, mult: Option<i32>, m: &str| {
            let (value, unit, problem) = normalize(Decimal::from_literal(v), unit, mult, m)
                .ok()
                .unwrap();
            assert!(problem.is_none());
            (value, unit)
        };
        assert_eq!(
            ok("7.4", Some("kW"), None, "Power.Active.Import"),
            (Decimal::from(7400), Unit::W)
        );
        assert_eq!(
            ok("74", None, Some(2), "Power.Active.Import"),
            (Decimal::from(7400), Unit::W)
        );
        assert_eq!(
            ok("16", None, None, "Current.Import"),
            (Decimal::from(16), Unit::A)
        );
        assert_eq!(
            ok("230", None, None, "Voltage"),
            (Decimal::from(230), Unit::V)
        );
        assert_eq!(
            ok("1.5", Some("KWH"), None, "Energy.Active.Import.Register"),
            (Decimal::from(1500), Unit::Wh)
        );
        assert_eq!(
            ok("0.98", None, None, "Power.Factor"),
            (Decimal::from_literal("0.98"), Unit::Unspecified)
        );

        let (value, unit, problem) = normalize(
            Decimal::from(3),
            Some("furlongs"),
            Some(1),
            "Power.Active.Import",
        )
        .ok()
        .unwrap();
        assert_eq!(
            (value, unit),
            (Decimal::from(30), Unit::Other("furlongs".into()))
        );
        assert!(matches!(problem, Some(NormalizeProblem::UnknownUnit)));

        assert!(matches!(
            normalize(
                Decimal::from(1),
                Some("kW"),
                Some(30),
                "Power.Active.Import"
            ),
            Err(NormalizeProblem::Unrepresentable)
        ));
    }

    #[test]
    fn line_to_neutral_is_the_line_and_line_to_line_is_no_line() {
        assert_eq!(Phase::parse("L2-N").line(), Some(2));
        assert_eq!(Phase::parse("L2").line(), Some(2));
        assert_eq!(Phase::parse("L1-L2").line(), None);
        assert_eq!(Phase::parse("N").line(), None);
        assert_eq!(Phase::parse("L4"), Phase::Other("L4".into()));
    }

    #[test]
    fn power_sums_currents_and_voltages_do_not() {
        let power = Measurements(vec![
            at("3000", Some(Phase::L1N)),
            at("3000", Some(Phase::L2N)),
            at("1000", Some(Phase::L3N)),
        ]);
        assert_eq!(
            power.per_phase(measurand::POWER_ACTIVE_IMPORT).total,
            Some(Total::Summed(Decimal::from(7000)))
        );

        let partial = Measurements(vec![
            at("3000", Some(Phase::L1)),
            at("3000", Some(Phase::L2)),
        ]);
        assert_eq!(
            partial.per_phase(measurand::POWER_ACTIVE_IMPORT).total,
            None,
            "a partial sum understates"
        );

        let mut current = Measurements(vec![
            at("16", Some(Phase::L1)),
            at("16", Some(Phase::L2)),
            at("16", Some(Phase::L3)),
        ]);
        for m in &mut current.0 {
            m.measurand = measurand::CURRENT_IMPORT.into();
        }
        assert_eq!(current.per_phase(measurand::CURRENT_IMPORT).total, None);
        assert_eq!(
            current
                .per_phase(measurand::CURRENT_IMPORT)
                .phases_in_use(Decimal::from(1)),
            Some(3)
        );
    }

    #[test]
    fn a_reported_total_wins_and_the_latest_sample_wins() {
        let early: DateTime = "2026-11-20T12:00:00Z".parse().unwrap();
        let late: DateTime = "2026-11-20T12:00:10Z".parse().unwrap();
        let mut a = at("100", None);
        a.timestamp = Some(late);
        let mut b = at("50", None);
        b.timestamp = Some(early);
        let view = Measurements(vec![
            a,
            b,
            at("1", Some(Phase::L1)),
            at("1", Some(Phase::L2)),
            at("1", Some(Phase::L3)),
        ])
        .per_phase(measurand::POWER_ACTIVE_IMPORT);
        assert_eq!(view.total, Some(Total::Reported(Decimal::from(100))));
    }

    #[test]
    fn the_outlet_is_the_default_location() {
        let mut inlet = at("9000", None);
        inlet.location = Location::Inlet;
        let view = Measurements(vec![inlet, at("7000", None)]);
        assert_eq!(
            view.per_phase(measurand::POWER_ACTIVE_IMPORT)
                .total
                .map(Total::value),
            Some(Decimal::from(7000))
        );
        assert_eq!(
            view.per_phase_at(measurand::POWER_ACTIVE_IMPORT, &Location::Inlet)
                .total
                .map(Total::value),
            Some(Decimal::from(9000))
        );
    }
}
