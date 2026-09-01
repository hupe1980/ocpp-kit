//! Version-agnostic domain events.
//!
//! A CSMS that supports 1.6, 2.0.1 and 2.1 does not want three copies of its business logic.
//! What it actually reacts to — a station booted, a connector became occupied, a transaction
//! started, a card was presented — is the same in all three, even though the messages that
//! carry it are not.
//!
//! [`DomainEvent`] is that common model. It is **deliberately lossy**: it keeps what almost
//! every CSMS needs and drops the rest, which is why every conversion hands back the typed
//! original alongside it. Reach for the original whenever the detail matters.

use alloc::string::String;
use alloc::vec::Vec;

use crate::metering::SignedMeterValue;
use crate::types::{DateTime, Decimal};
use crate::version::Version;

/// What a Charging Station told the CSMS, in version-neutral terms.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DomainEvent {
    /// The station booted and is asking to be accepted.
    Booted {
        /// Vendor name (1.6 `chargePointVendor`, 2.x `chargingStation.vendorName`).
        vendor: String,
        /// Model name.
        model: String,
        /// Serial number, when the station reports one.
        serial_number: Option<String>,
        /// Firmware version.
        firmware_version: Option<String>,
        /// Why it booted. 1.6 does not say, so this is `None` there.
        reason: Option<String>,
    },
    /// A heartbeat.
    Heartbeat,
    /// A connector changed state.
    StatusChanged {
        /// The EVSE. 1.6 has no EVSEs, so the connector number is reported there.
        evse_id: Option<i32>,
        /// The connector.
        connector_id: Option<i32>,
        /// The new status.
        status: String,
        /// The 1.6 `errorCode`; 2.x reports faults through `NotifyEvent` instead.
        error_code: Option<String>,
        /// When it changed.
        timestamp: Option<DateTime>,
    },
    /// A token was presented and the station is asking whether it may charge.
    AuthorizeRequested {
        /// The token.
        id_token: String,
        /// Its type, in 2.x.
        id_token_type: Option<String>,
    },
    /// A transaction began.
    TransactionStarted {
        /// The transaction id. 1.6 assigns it in the *response*, so it is `None` here.
        transaction_id: Option<String>,
        /// The EVSE the transaction is at. 1.6 has no EVSEs; see
        /// [`connector_id`](Self::TransactionStarted::connector_id).
        evse_id: Option<i32>,
        /// The connector, when the version or the message names one. 1.6's
        /// `StartTransaction.connectorId` is mandatory, so this is always set there.
        connector_id: Option<i32>,
        /// The event's position in the transaction; 1.6 has none.
        seq_no: Option<i32>,
        /// The authorizing token.
        id_token: Option<String>,
        /// The energy register at the start.
        meter_start: Option<EnergyReading>,
        /// Every signed meter value the event carried, in the order they arrived.
        signed: Vec<SignedReading>,
        /// When it started.
        timestamp: DateTime,
    },
    /// Something changed during a transaction.
    TransactionUpdated {
        /// The transaction id.
        transaction_id: String,
        /// The EVSE the transaction is at. 1.6 has no EVSEs; see
        /// [`connector_id`](Self::TransactionUpdated::connector_id).
        evse_id: Option<i32>,
        /// The connector, when the version or the message names one.
        connector_id: Option<i32>,
        /// The station's own account of whether energy is flowing — `Charging`,
        /// `SuspendedEV`, `Idle`, and the rest of `ChargingStateEnumType`.
        ///
        /// The one fact a meter reading cannot supply, and it decides money: a fast charger
        /// may add an occupancy fee for the time a vehicle is connected and *not* charging
        /// (AFIR Art. 5(4)), so a period on the wrong side of this flag is billed at the
        /// wrong rate. A register sitting at `0.000` kWh is a taper if the station says
        /// `Charging` and an occupancy if it says `SuspendedEV`, and no amount of metering
        /// data settles which.
        ///
        /// `None` in 1.6, which has no such field and says it through `StatusNotification` —
        /// a [`StatusChanged`](Self::StatusChanged) event here — instead.
        charging_state: Option<String>,
        /// The event's position in the transaction; 1.6 has none.
        seq_no: Option<i32>,
        /// The energy register.
        meter: Option<EnergyReading>,
        /// Every signed meter value the event carried, in the order they arrived.
        signed: Vec<SignedReading>,
        /// Whether the station was offline when it happened.
        offline: bool,
        /// When it happened.
        timestamp: DateTime,
    },
    /// A transaction finished.
    TransactionEnded {
        /// The transaction id.
        transaction_id: String,
        /// The EVSE the transaction was at. 1.6 has no EVSEs, and its `StopTransaction` names
        /// no connector either, so both are `None` there — the EVSE is the one the
        /// [`TransactionStarted`](Self::TransactionStarted) event reported.
        evse_id: Option<i32>,
        /// The connector, when the version or the message names one.
        connector_id: Option<i32>,
        /// The station's last account of whether energy was flowing. See
        /// [`TransactionUpdated::charging_state`](Self::TransactionUpdated::charging_state).
        charging_state: Option<String>,
        /// The event's position in the transaction; 1.6 has none.
        seq_no: Option<i32>,
        /// Why it stopped.
        stopped_reason: Option<String>,
        /// The energy register at the end.
        meter_stop: Option<EnergyReading>,
        /// Every signed meter value the event carried, in the order they arrived.
        ///
        /// In 1.6 this is where a whole transaction's signed records turn up at once: the
        /// begin record has no message of its own, so both travel in
        /// `StopTransaction.transactionData`.
        signed: Vec<SignedReading>,
        /// When it ended.
        timestamp: DateTime,
    },
    /// Metering samples arrived outside a transaction event.
    MeterValues {
        /// The EVSE. 1.6 has no EVSEs; see
        /// [`connector_id`](Self::MeterValues::connector_id).
        evse_id: Option<i32>,
        /// The connector. 1.6's `MeterValues.connectorId` is mandatory, so this is always set
        /// there; 2.x's `MeterValuesRequest` names an EVSE and no connector.
        connector_id: Option<i32>,
        /// The transaction, when the samples belong to one.
        transaction_id: Option<String>,
        /// How many samples arrived.
        samples: usize,
        /// The energy register, when one of the samples carried it.
        energy: Option<EnergyReading>,
        /// Every signed meter value the message carried, in the order they arrived.
        signed: Vec<SignedReading>,
        /// When the last set of samples was taken.
        ///
        /// `MeterValues` carries a timestamp per group of samples rather than one for the
        /// message, so this is the latest of them — the instant the reading in
        /// [`energy`](Self::MeterValues::energy) belongs to.
        timestamp: Option<DateTime>,
    },
    /// A security event.
    SecurityEvent {
        /// The event type, from the standard list in the specification's appendix.
        event_type: String,
        /// When it happened.
        timestamp: DateTime,
        /// Free-form detail.
        info: Option<String>,
    },
    /// A firmware update moved on.
    FirmwareStatus {
        /// The new status.
        status: String,
    },
    /// The message carries no event this model covers. Use the typed original.
    Other {
        /// The action name.
        action: String,
    },
}

/// A domain event together with the version it came from and anything that could not be
/// carried across.
#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    /// The version-neutral view.
    pub event: DomainEvent,
    /// Which version produced it, so a handler can reach for the right typed original.
    pub version: Version,
    /// What the message said that this view could not carry.
    ///
    /// Usually empty. When it is not, the station sent something it should not have, and the
    /// failure is one of the quiet kind — see [`Warning`].
    pub warnings: Vec<Warning>,
}

impl Observed {
    // Only the per-version conversions build one, and each is behind its version's feature.
    #[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
    fn new(version: Version, event: DomainEvent, warnings: Vec<Warning>) -> Self {
        Self {
            event,
            version,
            warnings,
        }
    }
}

/// Something a station sent that the version-neutral view could not carry.
///
/// The model is [deliberately lossy](self), and most of what it drops is detail a CSMS does
/// not need. These are the drops that are *not* like that: each is a station saying something
/// malformed about a value that decides money, and each otherwise fails silently — a station
/// that claims to send signed meter data and sends something unparseable looks, through the
/// funnel alone, exactly like one sending none.
///
/// Log them. They are rare enough that a line per occurrence costs nothing, and each names a
/// station that needs a firmware fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// What went wrong.
    pub kind: WarningKind,
    /// What the station actually sent, so the log line is actionable.
    pub detail: String,
}

impl Warning {
    #[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
    fn new(kind: WarningKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl core::fmt::Display for Warning {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.detail)
    }
}

/// Which kind of thing a [`Warning`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum WarningKind {
    /// A sampled value declared `format: SignedData` — so it claims to carry the record a
    /// customer may be billed for — and the document in it does not parse. 1.6 only; 2.x has
    /// a typed field that cannot be malformed this way.
    UnreadableSignedData,
    /// A sampled value carrying an energy register is not a number. 1.6 spells a reading as
    /// a string, so this is what a station sending anything else looks like. Samples of other
    /// measurands are not reported: this model does not carry them, so there is nothing lost
    /// to report.
    UnreadableReading,
    /// An energy register arrived in a unit that is not an energy unit, so it could not be
    /// converted to Wh. Assuming Wh would be a factor-1000 error in someone's invoice.
    UnknownEnergyUnit,
    /// An energy register, or its `multiplier`, puts the value outside what a
    /// [`Decimal`] can hold. A meter reading needs 19 digits only if something is wrong.
    UnrepresentableReading,
}

impl WarningKind {
    /// A short stable name, for a log line or a metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            WarningKind::UnreadableSignedData => "unreadable signed data",
            WarningKind::UnreadableReading => "unreadable reading",
            WarningKind::UnknownEnergyUnit => "unknown energy unit",
            WarningKind::UnrepresentableReading => "unrepresentable reading",
        }
    }
}

impl core::fmt::Display for WarningKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One energy-register reading, as a station reported it.
///
/// The register is an exact [`Decimal`] at the resolution the meter wrote, because that is
/// what a reading *is*: `2935.600` kWh claims three decimals, and a value that has been
/// through an `f64` can no longer make that claim. See [`Decimal`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EnergyReading {
    /// The register, in **Wh**, converted exactly.
    ///
    /// Both the 2.x `unitOfMeasure.multiplier` (a power of ten) and a `kWh` unit are applied
    /// by moving the decimal point, so the conversion cannot introduce a rounding error the
    /// way multiplying by `1000.0` can.
    pub wh: Decimal,
    /// The sample's `context` — `Transaction.Begin`, `Sample.Periodic`, … — when it named one.
    pub context: Option<String>,
    /// The signed form of *this* reading, when the same sample carried one.
    ///
    /// The event's own `signed` list is the complete set; this is the one attached to the
    /// sample the register was taken from, which is usually but not always the same thing.
    ///
    /// Always `None` in 1.6, where a signed record is not attached to a measurement but *is*
    /// the sample — see [`metering`](crate::metering). Read the event's list there.
    pub signed: Option<SignedMeterValue>,
}

/// A signed meter value, together with what the sample that carried it said about it.
///
/// The [`value`](Self::value) is carried through **verbatim** — the signature covers those
/// exact bytes, so nothing here decodes, re-encodes or normalizes them. The `context` comes
/// with it because it is what tells a begin record from an end one, and in 1.6 both arrive on
/// the same message.
///
/// See [`metering`](crate::metering) for why this, and not the protocol's own numbers, is the
/// value a customer may be billed for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignedReading {
    /// The signed record, exactly as it arrived.
    pub value: SignedMeterValue,
    /// The sample's `context` — `Transaction.Begin`, `Transaction.End`, … — when it named one.
    pub context: Option<String>,
    /// The sample's `measurand`, when it named one.
    pub measurand: Option<String>,
}

/// One sampled value, reduced to the parts the energy extraction looks at.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
struct Sample<'a> {
    measurand: Option<&'a str>,
    unit: Option<&'a str>,
    multiplier: Option<i32>,
    context: Option<&'a str>,
    value: Option<Decimal>,
    /// The text of a measurement that is not a number. 1.6 spells a reading as a string, so
    /// this is the only version where it can happen — and it is worth reporting rather than
    /// dropping, which is why it is kept apart from `value: None` ("no measurement here").
    unreadable_value: Option<&'a str>,
    signed: Option<Result<SignedMeterValue, crate::metering::SignedDataError>>,
}

/// Which of several matching samples to take.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
#[derive(Clone, Copy, PartialEq, Eq)]
// Only the 2.x conversions ask for `First`: 1.6 has no message carrying both ends of a
// transaction, so a build with 1.6 alone never constructs it.
#[cfg_attr(
    not(any(feature = "v2_0_1", feature = "v2_1")),
    allow(dead_code, clippy::allow_attributes)
)]
enum Prefer {
    /// The first — the opening reading of a transaction.
    First,
    /// The last — the closing reading. A `TransactionEvent` may carry several `meterValue`
    /// entries, and taking the first would bill the wrong end of the transaction.
    Last,
}

/// The measurand every version reports an energy register under, and the one assumed when a
/// sample names none (both schemas default to it).
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
const ENERGY_REGISTER: &str = "Energy.Active.Import.Register";

/// Collects every signed meter value out of a set of samples, in arrival order.
///
/// Untouched, and unfiltered: a signed record is the billable value whatever measurand or
/// context the sample around it names, so nothing is selected away here the way the energy
/// extraction selects a register.
///
/// A 1.6 `SignedData` sample whose document does not parse cannot be carried — but it is not
/// dropped in silence either: it raises a [`WarningKind::UnreadableSignedData`], because a
/// station that claims to send the billable record and sends something unparseable is
/// otherwise indistinguishable from one that sends none.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
fn signed_from_samples<'a>(
    samples: impl Iterator<Item = Sample<'a>>,
    warnings: &mut Vec<Warning>,
) -> Vec<SignedReading> {
    let mut out = Vec::new();
    for sample in samples {
        match sample.signed {
            Some(Ok(value)) => out.push(SignedReading {
                value,
                context: sample.context.map(alloc::string::ToString::to_string),
                measurand: sample.measurand.map(alloc::string::ToString::to_string),
            }),
            Some(Err(error)) => warnings.push(Warning::new(
                WarningKind::UnreadableSignedData,
                error.reason(),
            )),
            None => {}
        }
    }
    out
}

/// Extracts the energy register, in Wh, from a set of sampled values.
///
/// Three things routinely go wrong here and all three are handled:
///
/// * **The unit.** Both versions allow `kWh`, and 2.x adds a `multiplier` exponent on top.
///   Missing either is a factor-1000 bug in someone's invoice. Both are applied as decimal
///   point shifts, which are exact. A unit that is not an energy unit at all raises a
///   [`WarningKind::UnknownEnergyUnit`] rather than being assumed to be Wh.
/// * **Which sample.** An event may carry several readings — a `Transaction.Begin` and a
///   `Sample.Periodic`, say. `context` decides when it is stated, and `prefer` decides
///   otherwise; taking whichever came first is only right for the start of a transaction.
/// * **An unparseable value.** 1.6 spells the reading as a *string*, so it can be anything.
///   Such a sample is skipped — never turned into a `NaN` that poisons every sum it reaches —
///   and raises a [`WarningKind::UnreadableReading`].
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
fn energy_from_samples<'a>(
    samples: impl Iterator<Item = Sample<'a>>,
    prefer: Prefer,
    preferred_context: Option<&str>,
    warnings: &mut Vec<Warning>,
) -> Option<EnergyReading> {
    let mut best: Option<EnergyReading> = None;
    let mut best_in_context: Option<EnergyReading> = None;

    for sample in samples {
        if sample.measurand.unwrap_or(ENERGY_REGISTER) != ENERGY_REGISTER {
            continue;
        }
        let Some(value) = sample.value else {
            if let Some(text) = sample.unreadable_value {
                warnings.push(Warning::new(WarningKind::UnreadableReading, text));
            }
            continue;
        };
        // A unit that is not an energy unit is not this sample's problem to solve — skip it
        // and keep looking, rather than abandoning readings that come after it.
        let Some(exponent) = unit_exponent(sample.unit) else {
            warnings.push(Warning::new(
                WarningKind::UnknownEnergyUnit,
                sample.unit.unwrap_or_default(),
            ));
            continue;
        };
        // The multiplier scales the measurand; the unit is a separate conversion. Part 2
        // says so explicitly, and applying them in the other order gives the same answer only
        // because both are powers of ten.
        let converted = value
            .checked_pow10(sample.multiplier.unwrap_or(0))
            .and_then(|scaled| scaled.checked_pow10(exponent));
        let Some(wh) = converted else {
            warnings.push(Warning::new(
                WarningKind::UnrepresentableReading,
                alloc::format!("{value} with multiplier {}", sample.multiplier.unwrap_or(0)),
            ));
            continue;
        };
        let reading = EnergyReading {
            wh,
            context: sample.context.map(alloc::string::ToString::to_string),
            signed: sample.signed.and_then(Result::ok),
        };
        let in_context = preferred_context.is_some_and(|wanted| sample.context == Some(wanted));
        for slot in [Some(&mut best), in_context.then_some(&mut best_in_context)]
            .into_iter()
            .flatten()
        {
            if slot.is_none() || prefer == Prefer::Last {
                *slot = Some(reading.clone());
            }
        }
    }
    best_in_context.or(best)
}

/// How many powers of ten separate a unit from Wh, or `None` when the unit is not an energy
/// unit at all — at which point guessing would be worse than reporting nothing.
///
/// An absent unit means Wh: that is the schema default for an energy measurand in every
/// version. The comparison is case-insensitive because the field is free text in 2.x and
/// stations spell it `KWH`.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
fn unit_exponent(unit: Option<&str>) -> Option<i32> {
    match unit {
        None => Some(0),
        Some(unit) if unit.eq_ignore_ascii_case("wh") => Some(0),
        Some(unit) if unit.eq_ignore_ascii_case("kwh") => Some(3),
        Some(unit) if unit.eq_ignore_ascii_case("mwh") => Some(6),
        Some(_) => None,
    }
}

#[cfg(feature = "v1_6")]
mod v16_conversion {
    use super::{
        DomainEvent, EnergyReading, Observed, Prefer, Sample, energy_from_samples,
        signed_from_samples,
    };
    use crate::types::Decimal;
    use crate::v1_6;
    use crate::version::Version;
    use alloc::string::ToString;
    use alloc::vec::Vec;

    /// The meter values a `StopTransaction` carries. 1.6 puts a whole transaction's signed
    /// records here, because there is no start message to put the begin one on.
    fn transaction_data(stop: &v1_6::StopTransactionRequest) -> &[v1_6::MeterValue] {
        stop.transaction_data.as_deref().unwrap_or_default()
    }

    /// 1.6 spells a sampled value as a *string*, so both of the things a sample can carry are
    /// parsed rather than decoded:
    ///
    /// * a measurement, which is a number in a string — one that is not a number yields no
    ///   reading at all, because the alternative, a `NaN`, poisons every total it reaches;
    /// * a signed meter value, which with `format: SignedData` is a whole JSON document in
    ///   that same string (OCA SMV §3.2.1).
    fn samples(values: &[v1_6::MeterValue]) -> impl Iterator<Item = Sample<'_>> {
        values.iter().flat_map(|entry| {
            entry.sampled_value.iter().map(|sample| {
                let signed = sample.signed_meter_value();
                // A `SignedData` sample's `value` is a JSON document by design: not an
                // unreadable measurement, but not a measurement at all.
                let measurement = signed.is_none().then(|| sample.value.parse::<Decimal>());
                Sample {
                    measurand: sample.measurand.as_ref().map(v1_6::Measurand::as_str),
                    unit: sample.unit.as_ref().map(v1_6::UnitOfMeasure::as_str),
                    multiplier: None,
                    context: sample.context.as_ref().map(v1_6::ReadingContext::as_str),
                    value: measurement.and_then(Result::ok),
                    unreadable_value: matches!(measurement, Some(Err(_)))
                        .then_some(sample.value.as_str()),
                    signed,
                }
            })
        })
    }

    /// Maps an OCPP 1.6 station-originated request onto the common model.
    #[must_use]
    pub fn observe(request: &v1_6::CsRequest) -> Observed {
        let mut warnings = Vec::new();
        let event = match request {
            v1_6::CsRequest::BootNotification(boot) => DomainEvent::Booted {
                vendor: boot.charge_point_vendor.clone(),
                model: boot.charge_point_model.clone(),
                serial_number: boot
                    .charge_point_serial_number
                    .clone()
                    .or_else(|| boot.charge_box_serial_number.clone()),
                firmware_version: boot.firmware_version.clone(),
                reason: None,
            },
            v1_6::CsRequest::Heartbeat(_) => DomainEvent::Heartbeat,
            v1_6::CsRequest::StatusNotification(status) => DomainEvent::StatusChanged {
                evse_id: None,
                connector_id: Some(status.connector_id),
                status: status.status.as_str().to_string(),
                error_code: Some(status.error_code.as_str().to_string()),
                timestamp: status.timestamp,
            },
            v1_6::CsRequest::Authorize(authorize) => DomainEvent::AuthorizeRequested {
                id_token: authorize.id_tag.clone(),
                id_token_type: None,
            },
            v1_6::CsRequest::StartTransaction(start) => DomainEvent::TransactionStarted {
                // 1.6 assigns the transaction id in StartTransaction.conf, not here.
                transaction_id: None,
                // 1.6 has connectors, not EVSEs, and says so rather than reporting one as
                // the other: a CSMS addressing a 2.x-shaped inventory would otherwise aim at
                // the wrong outlet.
                evse_id: None,
                connector_id: Some(start.connector_id),
                seq_no: None,
                id_token: Some(start.id_tag.clone()),
                // 1.6 states meterStart and meterStop as whole Wh, so they are exact as they
                // stand — there is no resolution claim to lose.
                meter_start: Some(EnergyReading {
                    wh: Decimal::from(start.meter_start),
                    context: Some(v1_6::ReadingContext::TransactionBegin.as_str().to_string()),
                    signed: None,
                }),
                // 1.6 gives StartTransaction nowhere to put a signed record; the begin one
                // travels with the stop message instead.
                signed: Vec::new(),
                timestamp: start.timestamp,
            },
            v1_6::CsRequest::StopTransaction(stop) => DomainEvent::TransactionEnded {
                transaction_id: stop.transaction_id.to_string(),
                // `StopTransaction` names neither; the EVSE is the one the start reported.
                evse_id: None,
                connector_id: None,
                // 1.6 has no chargingState. It says the same thing through
                // `StatusNotification`, which arrives as its own event.
                charging_state: None,
                seq_no: None,
                stopped_reason: stop
                    .reason
                    .as_ref()
                    .map(|reason| reason.as_str().to_string()),
                // `meterStop` is the meter's own register, which in the OCA's example
                // message is its *lifetime* total while the signed record beside it reports
                // the session. They are not the same quantity; see `crate::metering`.
                meter_stop: Some(EnergyReading {
                    wh: Decimal::from(stop.meter_stop),
                    context: Some(v1_6::ReadingContext::TransactionEnd.as_str().to_string()),
                    signed: None,
                }),
                signed: signed_from_samples(samples(transaction_data(stop)), &mut warnings),
                timestamp: stop.timestamp,
            },
            v1_6::CsRequest::MeterValues(values) => {
                let count: usize = values
                    .meter_value
                    .iter()
                    .map(|entry| entry.sampled_value.len())
                    .sum();
                DomainEvent::MeterValues {
                    evse_id: None,
                    connector_id: Some(values.connector_id),
                    transaction_id: values.transaction_id.map(|id| id.to_string()),
                    samples: count,
                    energy: energy_from_samples(
                        samples(&values.meter_value),
                        Prefer::Last,
                        None,
                        &mut warnings,
                    ),
                    signed: signed_from_samples(samples(&values.meter_value), &mut warnings),
                    timestamp: values.meter_value.iter().map(|entry| entry.timestamp).max(),
                }
            }
            v1_6::CsRequest::SecurityEventNotification(security) => DomainEvent::SecurityEvent {
                event_type: security.r#type.clone(),
                timestamp: security.timestamp,
                info: security.tech_info.clone(),
            },
            v1_6::CsRequest::FirmwareStatusNotification(status) => DomainEvent::FirmwareStatus {
                status: status.status.as_str().to_string(),
            },
            other => DomainEvent::Other {
                action: other.action().as_str().to_string(),
            },
        };
        Observed::new(Version::V1_6, event, warnings)
    }
}

#[cfg(feature = "v1_6")]
pub use v16_conversion::observe as observe_v16;

/// Generates the 2.x conversions, which differ only in the module they name.
#[cfg(any(feature = "v2_0_1", feature = "v2_1"))]
macro_rules! observe_2x {
    // The two expansions are otherwise identical, so each gets a module of its own rather
    // than a set of names spelled twice.
    ($module:ident, $version:expr, $wrapper:ident, $name:ident) => {
        mod $wrapper {
            use super::{
                DomainEvent, Observed, Prefer, Sample, Warning, energy_from_samples,
                signed_from_samples,
            };
            use crate::version::Version;
            use alloc::vec::Vec;

            /// One sampled value of this version, reduced to what the energy extraction reads.
            fn samples(values: &[crate::$module::MeterValue]) -> impl Iterator<Item = Sample<'_>> {
                use crate::$module as v;
                values.iter().flat_map(|entry| {
                    entry.sampled_value.iter().map(|sample| Sample {
                        measurand: sample.measurand.as_ref().map(v::Measurand::as_str),
                        unit: sample
                            .unit_of_measure
                            .as_ref()
                            .and_then(|unit| unit.unit.as_deref()),
                        multiplier: sample
                            .unit_of_measure
                            .as_ref()
                            .and_then(|unit| unit.multiplier),
                        context: sample.context.as_ref().map(v::ReadingContext::as_str),
                        value: Some(sample.value),
                        // 2.x types a measurement as a JSON number, so there is no such
                        // thing as one that does not parse.
                        unreadable_value: None,
                        signed: sample
                            .signed_meter_value
                            .as_ref()
                            .map(|value| Ok(value.into())),
                    })
                })
            }

            /// Maps a station-originated request onto the common model.
            #[must_use]
            #[allow(clippy::too_many_lines)]
            pub fn observe(request: &crate::$module::CsRequest) -> Observed {
                use crate::$module as v;
                use alloc::string::ToString as _;
                let mut warnings = Vec::new();
                let event = match request {
                    v::CsRequest::BootNotification(boot) => DomainEvent::Booted {
                        vendor: boot.charging_station.vendor_name.clone(),
                        model: boot.charging_station.model.clone(),
                        serial_number: boot.charging_station.serial_number.clone(),
                        firmware_version: boot.charging_station.firmware_version.clone(),
                        reason: Some(boot.reason.as_str().to_string()),
                    },
                    v::CsRequest::Heartbeat(_) => DomainEvent::Heartbeat,
                    v::CsRequest::StatusNotification(status) => DomainEvent::StatusChanged {
                        evse_id: Some(status.evse_id),
                        connector_id: Some(status.connector_id),
                        status: status.connector_status.as_str().to_string(),
                        error_code: None,
                        timestamp: Some(status.timestamp),
                    },
                    v::CsRequest::Authorize(authorize) => DomainEvent::AuthorizeRequested {
                        id_token: authorize.id_token.id_token.clone(),
                        id_token_type: Some(authorize.id_token.r#type.as_str().to_string()),
                    },
                    v::CsRequest::TransactionEvent(event) => {
                        let entries: &[v::MeterValue] =
                            event.meter_value.as_deref().unwrap_or_default();
                        let evse_id = event.evse.as_ref().map(|evse| evse.id);
                        let connector_id = event.evse.as_ref().and_then(|evse| evse.connector_id);
                        let charging_state = event
                            .transaction_info
                            .charging_state
                            .as_ref()
                            .map(|state| state.as_str().to_string());
                        // Which reading an event means depends on which end of the transaction it
                        // is: an `Ended` event carrying both a periodic sample and the closing
                        // one must bill the closing one.
                        let energy = |prefer, context, warnings: &mut Vec<Warning>| {
                            energy_from_samples(samples(entries), prefer, Some(context), warnings)
                        };
                        match event.event_type {
                            v::TransactionEventEnum::Started => DomainEvent::TransactionStarted {
                                transaction_id: Some(event.transaction_info.transaction_id.clone()),
                                evse_id,
                                connector_id,
                                seq_no: Some(event.seq_no),
                                id_token: event
                                    .id_token
                                    .as_ref()
                                    .map(|token| token.id_token.clone()),
                                meter_start: energy(
                                    Prefer::First,
                                    v::ReadingContext::TransactionBegin.as_str(),
                                    &mut warnings,
                                ),
                                signed: signed_from_samples(samples(entries), &mut warnings),
                                timestamp: event.timestamp,
                            },
                            v::TransactionEventEnum::Ended => DomainEvent::TransactionEnded {
                                transaction_id: event.transaction_info.transaction_id.clone(),
                                evse_id,
                                connector_id,
                                charging_state,
                                seq_no: Some(event.seq_no),
                                stopped_reason: event
                                    .transaction_info
                                    .stopped_reason
                                    .as_ref()
                                    .map(|reason| reason.as_str().to_string()),
                                meter_stop: energy(
                                    Prefer::Last,
                                    v::ReadingContext::TransactionEnd.as_str(),
                                    &mut warnings,
                                ),
                                signed: signed_from_samples(samples(entries), &mut warnings),
                                timestamp: event.timestamp,
                            },
                            _ => DomainEvent::TransactionUpdated {
                                transaction_id: event.transaction_info.transaction_id.clone(),
                                evse_id,
                                connector_id,
                                charging_state,
                                seq_no: Some(event.seq_no),
                                meter: energy_from_samples(
                                    samples(entries),
                                    Prefer::Last,
                                    None,
                                    &mut warnings,
                                ),
                                signed: signed_from_samples(samples(entries), &mut warnings),
                                offline: event.offline.unwrap_or(false),
                                timestamp: event.timestamp,
                            },
                        }
                    }
                    v::CsRequest::MeterValues(values) => {
                        let count: usize = values
                            .meter_value
                            .iter()
                            .map(|entry| entry.sampled_value.len())
                            .sum();
                        DomainEvent::MeterValues {
                            evse_id: Some(values.evse_id),
                            // 2.x's `MeterValuesRequest` names an EVSE and no connector.
                            connector_id: None,
                            transaction_id: None,
                            samples: count,
                            energy: energy_from_samples(
                                samples(&values.meter_value),
                                Prefer::Last,
                                None,
                                &mut warnings,
                            ),
                            signed: signed_from_samples(
                                samples(&values.meter_value),
                                &mut warnings,
                            ),
                            timestamp: values.meter_value.iter().map(|entry| entry.timestamp).max(),
                        }
                    }
                    v::CsRequest::SecurityEventNotification(security) => {
                        DomainEvent::SecurityEvent {
                            event_type: security.r#type.clone(),
                            timestamp: security.timestamp,
                            info: security.tech_info.clone(),
                        }
                    }
                    v::CsRequest::FirmwareStatusNotification(status) => {
                        DomainEvent::FirmwareStatus {
                            status: status.status.as_str().to_string(),
                        }
                    }
                    other => DomainEvent::Other {
                        action: other.action().as_str().to_string(),
                    },
                };
                Observed::new($version, event, warnings)
            }
        }

        pub use $wrapper::observe as $name;
    };
}

#[cfg(feature = "v2_0_1")]
observe_2x!(v2_0_1, Version::V2_0_1, v201_conversion, observe_v201);
#[cfg(feature = "v2_1")]
observe_2x!(v2_1, Version::V2_1, v21_conversion, observe_v21);

/// Turns a domain event into the ledger's neutral shape, when it is transaction-related.
///
/// The bridge between [`DomainEvent`] and [`Ledger`](super::ledger::Ledger): one call, and a
/// CSMS has an idempotent transaction record regardless of which version the station speaks.
/// Every version reaches the ledger through this one funnel, which is why supporting a fourth
/// would not mean a fourth copy of the accounting.
#[must_use]
pub fn to_ledger_event(
    identity: &crate::types::Identity,
    observed: &Observed,
) -> Option<super::ledger::TransactionEvent> {
    to_ledger_event_inner(identity, observed, None)
}

/// [`to_ledger_event`], supplying the transaction id the CSMS itself assigned.
///
/// OCPP 1.6 assigns the transaction id in `StartTransaction.conf` — *after* the request has
/// been observed — so the start event carries none, and with it the meter register the
/// transaction began at cannot be placed. A 1.6 CSMS calls this from the handler that chose
/// the id.
///
/// It matters more than it looks. Without the start event, the first periodic `MeterValues`
/// becomes the ledger's start reading, and every 1.6 session is billed from a register taken
/// minutes after the car began charging. On 2.x the id is already in the event and this
/// behaves exactly like [`to_ledger_event`].
#[must_use]
pub fn to_ledger_event_with_id(
    identity: &crate::types::Identity,
    observed: &Observed,
    transaction_id: &str,
) -> Option<super::ledger::TransactionEvent> {
    to_ledger_event_inner(identity, observed, Some(transaction_id))
}

/// The pieces of a domain event a ledger entry is built from, whichever variant carried them.
struct LedgerParts<'a> {
    transaction_id: String,
    evse_id: Option<i32>,
    connector_id: Option<i32>,
    seq_no: Option<i32>,
    kind: super::ledger::EventKind,
    timestamp: DateTime,
    reading: &'a Option<EnergyReading>,
    signed: &'a [SignedReading],
}

/// Destructures whichever variant a transaction's events arrive as.
///
/// Four shapes reach the ledger and they differ only in where each piece sits, which is the
/// whole reason this model exists — a version-specific CSMS would write this match four
/// times, once per version and once more for 1.6's `MeterValues`.
#[allow(clippy::too_many_lines)]
fn ledger_parts<'a>(event: &'a DomainEvent, assigned: Option<&str>) -> Option<LedgerParts<'a>> {
    use super::ledger::EventKind;
    let parts = match event {
        DomainEvent::TransactionStarted {
            transaction_id,
            evse_id,
            connector_id,
            seq_no,
            meter_start,
            signed,
            timestamp,
            ..
        } => LedgerParts {
            transaction_id: transaction_id
                .clone()
                .or_else(|| assigned.map(alloc::string::ToString::to_string))?,
            evse_id: *evse_id,
            connector_id: *connector_id,
            seq_no: *seq_no,
            kind: EventKind::Started,
            timestamp: *timestamp,
            reading: meter_start,
            signed,
        },
        DomainEvent::TransactionUpdated {
            transaction_id,
            evse_id,
            connector_id,
            seq_no,
            meter,
            signed,
            timestamp,
            ..
        } => LedgerParts {
            transaction_id: transaction_id.clone(),
            evse_id: *evse_id,
            connector_id: *connector_id,
            seq_no: *seq_no,
            kind: EventKind::Updated,
            timestamp: *timestamp,
            reading: meter,
            signed,
        },
        DomainEvent::TransactionEnded {
            transaction_id,
            evse_id,
            connector_id,
            seq_no,
            meter_stop,
            signed,
            timestamp,
            ..
        } => LedgerParts {
            transaction_id: transaction_id.clone(),
            evse_id: *evse_id,
            connector_id: *connector_id,
            seq_no: *seq_no,
            kind: EventKind::Ended,
            timestamp: *timestamp,
            reading: meter_stop,
            signed,
        },
        // 1.6 reports mid-transaction readings as `MeterValues` with a `transactionId`
        // rather than as a transaction event — it has no such message. Folding it in here is
        // what makes `StartTransaction` / `MeterValues` / `StopTransaction` one shape, which
        // is the whole claim the ledger makes about 1.6. A 2.x `MeterValues` names no
        // transaction and stays out, as it should: it is not part of one.
        DomainEvent::MeterValues {
            transaction_id: Some(transaction_id),
            evse_id,
            connector_id,
            energy,
            signed,
            timestamp: Some(timestamp),
            ..
        } => LedgerParts {
            transaction_id: transaction_id.clone(),
            evse_id: *evse_id,
            connector_id: *connector_id,
            seq_no: None,
            kind: EventKind::Updated,
            timestamp: *timestamp,
            reading: energy,
            signed,
        },
        _ => return None,
    };
    Some(parts)
}

fn to_ledger_event_inner(
    identity: &crate::types::Identity,
    observed: &Observed,
    assigned: Option<&str>,
) -> Option<super::ledger::TransactionEvent> {
    use super::ledger::TransactionEvent;
    let parts = ledger_parts(&observed.event, assigned)?;

    let mut event = TransactionEvent::new(
        identity.clone(),
        parts.transaction_id,
        parts.seq_no.unwrap_or(0),
        parts.kind,
        parts.timestamp,
    );
    event.evse_id = parts.evse_id;
    event.connector_id = parts.connector_id;
    event.signed.extend_from_slice(parts.signed);
    if let Some(reading) = parts.reading {
        event.meter_wh = Some(reading.wh);
    }
    match &observed.event {
        DomainEvent::TransactionStarted { id_token, .. } => event.id_token.clone_from(id_token),
        DomainEvent::TransactionUpdated { offline, .. } => event.offline = *offline,
        DomainEvent::TransactionEnded { stopped_reason, .. } => {
            event.stopped_reason.clone_from(stopped_reason);
        }
        _ => {}
    }
    Some(event)
}

/// Every action that maps to something other than [`DomainEvent::Other`].
///
/// Checked rather than asserted: `tests/domain_events.rs` generates a schema-valid request for
/// every action of every version and confirms that it maps to a modelled event exactly when
/// this list says it does, so an action the code generator adds cannot quietly go unmodelled.
pub const COVERED_ACTIONS: &[&str] = &[
    "Authorize",
    "BootNotification",
    "FirmwareStatusNotification",
    "Heartbeat",
    "MeterValues",
    "SecurityEventNotification",
    "StartTransaction",
    "StatusNotification",
    "StopTransaction",
    "TransactionEvent",
];

#[cfg(all(test, feature = "v1_6", feature = "v2_1"))]
mod tests {
    use super::*;
    use crate::{v1_6, v2_1};

    #[test]
    fn a_boot_looks_the_same_in_16_and_21() {
        let legacy = observe_v16(&v1_6::CsRequest::BootNotification(
            v1_6::BootNotificationRequest::new("ACME", "Model-1").with_firmware_version("1.2.3"),
        ));
        let modern = observe_v21(&v2_1::CsRequest::BootNotification(
            v2_1::BootNotificationRequest::new(
                v2_1::ChargingStation::new("Model-1", "ACME").with_firmware_version("1.2.3"),
                v2_1::BootReason::PowerUp,
            ),
        ));

        let DomainEvent::Booted {
            vendor,
            model,
            firmware_version,
            ..
        } = &legacy.event
        else {
            panic!("{:?}", legacy.event)
        };
        assert_eq!(vendor, "ACME");
        assert_eq!(model, "Model-1");
        assert_eq!(firmware_version.as_deref(), Some("1.2.3"));

        let DomainEvent::Booted {
            vendor,
            model,
            reason,
            ..
        } = &modern.event
        else {
            panic!("{:?}", modern.event)
        };
        assert_eq!(vendor, "ACME");
        assert_eq!(model, "Model-1");
        // 2.x adds the reason; 1.6 has none, and the model says so rather than inventing one.
        assert_eq!(reason.as_deref(), Some("PowerUp"));
        assert_eq!(legacy.version, Version::V1_6);
        assert_eq!(modern.version, Version::V2_1);
    }

    /// One kilowatt-hour is a thousand watt-hours and a `multiplier` of 3 is a thousand on
    /// top of that. Missing either is a factor-1000 error in someone's invoice, and both are
    /// applied by moving the decimal point, so the answer is exact.
    #[test]
    fn kwh_and_the_multiplier_are_both_applied_and_neither_rounds() {
        let sample = |value: crate::types::Decimal, unit: v2_1::UnitOfMeasure| {
            let request = v2_1::MeterValuesRequest::new(
                1,
                alloc::vec![v2_1::MeterValue::new(
                    alloc::vec![
                        v2_1::SampledValue::new(value)
                            .with_measurand(v2_1::Measurand::EnergyActiveImportRegister)
                            .with_unit_of_measure(unit)
                    ],
                    DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
                )],
            );
            let observed = observe_v21(&v2_1::CsRequest::MeterValues(request));
            let DomainEvent::MeterValues { energy, .. } = observed.event else {
                panic!("expected MeterValues")
            };
            energy
        };

        let kwh = v2_1::UnitOfMeasure::new().with_unit("kWh");
        let reading = sample(crate::decimal!(2935.600), kwh.clone()).expect("a reading");
        // 2935.600 kWh is 2935600 Wh exactly. The scale follows the point: three decimals
        // of a kilowatt-hour *are* whole watt-hours, so the resolution claim is carried
        // across the unit change rather than invented or lost.
        assert_eq!(reading.wh.to_string(), "2935600");

        // multiplier 3 on top of kWh: 1.5 → 1500 kWh → 1500000 Wh.
        let scaled = sample(
            crate::decimal!(1.5),
            v2_1::UnitOfMeasure::new()
                .with_unit("kWh")
                .with_multiplier(3),
        )
        .expect("a reading");
        assert_eq!(scaled.wh.to_string(), "1500000");

        // A unit that is not an energy unit at all yields nothing, rather than a number that
        // is silently a thousand times wrong.
        assert!(
            sample(
                crate::decimal!(1),
                v2_1::UnitOfMeasure::new().with_unit("A")
            )
            .is_none()
        );

        // …and it costs nothing else: a usable reading after an unusable one still arrives.
        let mixed = v2_1::MeterValuesRequest::new(
            1,
            alloc::vec![v2_1::MeterValue::new(
                alloc::vec![
                    v2_1::SampledValue::new(crate::decimal!(1))
                        .with_measurand(v2_1::Measurand::EnergyActiveImportRegister)
                        .with_unit_of_measure(v2_1::UnitOfMeasure::new().with_unit("A")),
                    v2_1::SampledValue::new(crate::decimal!(4500.5))
                        .with_measurand(v2_1::Measurand::EnergyActiveImportRegister),
                ],
                DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            )],
        );
        let observed = observe_v21(&v2_1::CsRequest::MeterValues(mixed));
        let DomainEvent::MeterValues { energy, .. } = observed.event else {
            panic!("expected MeterValues")
        };
        assert_eq!(energy.unwrap().wh.to_string(), "4500.5");
    }

    /// An `Ended` event may carry a periodic sample as well as the closing one. Billing the
    /// first reading in the array would bill the wrong end of the transaction.
    #[test]
    fn the_closing_reading_wins_on_an_ended_event() {
        let reading = |value, context| {
            v2_1::SampledValue::new(value)
                .with_measurand(v2_1::Measurand::EnergyActiveImportRegister)
                .with_context(context)
        };
        let request = v2_1::TransactionEventRequest::new(
            v2_1::TransactionEventEnum::Ended,
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
            v2_1::TriggerReason::StopAuthorized,
            2,
            v2_1::Transaction::new("tx-1"),
        )
        .with_meter_value(alloc::vec![v2_1::MeterValue::new(
            alloc::vec![
                reading(crate::decimal!(4100), v2_1::ReadingContext::SamplePeriodic),
                reading(
                    crate::decimal!(7300.250),
                    v2_1::ReadingContext::TransactionEnd
                ),
            ],
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
        )]);

        let observed = observe_v21(&v2_1::CsRequest::TransactionEvent(request));
        let DomainEvent::TransactionEnded { meter_stop, .. } = &observed.event else {
            panic!("{:?}", observed.event)
        };
        let reading = meter_stop.as_ref().expect("a closing reading");
        assert_eq!(reading.wh.to_string(), "7300.250");
        assert_eq!(reading.context.as_deref(), Some("Transaction.End"));
    }

    /// 1.6 spells a sampled value as a string, so it can be anything at all. Turning an
    /// unparseable one into a `NaN` — the obvious shortcut — poisons every total it reaches
    /// and compares unequal to itself.
    #[test]
    fn an_unparseable_16_reading_is_skipped_rather_than_becoming_a_nan() {
        let values = alloc::vec![v1_6::MeterValue::new(
            DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            alloc::vec![
                v1_6::SampledValue::new("not a number")
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
                v1_6::SampledValue::new("4500.5")
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
            ],
        )];
        let observed = observe_v16(&v1_6::CsRequest::MeterValues(
            v1_6::MeterValuesRequest::new(1, values),
        ));
        let DomainEvent::MeterValues {
            energy, samples, ..
        } = &observed.event
        else {
            panic!("{:?}", observed.event)
        };
        assert_eq!(*samples, 2);
        assert_eq!(energy.as_ref().unwrap().wh.to_string(), "4500.5");
    }

    /// 1.6 hands the transaction id back in `StartTransaction.conf`, so the start event —
    /// and the register the transaction began at — can only be placed once the CSMS has
    /// chosen one. Left unplaced, the first periodic `MeterValues` silently becomes the
    /// start reading and the session is billed from the wrong register.
    #[test]
    fn a_16_start_reaches_the_ledger_once_the_csms_has_assigned_an_id() {
        let identity = crate::types::Identity::new("CS-0001").unwrap();
        let start = v1_6::StartTransactionRequest::new(
            1,
            "CARD-1",
            2_935_600,
            DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
        );
        let observed = observe_v16(&v1_6::CsRequest::StartTransaction(start));

        // Without the id there is nothing to file the event under, and it says so.
        assert!(to_ledger_event(&identity, &observed).is_none());

        let event = to_ledger_event_with_id(&identity, &observed, "42").expect("a start event");
        let mut ledger = super::super::ledger::Ledger::new();
        ledger.ingest_unsequenced(&event);

        let stop = v1_6::StopTransactionRequest::new(
            2_952_100,
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
            42,
        );
        let observed = observe_v16(&v1_6::CsRequest::StopTransaction(stop));
        ledger.ingest_unsequenced(&to_ledger_event(&identity, &observed).expect("a stop event"));

        let record = ledger.transaction(&identity, "42").unwrap();
        assert_eq!(record.energy_wh().unwrap().to_string(), "16500");
    }

    /// The ledger's claim about 1.6 is that `StartTransaction` / `MeterValues` /
    /// `StopTransaction` fold into one shape. 1.6 has no transaction-event message, so a
    /// mid-transaction reading arrives as `MeterValues` naming a `transactionId` — and if the
    /// funnel drops it, the claim is not true and a session's periodic readings never reach
    /// the record.
    #[test]
    fn a_16_meter_values_message_naming_a_transaction_folds_into_the_ledger() {
        let identity = crate::types::Identity::new("CS-0001").unwrap();
        let mut ledger = super::super::ledger::Ledger::new();

        let start = observe_v16(&v1_6::CsRequest::StartTransaction(
            v1_6::StartTransactionRequest::new(
                1,
                "CARD-1",
                1_000,
                DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            ),
        ));
        ledger.ingest_unsequenced(&to_ledger_event_with_id(&identity, &start, "42").unwrap());

        let periodic = observe_v16(&v1_6::CsRequest::MeterValues(
            v1_6::MeterValuesRequest::new(
                1,
                alloc::vec![v1_6::MeterValue::new(
                    DateTime::parse("2024-01-01T00:30:00Z").unwrap(),
                    alloc::vec![
                        v1_6::SampledValue::new("4500.5")
                            .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
                    ],
                )],
            )
            .with_transaction_id(42),
        ));
        let event = to_ledger_event(&identity, &periodic).expect("a mid-transaction reading");
        assert_eq!(event.kind, super::super::ledger::EventKind::Updated);
        ledger.ingest_unsequenced(&event);

        let record = ledger.transaction(&identity, "42").unwrap();
        assert_eq!(record.events(), 2);
        assert!(record.is_open());
        // The periodic reading is the running stop register until the real one arrives.
        assert_eq!(record.meter_stop_wh.unwrap().to_string(), "4500.5");
        assert_eq!(record.energy_wh().unwrap().to_string(), "3500.5");

        // A 2.x `MeterValues` names no transaction and must stay out of the ledger — there is
        // no transaction it could belong to.
        let orphan = observe_v21(&v2_1::CsRequest::MeterValues(
            v2_1::MeterValuesRequest::new(
                1,
                alloc::vec![v2_1::MeterValue::new(
                    alloc::vec![v2_1::SampledValue::new(crate::decimal!(1))],
                    DateTime::parse("2024-01-01T00:30:00Z").unwrap(),
                )],
            ),
        ));
        assert!(to_ledger_event(&identity, &orphan).is_none());
    }

    /// The quietest failure in the stack: a station that *says* it is sending signed meter
    /// data and sends something unparseable looks, through the funnel alone, exactly like a
    /// station sending none — and the operator finds out when a month of sessions turns out
    /// to be unbillable. It is reported rather than dropped.
    #[test]
    fn a_station_that_claims_to_sign_and_does_not_is_reported() {
        let values = alloc::vec![v1_6::MeterValue::new(
            DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            alloc::vec![
                v1_6::SampledValue::new("this is not a JSON document")
                    .with_format(v1_6::ValueFormat::SignedData)
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
                v1_6::SampledValue::new("4500.5")
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
            ],
        )];
        let observed = observe_v16(&v1_6::CsRequest::MeterValues(
            v1_6::MeterValuesRequest::new(1, values),
        ));

        // The good sample still gets through — one broken sample does not cost the message.
        let DomainEvent::MeterValues { energy, signed, .. } = &observed.event else {
            panic!("{:?}", observed.event)
        };
        assert_eq!(energy.as_ref().unwrap().wh.to_string(), "4500.5");
        assert!(signed.is_empty());

        assert_eq!(observed.warnings.len(), 1);
        assert_eq!(observed.warnings[0].kind, WarningKind::UnreadableSignedData);
        assert!(
            observed.warnings[0]
                .to_string()
                .starts_with("unreadable signed data:")
        );
    }

    /// The other two quiet ones: a 1.6 reading that is not a number, and an energy register
    /// in a unit that is not an energy unit. Assuming Wh for the second would be a
    /// factor-1000 error in an invoice, so it is refused — and said out loud.
    #[test]
    fn a_malformed_reading_and_an_impossible_unit_are_both_reported() {
        let values = alloc::vec![v1_6::MeterValue::new(
            DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            alloc::vec![
                v1_6::SampledValue::new("twelve")
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
                v1_6::SampledValue::new("7")
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister)
                    .with_unit(v1_6::UnitOfMeasure::A),
                // Not an energy register, so not this model's business either way.
                v1_6::SampledValue::new("nonsense").with_measurand(v1_6::Measurand::Voltage),
            ],
        )];
        let observed = observe_v16(&v1_6::CsRequest::MeterValues(
            v1_6::MeterValuesRequest::new(1, values),
        ));

        let DomainEvent::MeterValues { energy, .. } = &observed.event else {
            panic!("{:?}", observed.event)
        };
        assert!(energy.is_none(), "neither sample yields a register");
        let kinds: Vec<WarningKind> = observed.warnings.iter().map(|w| w.kind).collect();
        assert_eq!(
            kinds,
            alloc::vec![
                WarningKind::UnreadableReading,
                WarningKind::UnknownEnergyUnit
            ]
        );
        assert_eq!(observed.warnings[0].detail, "twelve");
        assert_eq!(observed.warnings[1].detail, "A");
    }

    /// A conforming message raises nothing. The warnings are worth logging precisely because
    /// they are rare.
    #[test]
    fn a_well_formed_message_warns_about_nothing() {
        let observed = observe_v21(&v2_1::CsRequest::Heartbeat(v2_1::HeartbeatRequest::new()));
        assert!(observed.warnings.is_empty());
    }

    /// `chargingState` is the one fact a meter reading cannot supply, and it decides money: a
    /// register sitting at zero is a taper if the station says `Charging` and an occupancy fee
    /// if it says `SuspendedEV`. It has to survive the funnel, along with the EVSE the
    /// session is at — a CDR names the point it happened at.
    #[test]
    fn the_charging_state_and_the_evse_survive_the_funnel() {
        let request = v2_1::TransactionEventRequest::new(
            v2_1::TransactionEventEnum::Updated,
            DateTime::parse("2024-01-01T00:30:00Z").unwrap(),
            v2_1::TriggerReason::MeterValuePeriodic,
            3,
            v2_1::Transaction::new("tx-1").with_charging_state(v2_1::ChargingState::SuspendedEV),
        )
        .with_evse(v2_1::EVSE::new(2).with_connector_id(1));

        let observed = observe_v21(&v2_1::CsRequest::TransactionEvent(request));
        let DomainEvent::TransactionUpdated {
            charging_state,
            evse_id,
            connector_id,
            ..
        } = &observed.event
        else {
            panic!("{:?}", observed.event)
        };
        assert_eq!(charging_state.as_deref(), Some("SuspendedEV"));
        assert_eq!(*evse_id, Some(2));
        assert_eq!(*connector_id, Some(1));

        // 1.6 has connectors and no EVSEs, and reports that rather than passing one off as
        // the other — a CSMS addressing a 2.x inventory would aim at the wrong outlet.
        let start = observe_v16(&v1_6::CsRequest::StartTransaction(
            v1_6::StartTransactionRequest::new(
                2,
                "CARD-1",
                0,
                DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            ),
        ));
        let DomainEvent::TransactionStarted {
            evse_id,
            connector_id,
            ..
        } = &start.event
        else {
            panic!("{:?}", start.event)
        };
        assert_eq!(*evse_id, None);
        assert_eq!(*connector_id, Some(2));

        // …and the point reaches the ledger, from whichever message named it.
        let identity = crate::types::Identity::new("CS-0001").unwrap();
        let mut ledger = super::super::ledger::Ledger::new();
        ledger.ingest_unsequenced(&to_ledger_event_with_id(&identity, &start, "42").unwrap());
        let record = ledger.transaction(&identity, "42").unwrap();
        assert_eq!(record.connector_id, Some(2));
        assert_eq!(record.evse_id, None);
    }

    /// The Open Charge Alliance's own example message, end to end.
    ///
    /// It is worth reading closely: `meterStop` is `108814` — the meter's **lifetime**
    /// register in Wh — while the signed records beside it report the transaction running
    /// `0.000 → 0.636` kWh. A CSMS billing `meterStop − meterStart` is not billing a slightly
    /// different number, it is billing a different register. Both reach the ledger, and they
    /// stay apart.
    #[test]
    fn the_oca_example_message_reaches_the_ledger_through_the_funnel() {
        const BEGIN: &str = "{\"signedMeterData\": \"T0NNRnx7IlJEIjpbeyJSViI6MC4wMDAsIlJJIjoiMS1iOjEuOC4wIn1dfQ==\", \"encodingMethod\": \"OCMF\", \"publicKey\": \"MzA1OTMwMTMwNjA3MkE4NjQ4Q0UzRDAyMDEwNjA4MkE4NjQ4Q0UzRDAzMDEwNw==\"}";
        const END: &str = "{\"signedMeterData\": \"T0NNRnx7IlJEIjpbeyJSViI6MC42MzYsIlJJIjoiMS1iOjEuOC4wIn1dfQ==\", \"encodingMethod\": \"OCMF\", \"publicKey\": \"MzA1OTMwMTMwNjA3MkE4NjQ4Q0UzRDAyMDEwNjA4MkE4NjQ4Q0UzRDAzMDEwNw==\"}";

        let signed = |document: &str, context| {
            v1_6::SampledValue::new(document)
                .with_format(v1_6::ValueFormat::SignedData)
                .with_measurand(v1_6::Measurand::EnergyActiveImportRegister)
                .with_context(context)
        };
        let stop = v1_6::StopTransactionRequest::new(
            108_814,
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
            42,
        )
        .with_transaction_data(alloc::vec![v1_6::MeterValue::new(
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
            alloc::vec![
                signed(BEGIN, v1_6::ReadingContext::TransactionBegin),
                signed(END, v1_6::ReadingContext::TransactionEnd),
            ],
        )]);

        let identity = crate::types::Identity::new("CS-0001").unwrap();
        let observed = observe_v16(&v1_6::CsRequest::StopTransaction(stop));
        let event = to_ledger_event(&identity, &observed).expect("a stop event");
        let mut ledger = super::super::ledger::Ledger::new();
        ledger.ingest_unsequenced(&event);
        let record = ledger.transaction(&identity, "42").unwrap();

        // 1.6 has no start message to carry a signed record, so both arrive here.
        assert_eq!(record.signed.len(), 2);
        let end = record
            .signed_with_context("Transaction.End")
            .next()
            .expect("the end record");
        assert_eq!(end.value.encoding_method.as_deref(), Some("OCMF"));
        assert_eq!(
            end.measurand.as_deref(),
            Some("Energy.Active.Import.Register")
        );
        // Verbatim: the signature covers these bytes, so nothing re-encoded them.
        assert_eq!(
            end.value.signed_meter_data,
            "T0NNRnx7IlJEIjpbeyJSViI6MC42MzYsIlJJIjoiMS1iOjEuOC4wIn1dfQ=="
        );
        // The key field is Base64 over uppercase hex, the shape the example message uses.
        let key = end.value.public_key().unwrap().unwrap();
        assert_eq!(key.shape, crate::metering::PublicKeyShape::PrintedHex);
        assert_eq!(&key.bytes[..2], &[0x30, 0x59]);

        // …and the protocol's own register is still there, still a different quantity.
        assert_eq!(record.meter_stop_wh.unwrap().to_string(), "108814");
    }

    /// A `SignedData` value is a JSON document, not a measurement. Reading it as one — which
    /// is what the field is for everywhere else — must not produce a number.
    #[test]
    fn a_signed_data_value_is_never_mistaken_for_a_reading() {
        let values = alloc::vec![v1_6::MeterValue::new(
            DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            alloc::vec![
                v1_6::SampledValue::new("{\"signedMeterData\":\"T0NNRnw=\"}")
                    .with_format(v1_6::ValueFormat::SignedData)
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
                v1_6::SampledValue::new("4500.5")
                    .with_measurand(v1_6::Measurand::EnergyActiveImportRegister),
            ],
        )];
        let observed = observe_v16(&v1_6::CsRequest::MeterValues(
            v1_6::MeterValuesRequest::new(1, values),
        ));
        let DomainEvent::MeterValues { energy, signed, .. } = &observed.event else {
            panic!("{:?}", observed.event)
        };
        assert_eq!(energy.as_ref().unwrap().wh.to_string(), "4500.5");
        assert_eq!(signed.len(), 1);
        assert_eq!(signed[0].value.signed_meter_data, "T0NNRnw=");
    }

    /// Where calibration law applies, the signed register is the billing basis. It reaches
    /// the ledger through the same funnel as everything else, untouched.
    #[test]
    fn a_signed_register_reaches_the_ledger_record() {
        let identity = crate::types::Identity::new("CS-0001").unwrap();
        let signed = v2_1::SignedMeterValue::new("BASE64-OCMF", "OCMF")
            .with_public_key("MzA1OQ==")
            .with_signing_method("ECDSA-secp256r1-SHA256");
        let request = v2_1::TransactionEventRequest::new(
            v2_1::TransactionEventEnum::Ended,
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
            v2_1::TriggerReason::StopAuthorized,
            1,
            v2_1::Transaction::new("tx-1"),
        )
        .with_meter_value(alloc::vec![v2_1::MeterValue::new(
            alloc::vec![
                v2_1::SampledValue::new(crate::decimal!(7300.250))
                    .with_measurand(v2_1::Measurand::EnergyActiveImportRegister)
                    .with_context(v2_1::ReadingContext::TransactionEnd)
                    .with_signed_meter_value(signed)
            ],
            DateTime::parse("2024-01-01T01:00:00Z").unwrap(),
        )]);

        let observed = observe_v21(&v2_1::CsRequest::TransactionEvent(request));
        let event = to_ledger_event(&identity, &observed).expect("transaction event");
        let mut ledger = super::super::ledger::Ledger::new();
        ledger.ingest(&event);
        let record = ledger.transaction(&identity, "tx-1").unwrap();
        let signed = record
            .signed_with_context("Transaction.End")
            .next()
            .expect("the signed reading");
        assert_eq!(signed.value.signed_meter_data, "BASE64-OCMF");
        assert_eq!(signed.value.encoding_method.as_deref(), Some("OCMF"));
        assert_eq!(
            signed.value.signing_method.as_deref(),
            Some("ECDSA-secp256r1-SHA256")
        );
        // And the public-key field is readable rather than an opaque string.
        assert_eq!(
            signed.value.public_key_bytes().unwrap().unwrap(),
            alloc::vec![0x30, 0x59]
        );
        assert_eq!(record.meter_stop_wh.unwrap().to_string(), "7300.250");

        // The register on the sample is not the billable value; the record beside it is.
        // Both reach the ledger, and the ledger keeps them apart.
        assert_eq!(record.signed.len(), 1);
    }

    #[test]
    fn a_21_transaction_event_becomes_a_ledger_entry() {
        let identity = crate::types::Identity::new("CS-0001").unwrap();
        let request = v2_1::TransactionEventRequest::new(
            v2_1::TransactionEventEnum::Started,
            DateTime::parse("2024-01-01T00:00:00Z").unwrap(),
            v2_1::TriggerReason::Authorized,
            0,
            v2_1::Transaction::new("tx-1"),
        );
        let observed = observe_v21(&v2_1::CsRequest::TransactionEvent(request));
        let event = to_ledger_event(&identity, &observed).expect("transaction event");
        assert_eq!(event.transaction_id, "tx-1");
        assert_eq!(event.kind, super::super::ledger::EventKind::Started);

        // A heartbeat is not transaction-related, and says so.
        let observed = observe_v21(&v2_1::CsRequest::Heartbeat(v2_1::HeartbeatRequest::new()));
        assert!(to_ledger_event(&identity, &observed).is_none());
    }
}
