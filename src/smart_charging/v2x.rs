//! Bidirectional charging (OCPP 2.1 block Q): operation modes and setpoints.
//!
//! What the EV can do comes from [`EvNeeds`], whose
//! [`clamp_watts`](super::EvNeeds::clamp_watts) fits a setpoint into it.
//!
//! [`ChargingPlan`] tells a station what it may *draw*. A V2H or V2B
//! controller also tells it what the EV should *do* — charge at 3.7 kW, discharge 5 kW into the
//! house, sit idle — and 2.1 says that with an `operationMode` and a `setpoint` in the same
//! schedule period. Three things in block Q are easy to get wrong, and each is a type here:
//!
//! * **Each mode allows different fields** (Q §2.2, "Limits and setpoints per operation mode").
//!   `ChargingOnly` needs a `limit`; `CentralSetpoint` needs a `setpoint`; `Idle` forbids all of
//!   them (Q10.FR.01); a setpoint outside `[dischargeLimit, limit]` is rejected (V2X.05). An
//!   [`Operation`] can only be built in a shape the station will accept.
//! * **Signs disagree across messages.** A profile's `setpoint` is signed and its
//!   `dischargeLimit` negative, while the EV's `maxDischargePower` is a positive magnitude. Here
//!   nothing takes a signed discharge: [`Setpoint::discharge`] and
//!   [`Operation::discharge_at_most`] take magnitudes, and the wire gets the sign block Q wants.
//! * **A dynamic setpoint needs a dead-man switch.** When a `Dynamic` profile is not updated
//!   within its `duration`, the station falls back to the next valid profile (K28.FR.13). A
//!   [`DynamicSetpoint`] cannot be built without that duration: a controller that crashes while
//!   the EV discharges at 11 kW must not leave it discharging.
//!
//! The modes are those a home or building controller uses; the frequency modes are not modelled.
//!
//! ```
//! use core::time::Duration;
//! use ocpp_kit::smart_charging::v2x::{
//!     DynamicSetpoint, Operation, OperationModes, Setpoint, V2xPlan, V2xStation,
//! };
//! use ocpp_kit::smart_charging::{RateUnit, RateUnits, StationLimits, Target};
//! use ocpp_kit::types::{DateTime, Decimal};
//!
//! let station = V2xStation::new(
//!     StationLimits::new(24, 3, RateUnits::parse_v2("A,W")),
//!     OperationModes::parse("ChargingOnly,CentralSetpoint,Idle"),
//! );
//! let w = |watts: i64| Decimal::from(watts);
//!
//! // A day in four slots: charge cheaply at night, idle, cover the evening peak, charge.
//! let plan = V2xPlan::new(
//!     "2026-11-20T00:00:00Z".parse::<DateTime>()?,
//!     Duration::from_secs(6 * 3600),
//!     [
//!         Operation::central_setpoint(Setpoint::charge(w(3700))?),
//!         Operation::idle(),
//!         Operation::central_setpoint(Setpoint::discharge(w(5000))?).discharge_at_most(w(6000))?,
//!         Operation::charging_only(w(11000))?,
//!     ],
//! )?;
//! let request = plan.to_request(1, 1, Target::transaction(1, "tx-1"), RateUnit::W, &station)?;
//! let evening = &request.charging_profile.charging_schedule[0].charging_schedule_period[2];
//! assert_eq!(evening.setpoint, Some(w(-5000)));
//! assert_eq!(evening.discharge_limit, Some(w(-6000)));
//!
//! // The one-second arbiter: a Dynamic profile that falls back after 30 s without an update.
//! let mut follow = DynamicSetpoint::new(
//!     2,
//!     Target::transaction(1, "tx-1"),
//!     RateUnit::W,
//!     Operation::central_setpoint(Setpoint::ZERO).discharge_at_most(w(7000))?,
//!     Duration::from_secs(30),
//! )?;
//! let install = follow.to_request(&station)?;
//! let update = follow.update_setpoint(Setpoint::discharge(w(2400))?)?;
//! assert_eq!(update.charging_profile_id, install.charging_profile.id);
//! assert_eq!(update.schedule_update.setpoint, Some(w(-2400)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use super::{ChargingPlan, EvNeeds, RateUnit, StationLimits, Target};
use crate::types::{DateTime, Decimal};
use crate::v2_1;

/// A V2X operation mode this module models (block Q §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Mode {
    /// Charging only — the default, and the mode before V2X is authorized (Q02).
    ChargingOnly,
    /// The CSMS sets the setpoint (Q03, Q04).
    CentralSetpoint,
    /// An external system (an EMS) sets the setpoint, within limits the CSMS may set (Q05).
    ExternalSetpoint,
    /// An external system sets the limits (Q06).
    ExternalLimits,
    /// The station balances against its grid meter's thresholds (Q09).
    LocalLoadBalancing,
    /// The EV is idle (Q10).
    Idle,
}

impl Mode {
    const ALL: [Self; 6] = [
        Self::ChargingOnly,
        Self::CentralSetpoint,
        Self::ExternalSetpoint,
        Self::ExternalLimits,
        Self::LocalLoadBalancing,
        Self::Idle,
    ];

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChargingOnly => "ChargingOnly",
            Self::CentralSetpoint => "CentralSetpoint",
            Self::ExternalSetpoint => "ExternalSetpoint",
            Self::ExternalLimits => "ExternalLimits",
            Self::LocalLoadBalancing => "LocalLoadBalancing",
            Self::Idle => "Idle",
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }

    const fn to_wire(self) -> v2_1::OperationMode {
        match self {
            Self::ChargingOnly => v2_1::OperationMode::ChargingOnly,
            Self::CentralSetpoint => v2_1::OperationMode::CentralSetpoint,
            Self::ExternalSetpoint => v2_1::OperationMode::ExternalSetpoint,
            Self::ExternalLimits => v2_1::OperationMode::ExternalLimits,
            Self::LocalLoadBalancing => v2_1::OperationMode::LocalLoadBalancing,
            Self::Idle => v2_1::OperationMode::Idle,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The operation modes a station supports — `V2XChargingCtrlr.SupportedOperationModes`
/// (Q01.FR.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationModes(u8);

impl OperationModes {
    /// What every V2X station supports (Q01.FR.31), as far as this module models it:
    /// `ChargingOnly` and `CentralSetpoint`. Use it when the station reports nothing.
    pub const MANDATORY: Self = Self(Mode::ChargingOnly.bit() | Mode::CentralSetpoint.bit());

    /// Reads the variable's `MemberList`, e.g. `"ChargingOnly,CentralSetpoint,LocalLoadBalancing"`.
    /// Modes this module does not model (the frequency modes) are ignored.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let mut bits = 0;
        for item in value.split(',').map(str::trim) {
            if let Some(mode) = Mode::ALL.iter().find(|mode| mode.as_str() == item) {
                bits |= mode.bit();
            }
        }
        Self(bits)
    }

    /// Whether `mode` is supported.
    #[must_use]
    pub const fn contains(self, mode: Mode) -> bool {
        self.0 & mode.bit() != 0
    }
}

/// What a V2X station allows: its smart-charging limits and its operation modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2xStation {
    /// `PeriodsPerSchedule`, `ProfileStackLevel`, `RateUnit`.
    pub limits: StationLimits,
    /// `V2XChargingCtrlr.SupportedOperationModes`.
    pub modes: OperationModes,
}

impl V2xStation {
    /// A station with these limits and modes.
    #[must_use]
    pub const fn new(limits: StationLimits, modes: OperationModes) -> Self {
        Self { limits, modes }
    }
}

/// A target rate the EV should follow as closely as it can (V2X.02).
///
/// Built from a magnitude and a direction, so a discharge can never be sent with the wrong sign.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Setpoint(Decimal);

impl Setpoint {
    /// Neither charging nor discharging.
    pub const ZERO: Self = Self(Decimal::ZERO);

    /// Charge at `rate` (in the schedule's unit).
    ///
    /// # Errors
    ///
    /// [`V2xError::NegativeMagnitude`] for a negative `rate`.
    pub fn charge(rate: Decimal) -> Result<Self, V2xError> {
        magnitude(rate).map(Self)
    }

    /// Discharge at `rate` — a magnitude; the wire gets it negative.
    ///
    /// # Errors
    ///
    /// [`V2xError::NegativeMagnitude`] for a negative `rate`.
    pub fn discharge(rate: Decimal) -> Result<Self, V2xError> {
        magnitude(rate)
            .and_then(|rate| rate.checked_neg().ok_or(V2xError::NegativeMagnitude))
            .map(Self)
    }

    /// The signed value block Q uses: positive charges, negative discharges.
    #[must_use]
    pub const fn signed(self) -> Decimal {
        self.0
    }
}

fn magnitude(value: Decimal) -> Result<Decimal, V2xError> {
    if value.signum() < 0 {
        Err(V2xError::NegativeMagnitude)
    } else {
        Ok(value)
    }
}

/// One schedule period's V2X content: a mode, and exactly the fields that mode allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation {
    mode: Mode,
    setpoint: Option<Setpoint>,
    limit: Option<Decimal>,
    /// Signed, ≤ 0, as on the wire.
    discharge_limit: Option<Decimal>,
    evse_sleep: bool,
    preconditioning: bool,
}

impl Operation {
    const fn bare(mode: Mode) -> Self {
        Self {
            mode,
            setpoint: None,
            limit: None,
            discharge_limit: None,
            evse_sleep: false,
            preconditioning: false,
        }
    }

    /// Charging only, up to `limit` — the only mode in which `limit` is required.
    ///
    /// # Errors
    ///
    /// [`V2xError::NegativeMagnitude`] for a negative `limit`.
    pub fn charging_only(limit: Decimal) -> Result<Self, V2xError> {
        Ok(Self {
            limit: Some(magnitude(limit)?),
            ..Self::bare(Mode::ChargingOnly)
        })
    }

    /// The CSMS sets the setpoint (Q03.FR.02). Bound it with
    /// [`charge_at_most`](Self::charge_at_most) and [`discharge_at_most`](Self::discharge_at_most).
    #[must_use]
    pub const fn central_setpoint(setpoint: Setpoint) -> Self {
        Self {
            setpoint: Some(setpoint),
            ..Self::bare(Mode::CentralSetpoint)
        }
    }

    /// An external system sets the setpoint; the CSMS may bound it.
    #[must_use]
    pub const fn external_setpoint() -> Self {
        Self::bare(Mode::ExternalSetpoint)
    }

    /// An external system sets the limits; the CSMS sets nothing.
    #[must_use]
    pub const fn external_limits() -> Self {
        Self::bare(Mode::ExternalLimits)
    }

    /// The station balances against its grid meter (Q09); the CSMS may bound the result
    /// (Q09.FR.05). Its thresholds are device-model variables (`V2XLocalLoadBalancing`), set
    /// separately — the station rejects this mode without them (Q09.FR.02).
    #[must_use]
    pub const fn local_load_balancing() -> Self {
        Self::bare(Mode::LocalLoadBalancing)
    }

    /// The EV is idle; no limit or setpoint may be set (Q10.FR.01).
    #[must_use]
    pub const fn idle() -> Self {
        Self::bare(Mode::Idle)
    }

    /// Caps charging at `rate` (V2X.03).
    ///
    /// # Errors
    ///
    /// [`V2xError::NotInMode`] for `ChargingOnly` (its limit is already set), `ExternalLimits`
    /// and `Idle`; [`V2xError::SetpointOutsideLimits`] when the setpoint would exceed it (V2X.05);
    /// [`V2xError::NegativeMagnitude`].
    pub fn charge_at_most(mut self, rate: Decimal) -> Result<Self, V2xError> {
        self.allow(
            "limit",
            &[
                Mode::CentralSetpoint,
                Mode::ExternalSetpoint,
                Mode::LocalLoadBalancing,
            ],
        )?;
        self.limit = Some(magnitude(rate)?);
        self.check_range()
    }

    /// Caps discharging at `rate`, a magnitude; the wire gets `dischargeLimit = -rate`
    /// (V2X.04).
    ///
    /// # Errors
    ///
    /// As [`charge_at_most`](Self::charge_at_most).
    pub fn discharge_at_most(mut self, rate: Decimal) -> Result<Self, V2xError> {
        self.allow(
            "dischargeLimit",
            &[
                Mode::CentralSetpoint,
                Mode::ExternalSetpoint,
                Mode::LocalLoadBalancing,
            ],
        )?;
        self.discharge_limit = Some(
            magnitude(rate)?
                .checked_neg()
                .ok_or(V2xError::NegativeMagnitude)?,
        );
        self.check_range()
    }

    /// Lets the EVSE switch its electronics off while idle (Q10.FR.03).
    ///
    /// # Errors
    ///
    /// [`V2xError::NotInMode`] unless the mode is `Idle`.
    pub fn sleep_evse(mut self) -> Result<Self, V2xError> {
        self.allow("evseSleep", &[Mode::Idle])?;
        self.evse_sleep = true;
        Ok(self)
    }

    /// Asks the EV to stay on standby rather than sleep, ready to react quickly (Q §2.2 Idle).
    ///
    /// # Errors
    ///
    /// [`V2xError::NotInMode`] unless the mode is `Idle`.
    pub fn precondition(mut self) -> Result<Self, V2xError> {
        self.allow("preconditioningRequest", &[Mode::Idle])?;
        self.preconditioning = true;
        Ok(self)
    }

    /// The same operation with another setpoint, checked against its limits.
    ///
    /// # Errors
    ///
    /// [`V2xError::NotInMode`] unless the mode is `CentralSetpoint`;
    /// [`V2xError::SetpointOutsideLimits`].
    pub fn with_setpoint(mut self, setpoint: Setpoint) -> Result<Self, V2xError> {
        self.allow("setpoint", &[Mode::CentralSetpoint])?;
        self.setpoint = Some(setpoint);
        self.check_range()
    }

    /// The mode.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// The setpoint, signed as on the wire.
    #[must_use]
    pub fn setpoint(&self) -> Option<Decimal> {
        self.setpoint.map(Setpoint::signed)
    }

    /// The charge limit.
    #[must_use]
    pub const fn limit(&self) -> Option<Decimal> {
        self.limit
    }

    /// The discharge limit, signed (≤ 0) as on the wire.
    #[must_use]
    pub const fn discharge_limit(&self) -> Option<Decimal> {
        self.discharge_limit
    }

    fn allow(&self, field: &'static str, modes: &[Mode]) -> Result<(), V2xError> {
        if modes.contains(&self.mode) {
            Ok(())
        } else {
            Err(V2xError::NotInMode {
                field,
                mode: self.mode,
            })
        }
    }

    /// V2X.05: the setpoint lies between `dischargeLimit` and `limit`.
    fn check_range(self) -> Result<Self, V2xError> {
        if let Some(setpoint) = self.setpoint() {
            let above = self.limit.is_some_and(|limit| setpoint > limit);
            let below = self.discharge_limit.is_some_and(|floor| setpoint < floor);
            if above || below {
                return Err(V2xError::SetpointOutsideLimits);
            }
        }
        Ok(self)
    }

    fn to_period(self, start: i32) -> v2_1::ChargingSchedulePeriod {
        let mut period = v2_1::ChargingSchedulePeriod::new(start);
        period.operation_mode = Some(self.mode.to_wire());
        period.setpoint = self.setpoint();
        period.limit = self.limit;
        period.discharge_limit = self.discharge_limit;
        period.evse_sleep = self.evse_sleep.then_some(true);
        period.preconditioning_request = self.preconditioning.then_some(true);
        period
    }

    fn to_update(self) -> v2_1::ChargingScheduleUpdate {
        let mut update = v2_1::ChargingScheduleUpdate::new();
        update.setpoint = self.setpoint();
        update.limit = self.limit;
        update.discharge_limit = self.discharge_limit;
        update
    }
}

/// Why a V2X operation, plan or update was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum V2xError {
    /// A rate is a magnitude; the direction is in the method's name.
    NegativeMagnitude,
    /// The setpoint lies outside `[dischargeLimit, limit]` (V2X.05).
    SetpointOutsideLimits,
    /// The field is not one this mode allows (block Q §2.2; Q10.FR.01 for `Idle`).
    NotInMode {
        /// The wire name of the field.
        field: &'static str,
        /// The mode.
        mode: Mode,
    },
    /// The station does not list this mode in `SupportedOperationModes` (Q09.FR.01).
    UnsupportedMode(Mode),
    /// More distinct periods than `PeriodsPerSchedule`. Setpoints are not merged.
    TooManyPeriods {
        /// How many the plan needs.
        periods: usize,
        /// How many the station allows.
        max: usize,
    },
    /// The stack level is above `ProfileStackLevel`.
    StackLevel {
        /// Requested.
        requested: i32,
        /// Allowed.
        max: i32,
    },
    /// A `TxProfile` on EVSE 0 (K01.FR.16).
    TxProfileOnEvseZero,
    /// The station does not accept this `chargingRateUnit` (K01.FR.26).
    UnitNotAllowed(RateUnit),
    /// A plan needs at least one slot.
    Empty,
    /// More than [`ChargingPlan::MAX_SLOTS`] slots.
    TooManySlots(usize),
    /// A slot or fallback shorter than a second, or a schedule longer than `i32::MAX` seconds.
    BadDuration,
    /// A dynamic update must keep the profile's mode: an update carries no `operationMode`.
    ModeChanged {
        /// The profile's mode.
        from: Mode,
        /// The update's.
        to: Mode,
    },
    /// A number that does not fit the wire's integer.
    OutOfRange(&'static str),
}

impl fmt::Display for V2xError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeMagnitude => {
                f.write_str("a rate is a magnitude; say charge or discharge instead of a sign")
            }
            Self::SetpointOutsideLimits => {
                f.write_str("the setpoint lies outside [dischargeLimit, limit] (V2X.05)")
            }
            Self::NotInMode { field, mode } => write!(f, "{mode} does not take {field}"),
            Self::UnsupportedMode(mode) => {
                write!(f, "the station does not support operation mode {mode}")
            }
            Self::TooManyPeriods { periods, max } => write!(
                f,
                "{periods} periods, the station allows {max}; setpoints are not merged"
            ),
            Self::StackLevel { requested, max } => write!(
                f,
                "stack level {requested} is above the station's ProfileStackLevel {max}"
            ),
            Self::TxProfileOnEvseZero => f.write_str("a TxProfile needs an EVSE above 0"),
            Self::UnitNotAllowed(unit) => write!(f, "the station does not accept {unit}"),
            Self::Empty => f.write_str("a V2X plan needs at least one slot"),
            Self::TooManySlots(n) => write!(f, "{n} slots is more than a plan may hold"),
            Self::BadDuration => f.write_str("a duration is outside 1 s ..= i32::MAX s"),
            Self::ModeChanged { from, to } => write!(
                f,
                "an update cannot change {from} into {to}; send a new profile"
            ),
            Self::OutOfRange(field) => write!(f, "{field} does not fit the wire's integer"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for V2xError {}

fn seconds(duration: Duration) -> Result<i32, V2xError> {
    i32::try_from(duration.as_secs())
        .ok()
        .filter(|seconds| *seconds >= 1)
        .ok_or(V2xError::BadDuration)
}

/// The checks every V2X profile shares: unit, stack level, target.
fn profile(
    id: i32,
    stack_level: i32,
    target: Target,
    kind: v2_1::ChargingProfileKind,
    schedule: v2_1::ChargingSchedule,
    station: &V2xStation,
) -> Result<v2_1::SetChargingProfileRequest, V2xError> {
    if stack_level > station.limits.max_stack_level {
        return Err(V2xError::StackLevel {
            requested: stack_level,
            max: station.limits.max_stack_level,
        });
    }
    let unit = match schedule.charging_rate_unit {
        v2_1::ChargingRateUnit::A => RateUnit::A,
        _ => RateUnit::W,
    };
    let allowed = match unit {
        RateUnit::A => station.limits.units.amperes,
        RateUnit::W => station.limits.units.watts,
    };
    if !allowed {
        return Err(V2xError::UnitNotAllowed(unit));
    }
    let (purpose, evse, transaction) = match target {
        Target::Transaction {
            evse_id,
            transaction_id,
        } => {
            if evse_id == 0 {
                return Err(V2xError::TxProfileOnEvseZero);
            }
            (
                v2_1::ChargingProfilePurpose::TxProfile,
                evse_id,
                Some(transaction_id),
            )
        }
        Target::EvseDefault { evse_id } => (
            v2_1::ChargingProfilePurpose::TxDefaultProfile,
            evse_id,
            None,
        ),
    };
    let mut wire =
        v2_1::ChargingProfile::new(id, stack_level, purpose, kind, alloc::vec![schedule]);
    wire.transaction_id = transaction;
    let evse = i32::try_from(evse).map_err(|_| V2xError::OutOfRange("evseId"))?;
    Ok(v2_1::SetChargingProfileRequest::new(evse, wire))
}

const fn wire_unit(unit: RateUnit) -> v2_1::ChargingRateUnit {
    match unit {
        RateUnit::A => v2_1::ChargingRateUnit::A,
        RateUnit::W => v2_1::ChargingRateUnit::W,
    }
}

/// A V2X plan: one [`Operation`] per equally long slot from a start instant.
#[derive(Clone, Debug, PartialEq)]
pub struct V2xPlan {
    start: DateTime,
    slot: i32,
    operations: Vec<Operation>,
}

impl V2xPlan {
    /// A plan of `operations`, one per `slot`, from `start`.
    ///
    /// # Errors
    ///
    /// [`V2xError::Empty`], [`V2xError::TooManySlots`] (above [`ChargingPlan::MAX_SLOTS`]),
    /// [`V2xError::BadDuration`].
    pub fn new(
        start: DateTime,
        slot: Duration,
        operations: impl IntoIterator<Item = Operation>,
    ) -> Result<Self, V2xError> {
        let operations: Vec<Operation> = operations.into_iter().collect();
        if operations.is_empty() {
            return Err(V2xError::Empty);
        }
        if operations.len() > ChargingPlan::MAX_SLOTS {
            return Err(V2xError::TooManySlots(operations.len()));
        }
        let slot = seconds(slot)?;
        let slots = i32::try_from(operations.len()).map_err(|_| V2xError::BadDuration)?;
        slot.checked_mul(slots).ok_or(V2xError::BadDuration)?;
        Ok(Self {
            start,
            slot,
            operations,
        })
    }

    /// The operations, in order.
    #[must_use]
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    /// The `SetChargingProfileRequest` for this plan: one `Absolute` profile in `unit`, equal
    /// neighbours merged, checked against everything the station said about itself.
    ///
    /// # Errors
    ///
    /// [`V2xError::UnsupportedMode`], [`V2xError::TooManyPeriods`],
    /// [`V2xError::StackLevel`], [`V2xError::UnitNotAllowed`], [`V2xError::TxProfileOnEvseZero`].
    pub fn to_request(
        &self,
        id: i32,
        stack_level: i32,
        target: Target,
        unit: RateUnit,
        station: &V2xStation,
    ) -> Result<v2_1::SetChargingProfileRequest, V2xError> {
        if let Some(op) = self
            .operations
            .iter()
            .find(|op| !station.modes.contains(op.mode))
        {
            return Err(V2xError::UnsupportedMode(op.mode));
        }
        let mut periods: Vec<v2_1::ChargingSchedulePeriod> = Vec::new();
        let mut previous: Option<&Operation> = None;
        for (index, operation) in self.operations.iter().enumerate() {
            if previous == Some(operation) {
                continue;
            }
            previous = Some(operation);
            // `new` checked that slot × len fits an i32.
            let start = self.slot * i32::try_from(index).unwrap_or(i32::MAX);
            periods.push(operation.to_period(start));
        }
        if periods.len() > station.limits.max_periods {
            return Err(V2xError::TooManyPeriods {
                periods: periods.len(),
                max: station.limits.max_periods,
            });
        }
        let mut schedule = v2_1::ChargingSchedule::new(id, wire_unit(unit), periods);
        schedule.start_schedule = Some(self.start);
        schedule.duration =
            Some(self.slot * i32::try_from(self.operations.len()).unwrap_or(i32::MAX));
        profile(
            id,
            stack_level,
            target,
            v2_1::ChargingProfileKind::Absolute,
            schedule,
            station,
        )
    }
}

/// A `Dynamic` profile whose one period a controller moves with updates (Q04, K28).
///
/// Built with a fallback: when no update arrives within it, the station drops the profile and
/// falls back to the next valid one (K28.FR.13). There is no way to build one without.
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicSetpoint {
    id: i32,
    stack_level: i32,
    target: Target,
    unit: RateUnit,
    operation: Operation,
    fallback: i32,
    pull_every: Option<i32>,
}

impl DynamicSetpoint {
    /// A dynamic profile `id` for `target`, starting with `operation`, falling back when not
    /// updated for `fallback_after`. Stack level 0; see [`stack_level`](Self::stack_level).
    ///
    /// # Errors
    ///
    /// [`V2xError::BadDuration`] for a fallback under a second or above `i32::MAX` seconds.
    pub fn new(
        id: i32,
        target: Target,
        unit: RateUnit,
        operation: Operation,
        fallback_after: Duration,
    ) -> Result<Self, V2xError> {
        Ok(Self {
            id,
            stack_level: 0,
            target,
            unit,
            operation,
            fallback: seconds(fallback_after)?,
            pull_every: None,
        })
    }

    /// Sets the stack level — above a plan's, so the dynamic profile leads while it lives.
    #[must_use]
    pub const fn stack_level(mut self, stack_level: i32) -> Self {
        self.stack_level = stack_level;
        self
    }

    /// Lets the station pull updates (`PullDynamicScheduleUpdateRequest`) every `interval`
    /// (`dynUpdateInterval`, K28.FR.10); answer with [`pull_response`](Self::pull_response).
    ///
    /// # Errors
    ///
    /// [`V2xError::BadDuration`].
    pub fn pull_every(mut self, interval: Duration) -> Result<Self, V2xError> {
        self.pull_every = Some(seconds(interval)?);
        Ok(self)
    }

    /// The operation currently in force.
    #[must_use]
    pub const fn operation(&self) -> &Operation {
        &self.operation
    }

    /// The profile id updates are addressed to.
    #[must_use]
    pub const fn id(&self) -> i32 {
        self.id
    }

    /// The `SetChargingProfileRequest` installing it: `Dynamic`, one schedule, one period at 0
    /// (K28.FR.01/02), `duration` = the fallback.
    ///
    /// # Errors
    ///
    /// As [`V2xPlan::to_request`].
    pub fn to_request(
        &self,
        station: &V2xStation,
    ) -> Result<v2_1::SetChargingProfileRequest, V2xError> {
        if !station.modes.contains(self.operation.mode) {
            return Err(V2xError::UnsupportedMode(self.operation.mode));
        }
        let mut schedule = v2_1::ChargingSchedule::new(
            self.id,
            wire_unit(self.unit),
            alloc::vec![self.operation.to_period(0)],
        );
        schedule.duration = Some(self.fallback);
        let mut request = profile(
            self.id,
            self.stack_level,
            self.target.clone(),
            v2_1::ChargingProfileKind::Dynamic,
            schedule,
            station,
        )?;
        request.charging_profile.dyn_update_interval = self.pull_every;
        Ok(request)
    }

    /// Replaces the operation, keeping its mode, and returns the update to send (K28.FR.06).
    ///
    /// # Errors
    ///
    /// [`V2xError::ModeChanged`]: an update carries no `operationMode`, so changing the mode
    /// needs a new profile.
    pub fn update(
        &mut self,
        next: Operation,
    ) -> Result<v2_1::UpdateDynamicScheduleRequest, V2xError> {
        if next.mode != self.operation.mode {
            return Err(V2xError::ModeChanged {
                from: self.operation.mode,
                to: next.mode,
            });
        }
        self.operation = next;
        Ok(v2_1::UpdateDynamicScheduleRequest::new(
            self.id,
            next.to_update(),
        ))
    }

    /// Moves the setpoint within the current limits and returns the update to send.
    ///
    /// # Errors
    ///
    /// [`V2xError::SetpointOutsideLimits`], or [`V2xError::NotInMode`] unless `CentralSetpoint`.
    pub fn update_setpoint(
        &mut self,
        setpoint: Setpoint,
    ) -> Result<v2_1::UpdateDynamicScheduleRequest, V2xError> {
        let next = self.operation.with_setpoint(setpoint)?;
        self.update(next)
    }

    /// The answer to the station's `PullDynamicScheduleUpdateRequest` (K28.FR.07): the operation
    /// currently in force.
    #[must_use]
    pub fn pull_response(&self) -> v2_1::PullDynamicScheduleUpdateResponse {
        let mut response =
            v2_1::PullDynamicScheduleUpdateResponse::new(v2_1::ChargingProfileStatus::Accepted);
        response.schedule_update = Some(self.operation.to_update());
        response
    }
}

impl EvNeeds {
    /// `setpoint` (in W) clamped to what the EV can do: no faster than
    /// [`max_charge`](EvNeeds::max_charge), no faster than
    /// [`max_discharge`](EvNeeds::max_discharge), and no discharge at all unless the transfer is
    /// bidirectional.
    #[must_use]
    pub fn clamp_watts(&self, setpoint: Setpoint) -> Setpoint {
        let value = setpoint.signed();
        if value.signum() >= 0 {
            Setpoint(self.max_charge.map_or(value, |max| value.min(max)))
        } else if !self.bidirectional {
            Setpoint::ZERO
        } else {
            let floor = self.max_discharge.and_then(Decimal::checked_neg);
            Setpoint(floor.map_or(value, |floor| value.max(floor)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_charging::RateUnits;

    fn w(value: i64) -> Decimal {
        Decimal::from(value)
    }

    fn station(modes: &str) -> V2xStation {
        V2xStation::new(
            StationLimits::new(10, 3, RateUnits::BOTH),
            OperationModes::parse(modes),
        )
    }

    fn start() -> DateTime {
        "2026-11-20T00:00:00Z".parse().unwrap()
    }

    const ALL: &str =
        "ChargingOnly,CentralSetpoint,ExternalSetpoint,ExternalLimits,LocalLoadBalancing,Idle";

    #[test]
    fn signs_reach_the_wire_as_block_q_wants_them() {
        let op = Operation::central_setpoint(Setpoint::discharge(w(5000)).unwrap())
            .discharge_at_most(w(6000))
            .unwrap()
            .charge_at_most(w(11000))
            .unwrap();
        let period = op.to_period(0);
        assert_eq!(period.setpoint, Some(w(-5000)));
        assert_eq!(period.discharge_limit, Some(w(-6000)));
        assert_eq!(period.limit, Some(w(11000)));
        assert_eq!(
            period.operation_mode,
            Some(v2_1::OperationMode::CentralSetpoint)
        );
        assert_eq!(Setpoint::charge(w(3700)).unwrap().signed(), w(3700));
        assert_eq!(Setpoint::discharge(w(-1)), Err(V2xError::NegativeMagnitude));
        assert_eq!(
            Operation::central_setpoint(Setpoint::ZERO).discharge_at_most(w(-5)),
            Err(V2xError::NegativeMagnitude)
        );
        assert_eq!(Setpoint::charge(Decimal::ZERO).unwrap(), Setpoint::ZERO);
    }

    #[test]
    fn a_setpoint_outside_its_limits_is_refused_v2x_05() {
        let discharge = Operation::central_setpoint(Setpoint::discharge(w(7000)).unwrap());
        assert_eq!(
            discharge.discharge_at_most(w(6000)),
            Err(V2xError::SetpointOutsideLimits)
        );
        let charge = Operation::central_setpoint(Setpoint::charge(w(1)).unwrap());
        assert_eq!(
            charge.charge_at_most(Decimal::ZERO),
            Err(V2xError::SetpointOutsideLimits)
        );
        let bounded = Operation::central_setpoint(Setpoint::ZERO)
            .charge_at_most(w(1000))
            .unwrap();
        assert_eq!(
            bounded.with_setpoint(Setpoint::charge(w(2000)).unwrap()),
            Err(V2xError::SetpointOutsideLimits)
        );
    }

    #[test]
    fn each_mode_takes_only_its_fields() {
        let idle = Operation::idle()
            .sleep_evse()
            .unwrap()
            .precondition()
            .unwrap();
        let period = idle.to_period(0);
        assert_eq!(
            (
                period.limit,
                period.discharge_limit,
                period.setpoint,
                period.setpoint_reactive
            ),
            (None, None, None, None),
            "Q10.FR.01"
        );
        assert_eq!(
            (period.evse_sleep, period.preconditioning_request),
            (Some(true), Some(true))
        );
        assert!(matches!(
            Operation::idle().charge_at_most(w(1)),
            Err(V2xError::NotInMode { field: "limit", .. })
        ));
        assert!(matches!(
            Operation::external_limits().discharge_at_most(w(1)),
            Err(V2xError::NotInMode { .. })
        ));
        assert!(matches!(
            Operation::charging_only(w(1)).unwrap().sleep_evse(),
            Err(V2xError::NotInMode {
                field: "evseSleep",
                ..
            })
        ));
        assert!(matches!(
            Operation::local_load_balancing().with_setpoint(Setpoint::ZERO),
            Err(V2xError::NotInMode {
                field: "setpoint",
                ..
            })
        ));
        assert!(
            Operation::local_load_balancing()
                .charge_at_most(w(9000))
                .is_ok(),
            "Q09.FR.05"
        );
        let charging = Operation::charging_only(w(11000)).unwrap().to_period(0);
        assert_eq!((charging.limit, charging.setpoint), (Some(w(11000)), None));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_plan_merges_neighbours_and_honours_the_station() {
        let charge = Operation::charging_only(w(11000)).unwrap();
        let plan = V2xPlan::new(
            start(),
            Duration::from_secs(900),
            [charge, charge, Operation::idle(), charge],
        )
        .unwrap();
        let request = plan
            .to_request(
                1,
                1,
                Target::transaction(1, "tx"),
                RateUnit::W,
                &station(ALL),
            )
            .unwrap();
        let profile = &request.charging_profile;
        let starts: Vec<i32> = profile.charging_schedule[0]
            .charging_schedule_period
            .iter()
            .map(|p| p.start_period)
            .collect();
        assert_eq!(starts, [0, 1800, 2700]);
        assert_eq!(profile.charging_schedule[0].duration, Some(3600));
        assert_eq!(
            profile.charging_profile_kind,
            v2_1::ChargingProfileKind::Absolute
        );
        assert_eq!(profile.transaction_id.as_deref(), Some("tx"));
        assert_eq!(request.evse_id, 1);

        let llb = V2xPlan::new(
            start(),
            Duration::from_secs(900),
            [Operation::local_load_balancing()],
        )
        .unwrap();
        assert_eq!(
            llb.to_request(
                1,
                1,
                Target::transaction(1, "tx"),
                RateUnit::W,
                &station("ChargingOnly,CentralSetpoint")
            ),
            Err(V2xError::UnsupportedMode(Mode::LocalLoadBalancing))
        );
        let many = V2xPlan::new(
            start(),
            Duration::from_secs(60),
            (0..11).map(|n| Operation::charging_only(w(1000 + n)).unwrap()),
        )
        .unwrap();
        assert_eq!(
            many.to_request(
                1,
                1,
                Target::transaction(1, "tx"),
                RateUnit::W,
                &station(ALL)
            ),
            Err(V2xError::TooManyPeriods {
                periods: 11,
                max: 10
            })
        );
        let one = V2xPlan::new(start(), Duration::from_secs(60), [charge]).unwrap();
        assert_eq!(
            one.to_request(
                1,
                4,
                Target::transaction(1, "tx"),
                RateUnit::W,
                &station(ALL)
            ),
            Err(V2xError::StackLevel {
                requested: 4,
                max: 3
            })
        );
        let watts_only = V2xStation::new(
            StationLimits::new(10, 3, RateUnits::parse_v2("W")),
            OperationModes::MANDATORY,
        );
        assert_eq!(
            one.to_request(1, 1, Target::transaction(1, "tx"), RateUnit::A, &watts_only),
            Err(V2xError::UnitNotAllowed(RateUnit::A))
        );
        assert_eq!(
            one.to_request(
                1,
                1,
                Target::transaction(0, "tx"),
                RateUnit::W,
                &station(ALL)
            ),
            Err(V2xError::TxProfileOnEvseZero)
        );
        assert_eq!(
            V2xPlan::new(start(), Duration::from_secs(60), []),
            Err(V2xError::Empty)
        );
    }

    #[test]
    fn operation_modes_read_the_member_list() {
        let modes = OperationModes::parse(
            "ChargingOnly, CentralSetpoint,CentralFrequency,LocalLoadBalancing",
        );
        assert!(modes.contains(Mode::LocalLoadBalancing));
        assert!(!modes.contains(Mode::Idle));
        assert!(OperationModes::MANDATORY.contains(Mode::CentralSetpoint));
        assert!(!OperationModes::MANDATORY.contains(Mode::Idle));
    }

    #[test]
    fn a_dynamic_setpoint_has_a_fallback_and_one_period() {
        let mut follow = DynamicSetpoint::new(
            10,
            Target::transaction(1, "tx"),
            RateUnit::W,
            Operation::central_setpoint(Setpoint::ZERO)
                .charge_at_most(w(11000))
                .unwrap(),
            Duration::from_secs(30),
        )
        .unwrap()
        .stack_level(2)
        .pull_every(Duration::from_secs(60))
        .unwrap();
        let request = follow.to_request(&station(ALL)).unwrap();
        let profile = &request.charging_profile;
        assert_eq!(
            profile.charging_profile_kind,
            v2_1::ChargingProfileKind::Dynamic
        );
        assert_eq!(profile.charging_schedule.len(), 1, "K28.FR.01");
        assert_eq!(
            profile.charging_schedule[0].charging_schedule_period.len(),
            1
        );
        assert_eq!(
            profile.charging_schedule[0].charging_schedule_period[0].start_period, 0,
            "K28.FR.02"
        );
        assert_eq!(profile.charging_schedule[0].duration, Some(30), "K28.FR.13");
        assert_eq!(profile.dyn_update_interval, Some(60));
        assert_eq!(profile.stack_level, 2);

        let update = follow
            .update_setpoint(Setpoint::charge(w(2000)).unwrap())
            .unwrap();
        assert_eq!(update.charging_profile_id, 10);
        assert_eq!(update.schedule_update.setpoint, Some(w(2000)));
        assert_eq!(update.schedule_update.limit, Some(w(11000)));
        assert_eq!(
            follow.update_setpoint(Setpoint::charge(w(12000)).unwrap()),
            Err(V2xError::SetpointOutsideLimits)
        );
        assert_eq!(
            follow.operation().setpoint(),
            Some(w(2000)),
            "a refused update changes nothing"
        );
        assert_eq!(
            follow.update(Operation::idle()),
            Err(V2xError::ModeChanged {
                from: Mode::CentralSetpoint,
                to: Mode::Idle
            })
        );
        let pull = follow.pull_response();
        assert_eq!(pull.status, v2_1::ChargingProfileStatus::Accepted);
        assert_eq!(pull.schedule_update.unwrap().setpoint, Some(w(2000)));

        assert_eq!(
            DynamicSetpoint::new(
                1,
                Target::transaction(1, "tx"),
                RateUnit::W,
                Operation::idle(),
                Duration::ZERO
            ),
            Err(V2xError::BadDuration)
        );
        let plain = DynamicSetpoint::new(
            1,
            Target::transaction(1, "tx"),
            RateUnit::W,
            Operation::idle(),
            Duration::from_secs(5),
        )
        .unwrap()
        .to_request(&station(ALL))
        .unwrap();
        assert_eq!(
            plain.charging_profile.dyn_update_interval, None,
            "K28.FR.04 only when asked"
        );
    }

    fn needs(json: &str) -> EvNeeds {
        EvNeeds::from_v2_1(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn a_setpoint_is_clamped_to_what_the_ev_can_do() {
        let ev = needs(
            r#"{"requestedEnergyTransfer":"AC_BPT","v2xChargingParameters":{
                "maxChargePower":11000,"maxDischargePower":2000,"maxDischargePower_L2":2000,
                "maxDischargePower_L3":2000}}"#,
        );
        assert_eq!(
            ev.clamp_watts(Setpoint::charge(w(22000)).unwrap()),
            Setpoint::charge(w(11000)).unwrap()
        );
        assert_eq!(
            ev.clamp_watts(Setpoint::discharge(w(9000)).unwrap()),
            Setpoint::discharge(w(6000)).unwrap()
        );

        let plain = needs(r#"{"requestedEnergyTransfer":"AC_three_phase"}"#);
        assert!(!plain.bidirectional);
        assert_eq!(
            plain.clamp_watts(Setpoint::discharge(w(1000)).unwrap()),
            Setpoint::ZERO
        );
    }
}
