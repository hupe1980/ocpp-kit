//! `Profile` across the wire. See [`ConversionError`].

use core::fmt;

#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
use alloc::{string::ToString, vec::Vec};

use super::Purpose;
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
use super::{Period, Profile, ProfileKind, RateUnit, Recurrency, Schedule};
use crate::version::Version;

/// Why a profile could not cross the wire.
///
/// The domain [`Profile`] across the wire, in both directions, for every version.
///
/// A CSMS sends a `Profile` as a `SetChargingProfileRequest`; a station — or a Local Controller
/// that wants to know what the CSMS asked for — turns a received `ChargingProfile` back into a
/// `Profile` for the [composite calculation](super::ProfileStore::composite). Each direction refuses, with a
/// [`ConversionError`], every field the other side cannot carry *with its meaning*: a default
/// here would be a profile the station applies differently from the one that was sent.
///
/// What is refused, and why:
///
/// * 1.6 numbers transactions; a domain transaction id that is not an integer cannot be a 1.6
///   `TxProfile` (it would bind to another transaction, or to none).
/// * Purposes a version lacks — 1.6 has no `ChargingStationExternalConstraints`, neither 1.6
///   nor 2.0.1 has `PriorityCharging` or `LocalGeneration`.
/// * 2.1 `Dynamic` profiles, periods without a `limit`, and everything that is not a plain
///   ceiling: `limit_L2`/`limit_L3` (which turn `limit` into a per-phase value), discharge
///   limits, setpoints, V2X curves and baselines, an `operationMode` other than
///   `ChargingOnly`, `limitAtSoC`, `useLocalTime`, `randomizedDelay`. Those are V2X control
///   (block Q), not a limit schedule — build those with [`v2x`](super::v2x), which produces
///   the wire profile directly.
/// * 1.6 `phaseToUse`, which 1.6 cannot say.
///
/// Carried by neither direction, on purpose, because none of them changes the limit: the
/// ISO 15118 tariff and price schedules and their signatures, `customData`, 2.1
/// `powerTolerance`, `evseSleep`, `preconditioningRequest`, and the 2.1 offline fields
/// (`maxOfflineDuration`, `invalidAfterOfflineDuration`) — the composite here is the one that
/// applies while online.
///
/// ```
/// use ocpp_kit::smart_charging::{Period, Profile, ProfileKind, Purpose, RateUnit, Schedule};
/// use ocpp_kit::types::Decimal;
///
/// let profile = Profile::new(
///     1,
///     0,
///     Purpose::TxProfile,
///     ProfileKind::Relative,
///     Schedule::new(1, RateUnit::A, vec![Period::new(0, Decimal::from(16))]),
/// )
/// .on_evse(1)
/// .for_transaction("42");
///
/// let request = profile.set_request_v1_6()?;          // 1.6 numbers its transactions
/// assert_eq!(request.cs_charging_profiles.transaction_id, Some(42));
/// let back = Profile::from_v1_6(&request.cs_charging_profiles, 1)?;
/// assert_eq!(back, profile);
///
/// let not_a_number = profile.clone().for_transaction("abc");
/// assert!(not_a_number.to_v1_6().is_err());
/// # Ok::<(), ocpp_kit::smart_charging::ConversionError>(())
/// ```

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConversionError {
    /// A 1.6 `TxProfile` needs an integer transaction id.
    TransactionIdNotInteger(alloc::string::String),
    /// The version has no such purpose.
    PurposeNotInVersion {
        /// The purpose.
        purpose: Purpose,
        /// The version.
        version: Version,
    },
    /// A profile kind the domain does not model (2.1 `Dynamic`).
    UnmodelledKind(&'static str),
    /// An enumeration value the station sent that no version defines, kept by lenient decoding.
    UnknownValue {
        /// The field.
        field: &'static str,
        /// What arrived.
        value: alloc::string::String,
    },
    /// A profile without a schedule.
    NoSchedule,
    /// 1.6 carries exactly one schedule per profile.
    SchedulesNotInVersion(usize),
    /// A 2.1 period without a `limit` — a setpoint operation mode, not a ceiling.
    PeriodWithoutLimit {
        /// The period's index.
        period: usize,
    },
    /// A field that would change what the profile means, which the target cannot carry.
    UnmodelledField {
        /// The wire name of the field.
        field: &'static str,
        /// The period's index, for a period field.
        period: Option<usize>,
    },
    /// A number that does not fit the target's integer type.
    OutOfRange {
        /// The field.
        field: &'static str,
    },
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `{:?}` quotes and escapes: these strings came from a peer or a caller.
            Self::TransactionIdNotInteger(id) => write!(
                f,
                "transaction id {id:?} is not an integer, which a 1.6 TxProfile needs"
            ),
            Self::PurposeNotInVersion { purpose, version } => {
                write!(f, "OCPP {version} has no {purpose:?} purpose")
            }
            Self::UnmodelledKind(kind) => write!(f, "{kind} profiles are not modelled"),
            Self::UnknownValue { field, value } => {
                write!(f, "{field} {value:?} is not a value any version defines")
            }
            Self::NoSchedule => f.write_str("the profile has no schedule"),
            Self::SchedulesNotInVersion(n) => {
                write!(f, "OCPP 1.6 carries one schedule per profile, not {n}")
            }
            Self::PeriodWithoutLimit { period } => {
                write!(
                    f,
                    "period {period} has no limit (a setpoint operation mode)"
                )
            }
            Self::UnmodelledField {
                field,
                period: Some(period),
            } => write!(
                f,
                "period {period} sets {field}, which is not a plain ceiling"
            ),
            Self::UnmodelledField {
                field,
                period: None,
            } => write!(f, "{field} is set, which is not a plain ceiling"),
            Self::OutOfRange { field } => write!(f, "{field} does not fit the wire's integer"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ConversionError {}

#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
fn to_i32(value: i64, field: &'static str) -> Result<i32, ConversionError> {
    i32::try_from(value).map_err(|_| ConversionError::OutOfRange { field })
}

/// Generates the purpose, kind, recurrency and unit mappings for one version's enums.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
macro_rules! enum_mappings {
    ($module:ident, $version:expr, { $($domain:ident <=> $wire:ident),* $(,)? }) => {
        fn purpose_to_wire(purpose: Purpose) -> Result<crate::$module::ChargingProfilePurpose, ConversionError> {
            match purpose {
                $(Purpose::$domain => Ok(crate::$module::ChargingProfilePurpose::$wire),)*
                #[allow(unreachable_patterns)]
                other => Err(ConversionError::PurposeNotInVersion { purpose: other, version: $version }),
            }
        }

        fn purpose_from_wire(purpose: &crate::$module::ChargingProfilePurpose) -> Result<Purpose, ConversionError> {
            match purpose {
                $(crate::$module::ChargingProfilePurpose::$wire => Ok(Purpose::$domain),)*
                other => Err(ConversionError::UnknownValue {
                    field: "chargingProfilePurpose",
                    value: other.as_str().to_string(),
                }),
            }
        }

        fn kind_to_wire(kind: ProfileKind) -> crate::$module::ChargingProfileKind {
            match kind {
                ProfileKind::Absolute => crate::$module::ChargingProfileKind::Absolute,
                ProfileKind::Recurring => crate::$module::ChargingProfileKind::Recurring,
                ProfileKind::Relative => crate::$module::ChargingProfileKind::Relative,
            }
        }

        fn kind_from_wire(kind: &crate::$module::ChargingProfileKind) -> Result<ProfileKind, ConversionError> {
            match kind {
                crate::$module::ChargingProfileKind::Absolute => Ok(ProfileKind::Absolute),
                crate::$module::ChargingProfileKind::Recurring => Ok(ProfileKind::Recurring),
                crate::$module::ChargingProfileKind::Relative => Ok(ProfileKind::Relative),
                #[allow(unreachable_patterns)]
                crate::$module::ChargingProfileKind::UnknownValue(value) => Err(ConversionError::UnknownValue {
                    field: "chargingProfileKind",
                    value: value.clone(),
                }),
                #[allow(unreachable_patterns)]
                other => Err(ConversionError::UnmodelledKind(kind_name(other.as_str()))),
            }
        }

        fn recurrency_to_wire(recurrency: Recurrency) -> crate::$module::RecurrencyKind {
            match recurrency {
                Recurrency::Daily => crate::$module::RecurrencyKind::Daily,
                Recurrency::Weekly => crate::$module::RecurrencyKind::Weekly,
            }
        }

        fn recurrency_from_wire(recurrency: &crate::$module::RecurrencyKind) -> Result<Recurrency, ConversionError> {
            match recurrency {
                crate::$module::RecurrencyKind::Daily => Ok(Recurrency::Daily),
                crate::$module::RecurrencyKind::Weekly => Ok(Recurrency::Weekly),
                other => Err(ConversionError::UnknownValue {
                    field: "recurrencyKind",
                    value: other.as_str().to_string(),
                }),
            }
        }

        fn unit_to_wire(unit: RateUnit) -> crate::$module::ChargingRateUnit {
            match unit {
                RateUnit::A => crate::$module::ChargingRateUnit::A,
                RateUnit::W => crate::$module::ChargingRateUnit::W,
            }
        }

        fn unit_from_wire(unit: &crate::$module::ChargingRateUnit) -> Result<RateUnit, ConversionError> {
            match unit {
                crate::$module::ChargingRateUnit::A => Ok(RateUnit::A),
                crate::$module::ChargingRateUnit::W => Ok(RateUnit::W),
                other => Err(ConversionError::UnknownValue {
                    field: "chargingRateUnit",
                    value: other.as_str().to_string(),
                }),
            }
        }
    };
}

/// The one kind name a newer version adds, as a `'static` for the error.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
fn kind_name(kind: &str) -> &'static str {
    if kind == "Dynamic" {
        "Dynamic"
    } else {
        "unknown"
    }
}

#[cfg(feature = "v1_6")]
mod v1_6_wire {
    use super::{
        ConversionError, Period, Profile, ProfileKind, Purpose, RateUnit, Recurrency, Schedule,
        ToString, Vec, Version, kind_name, to_i32,
    };
    use crate::v1_6;

    enum_mappings!(v1_6, Version::V1_6, {
        ChargingStationMaxProfile <=> ChargePointMaxProfile,
        TxDefaultProfile <=> TxDefaultProfile,
        TxProfile <=> TxProfile,
    });

    impl Profile {
        /// This profile as an OCPP 1.6 `ChargingProfile`.
        ///
        /// # Errors
        ///
        /// [`ConversionError`] for a transaction id that is not an integer, a purpose 1.6 lacks,
        /// anything but exactly one schedule, a `phaseToUse`, or a number out of range.
        pub fn to_v1_6(&self) -> Result<v1_6::ChargingProfile, ConversionError> {
            let schedule = match self.schedules.as_slice() {
                [] => return Err(ConversionError::NoSchedule),
                [one] => one,
                many => return Err(ConversionError::SchedulesNotInVersion(many.len())),
            };
            let mut periods = Vec::with_capacity(schedule.periods.len());
            for (index, period) in schedule.periods.iter().enumerate() {
                if period.phase_to_use.is_some() {
                    return Err(ConversionError::UnmodelledField {
                        field: "phaseToUse",
                        period: Some(index),
                    });
                }
                let mut wire = v1_6::ChargingSchedulePeriod::new(
                    to_i32(period.start_period, "startPeriod")?,
                    period.limit,
                );
                wire.number_phases = period.number_phases;
                periods.push(wire);
            }
            let mut wire_schedule =
                v1_6::ChargingSchedule::new(unit_to_wire(schedule.rate_unit), periods);
            wire_schedule.duration = schedule
                .duration
                .map(|d| to_i32(d, "duration"))
                .transpose()?;
            wire_schedule.start_schedule = schedule.start;
            wire_schedule.min_charging_rate = schedule.min_charging_rate;

            let mut profile = v1_6::ChargingProfile::new(
                self.id,
                self.stack_level,
                purpose_to_wire(self.purpose)?,
                kind_to_wire(self.kind),
                wire_schedule,
            );
            profile.transaction_id = self
                .transaction_id
                .as_ref()
                .map(|id| {
                    id.parse::<i32>()
                        .map_err(|_| ConversionError::TransactionIdNotInteger(id.clone()))
                })
                .transpose()?;
            profile.recurrency_kind = self.recurrency.map(recurrency_to_wire);
            profile.valid_from = self.valid_from;
            profile.valid_to = self.valid_to;
            Ok(profile)
        }

        /// This profile as an OCPP 1.6 `SetChargingProfile.req`, for connector
        /// [`evse_id`](Self::evse_id).
        ///
        /// # Errors
        ///
        /// As [`to_v1_6`](Self::to_v1_6), and for a connector id out of range.
        pub fn set_request_v1_6(&self) -> Result<v1_6::SetChargingProfileRequest, ConversionError> {
            let connector =
                i32::try_from(self.evse_id).map_err(|_| ConversionError::OutOfRange {
                    field: "connectorId",
                })?;
            Ok(v1_6::SetChargingProfileRequest::new(
                connector,
                self.to_v1_6()?,
            ))
        }

        /// A profile received in an OCPP 1.6 `SetChargingProfile.req` for `connector_id`.
        ///
        /// 1.6 schedules carry no id; the domain schedule takes the profile's.
        ///
        /// # Errors
        ///
        /// [`ConversionError`] for an enumeration value 1.6 does not define.
        pub fn from_v1_6(
            wire: &v1_6::ChargingProfile,
            connector_id: u32,
        ) -> Result<Self, ConversionError> {
            let schedule = &wire.charging_schedule;
            let periods = schedule
                .charging_schedule_period
                .iter()
                .map(|period| Period {
                    start_period: i64::from(period.start_period),
                    limit: period.limit,
                    number_phases: period.number_phases,
                    phase_to_use: None,
                })
                .collect();
            // 1.6 schedules have no id of their own; the profile's stands in, which is what a
            // plan converted by `ChargingPlan::to_profile` uses for both.
            let mut domain = Schedule::new(
                wire.charging_profile_id,
                unit_from_wire(&schedule.charging_rate_unit)?,
                periods,
            );
            domain.start = schedule.start_schedule;
            domain.duration = schedule.duration.map(i64::from);
            domain.min_charging_rate = schedule.min_charging_rate;
            Ok(Self {
                id: wire.charging_profile_id,
                stack_level: wire.stack_level,
                purpose: purpose_from_wire(&wire.charging_profile_purpose)?,
                kind: kind_from_wire(&wire.charging_profile_kind)?,
                recurrency: wire
                    .recurrency_kind
                    .as_ref()
                    .map(recurrency_from_wire)
                    .transpose()?,
                valid_from: wire.valid_from,
                valid_to: wire.valid_to,
                transaction_id: wire.transaction_id.map(|id| id.to_string()),
                evse_id: connector_id,
                schedules: alloc::vec![domain],
            })
        }
    }
}

/// The 2.x conversions, which share their shape and differ in their periods.
#[cfg(any(feature = "v2_0_1", feature = "v2_1"))]
macro_rules! wire_2x {
    (
        $module:ident, $version:expr, $to:ident, $set:ident, $from:ident,
        { $($domain:ident <=> $wire:ident),* $(,)? },
        period_to_wire: $period_to:expr,
        period_from_wire: $period_from:expr,
        schedule_check: $schedule_check:expr $(,)?
    ) => {
        use super::{
            ConversionError, Period, Profile, ProfileKind, Purpose, RateUnit, Recurrency, Schedule,
            ToString, Vec, Version, kind_name, to_i32,
        };
        use crate::$module;

        enum_mappings!($module, $version, { $($domain <=> $wire),* });

        impl Profile {
            #[doc = concat!("This profile as an OCPP ", stringify!($module), " `ChargingProfile`.")]
            ///
            /// # Errors
            ///
            /// [`ConversionError`] for a purpose the version lacks, no schedule, or a number out
            /// of range.
            pub fn $to(&self) -> Result<$module::ChargingProfile, ConversionError> {
                if self.schedules.is_empty() {
                    return Err(ConversionError::NoSchedule);
                }
                let mut schedules = Vec::with_capacity(self.schedules.len());
                for schedule in &self.schedules {
                    let periods = schedule
                        .periods
                        .iter()
                        .map($period_to)
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut wire =
                        $module::ChargingSchedule::new(schedule.id, unit_to_wire(schedule.rate_unit), periods);
                    wire.start_schedule = schedule.start;
                    wire.duration = schedule.duration.map(|d| to_i32(d, "duration")).transpose()?;
                    wire.min_charging_rate = schedule.min_charging_rate;
                    schedules.push(wire);
                }
                let mut profile = $module::ChargingProfile::new(
                    self.id,
                    self.stack_level,
                    purpose_to_wire(self.purpose)?,
                    kind_to_wire(self.kind),
                    schedules,
                );
                profile.transaction_id = self.transaction_id.clone();
                profile.recurrency_kind = self.recurrency.map(recurrency_to_wire);
                profile.valid_from = self.valid_from;
                profile.valid_to = self.valid_to;
                Ok(profile)
            }

            #[doc = concat!("This profile as an OCPP ", stringify!($module), " `SetChargingProfileRequest` for EVSE [`evse_id`](Self::evse_id).")]
            ///
            /// # Errors
            ///
            #[doc = concat!("As [`", stringify!($to), "`](Self::", stringify!($to), "), and for an EVSE id out of range.")]
            pub fn $set(&self) -> Result<$module::SetChargingProfileRequest, ConversionError> {
                let evse = i32::try_from(self.evse_id)
                    .map_err(|_| ConversionError::OutOfRange { field: "evseId" })?;
                Ok($module::SetChargingProfileRequest::new(evse, self.$to()?))
            }

            #[doc = concat!("A profile received in an OCPP ", stringify!($module), " `SetChargingProfileRequest` for `evse_id`.")]
            ///
            /// # Errors
            ///
            /// [`ConversionError`] for anything that is not a plain ceiling — see its
            /// documentation for the list.
            pub fn $from(wire: &$module::ChargingProfile, evse_id: u32) -> Result<Self, ConversionError> {
                let mut schedules = Vec::with_capacity(wire.charging_schedule.len());
                for schedule in &wire.charging_schedule {
                    $schedule_check(schedule)?;
                    let periods = schedule
                        .charging_schedule_period
                        .iter()
                        .enumerate()
                        .map($period_from)
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut domain =
                        Schedule::new(schedule.id, unit_from_wire(&schedule.charging_rate_unit)?, periods);
                    domain.start = schedule.start_schedule;
                    domain.duration = schedule.duration.map(i64::from);
                    domain.min_charging_rate = schedule.min_charging_rate;
                    schedules.push(domain);
                }
                if schedules.is_empty() {
                    return Err(ConversionError::NoSchedule);
                }
                Ok(Self {
                    id: wire.id,
                    stack_level: wire.stack_level,
                    purpose: purpose_from_wire(&wire.charging_profile_purpose)?,
                    kind: kind_from_wire(&wire.charging_profile_kind)?,
                    recurrency: wire.recurrency_kind.as_ref().map(recurrency_from_wire).transpose()?,
                    valid_from: wire.valid_from,
                    valid_to: wire.valid_to,
                    transaction_id: wire.transaction_id.clone(),
                    evse_id,
                    schedules,
                })
            }
        }
    };
}

#[cfg(feature = "v2_0_1")]
mod v2_0_1_wire {
    wire_2x!(
        v2_0_1, Version::V2_0_1, to_v2_0_1, set_request_v2_0_1, from_v2_0_1,
        {
            ChargingStationMaxProfile <=> ChargingStationMaxProfile,
            TxDefaultProfile <=> TxDefaultProfile,
            TxProfile <=> TxProfile,
            ChargingStationExternalConstraints <=> ChargingStationExternalConstraints,
        },
        period_to_wire: |period: &Period| -> Result<v2_0_1::ChargingSchedulePeriod, ConversionError> {
            let mut wire = v2_0_1::ChargingSchedulePeriod::new(
                to_i32(period.start_period, "startPeriod")?,
                period.limit,
            );
            wire.number_phases = period.number_phases;
            wire.phase_to_use = period.phase_to_use;
            Ok(wire)
        },
        period_from_wire: |(_, period): (usize, &v2_0_1::ChargingSchedulePeriod)| -> Result<Period, ConversionError> {
            Ok(Period {
                start_period: i64::from(period.start_period),
                limit: period.limit,
                number_phases: period.number_phases,
                phase_to_use: period.phase_to_use,
            })
        },
        schedule_check: |_: &v2_0_1::ChargingSchedule| -> Result<(), ConversionError> { Ok(()) },
    );
}

#[cfg(feature = "v2_1")]
mod v2_1_wire {
    wire_2x!(
        v2_1, Version::V2_1, to_v2_1, set_request_v2_1, from_v2_1,
        {
            ChargingStationMaxProfile <=> ChargingStationMaxProfile,
            TxDefaultProfile <=> TxDefaultProfile,
            TxProfile <=> TxProfile,
            ChargingStationExternalConstraints <=> ChargingStationExternalConstraints,
            PriorityCharging <=> PriorityCharging,
            LocalGeneration <=> LocalGeneration,
        },
        period_to_wire: |period: &Period| -> Result<v2_1::ChargingSchedulePeriod, ConversionError> {
            let mut wire = v2_1::ChargingSchedulePeriod::new(to_i32(period.start_period, "startPeriod")?);
            wire.limit = Some(period.limit);
            wire.number_phases = period.number_phases;
            wire.phase_to_use = period.phase_to_use;
            Ok(wire)
        },
        period_from_wire: |(index, period): (usize, &v2_1::ChargingSchedulePeriod)| -> Result<Period, ConversionError> {
            let refuse = |field| Err(ConversionError::UnmodelledField { field, period: Some(index) });
            for (field, set) in [
                ("limit_L2", period.limit_l2.is_some()),
                ("limit_L3", period.limit_l3.is_some()),
                ("dischargeLimit", period.discharge_limit.is_some()),
                ("dischargeLimit_L2", period.discharge_limit_l2.is_some()),
                ("dischargeLimit_L3", period.discharge_limit_l3.is_some()),
                ("setpoint", period.setpoint.is_some()),
                ("setpoint_L2", period.setpoint_l2.is_some()),
                ("setpoint_L3", period.setpoint_l3.is_some()),
                ("setpointReactive", period.setpoint_reactive.is_some()),
                ("setpointReactive_L2", period.setpoint_reactive_l2.is_some()),
                ("setpointReactive_L3", period.setpoint_reactive_l3.is_some()),
                ("v2xBaseline", period.v2x_baseline.is_some()),
                ("v2xFreqWattCurve", period.v2x_freq_watt_curve.is_some()),
                ("v2xSignalWattCurve", period.v2x_signal_watt_curve.is_some()),
            ] {
                if set {
                    return refuse(field);
                }
            }
            if period
                .operation_mode
                .as_ref()
                .is_some_and(|mode| *mode != v2_1::OperationMode::ChargingOnly)
            {
                return refuse("operationMode");
            }
            let limit = period
                .limit
                .ok_or(ConversionError::PeriodWithoutLimit { period: index })?;
            Ok(Period {
                start_period: i64::from(period.start_period),
                limit,
                number_phases: period.number_phases,
                phase_to_use: period.phase_to_use,
            })
        },
        schedule_check: |schedule: &v2_1::ChargingSchedule| -> Result<(), ConversionError> {
            let refuse = |field| Err(ConversionError::UnmodelledField { field, period: None });
            if schedule.limit_at_so_c.is_some() {
                return refuse("limitAtSoC");
            }
            if schedule.use_local_time == Some(true) {
                return refuse("useLocalTime");
            }
            if schedule.randomized_delay.is_some_and(|delay| delay > 0) {
                return refuse("randomizedDelay");
            }
            Ok(())
        },
    );
}
