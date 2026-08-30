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

use crate::types::DateTime;
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
        /// The event's position in the transaction; 1.6 has none.
        seq_no: Option<i32>,
        /// The authorizing token.
        id_token: Option<String>,
        /// The energy register at the start, in Wh.
        meter_start_wh: Option<f64>,
        /// When it started.
        timestamp: DateTime,
    },
    /// Something changed during a transaction.
    TransactionUpdated {
        /// The transaction id.
        transaction_id: String,
        /// The event's position in the transaction; 1.6 has none.
        seq_no: Option<i32>,
        /// The energy register, in Wh.
        meter_wh: Option<f64>,
        /// Whether the station was offline when it happened.
        offline: bool,
        /// When it happened.
        timestamp: DateTime,
    },
    /// A transaction finished.
    TransactionEnded {
        /// The transaction id.
        transaction_id: String,
        /// The event's position in the transaction; 1.6 has none.
        seq_no: Option<i32>,
        /// Why it stopped.
        stopped_reason: Option<String>,
        /// The energy register at the end, in Wh.
        meter_stop_wh: Option<f64>,
        /// When it ended.
        timestamp: DateTime,
    },
    /// Metering samples arrived outside a transaction event.
    MeterValues {
        /// The EVSE (2.x) or connector (1.6).
        evse_id: Option<i32>,
        /// The transaction, when the samples belong to one.
        transaction_id: Option<String>,
        /// How many samples arrived.
        samples: usize,
        /// The energy register, in Wh, when one of the samples carried it.
        energy_wh: Option<f64>,
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

/// A domain event together with the version it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    /// The version-neutral view.
    pub event: DomainEvent,
    /// Which version produced it, so a handler can reach for the right typed original.
    pub version: Version,
}

impl Observed {
    // Only the per-version conversions build one, and each is behind its version's feature.
    #[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
    fn new(version: Version, event: DomainEvent) -> Self {
        Self { event, version }
    }
}

/// Extracts the energy register, in Wh, from a set of sampled values.
///
/// Both versions report `Energy.Active.Import.Register` by default, and both allow the unit
/// to be `kWh`, which is a routine source of factor-1000 bugs.
#[cfg(any(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]
fn energy_from_samples<'a>(
    samples: impl Iterator<Item = (Option<&'a str>, Option<&'a str>, f64)>,
) -> Option<f64> {
    for (measurand, unit, value) in samples {
        let measurand = measurand.unwrap_or("Energy.Active.Import.Register");
        if measurand != "Energy.Active.Import.Register" {
            continue;
        }
        return Some(match unit {
            Some("kWh") => value * 1000.0,
            _ => value,
        });
    }
    None
}

#[cfg(feature = "v1_6")]
mod v16_conversion {
    use super::{DomainEvent, Observed, energy_from_samples};
    use crate::v1_6;
    use crate::version::Version;
    use alloc::string::ToString;

    /// Maps an OCPP 1.6 station-originated request onto the common model.
    #[must_use]
    pub fn observe(request: &v1_6::CsRequest) -> Observed {
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
                seq_no: None,
                id_token: Some(start.id_tag.clone()),
                meter_start_wh: Some(f64::from(start.meter_start)),
                timestamp: start.timestamp,
            },
            v1_6::CsRequest::StopTransaction(stop) => DomainEvent::TransactionEnded {
                transaction_id: stop.transaction_id.to_string(),
                seq_no: None,
                stopped_reason: stop
                    .reason
                    .as_ref()
                    .map(|reason| reason.as_str().to_string()),
                meter_stop_wh: Some(f64::from(stop.meter_stop)),
                timestamp: stop.timestamp,
            },
            v1_6::CsRequest::MeterValues(values) => {
                let samples: usize = values
                    .meter_value
                    .iter()
                    .map(|entry| entry.sampled_value.len())
                    .sum();
                DomainEvent::MeterValues {
                    evse_id: Some(values.connector_id),
                    transaction_id: values.transaction_id.map(|id| id.to_string()),
                    samples,
                    energy_wh: energy_from_samples(values.meter_value.iter().flat_map(|entry| {
                        entry.sampled_value.iter().map(|sample| {
                            (
                                sample.measurand.as_ref().map(v1_6::Measurand::as_str),
                                sample.unit.as_ref().map(v1_6::UnitOfMeasure::as_str),
                                sample.value.parse::<f64>().unwrap_or(f64::NAN),
                            )
                        })
                    })),
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
        Observed::new(Version::V1_6, event)
    }
}

#[cfg(feature = "v1_6")]
pub use v16_conversion::observe as observe_v16;

/// Generates the 2.x conversions, which differ only in the module they name.
#[cfg(any(feature = "v2_0_1", feature = "v2_1"))]
macro_rules! observe_2x {
    ($module:ident, $version:expr, $name:ident) => {
        /// Maps a station-originated request onto the common model.
        #[must_use]
        #[allow(clippy::too_many_lines)]
        pub fn $name(request: &crate::$module::CsRequest) -> Observed {
            use crate::$module as v;
            use alloc::string::ToString as _;
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
                    let energy =
                        energy_from_samples(event.meter_value.iter().flatten().flat_map(|entry| {
                            entry.sampled_value.iter().map(|sample| {
                                (
                                    sample.measurand.as_ref().map(v::Measurand::as_str),
                                    sample
                                        .unit_of_measure
                                        .as_ref()
                                        .and_then(|unit| unit.unit.as_deref()),
                                    sample.value.get(),
                                )
                            })
                        }));
                    match event.event_type {
                        v::TransactionEventEnum::Started => DomainEvent::TransactionStarted {
                            transaction_id: Some(event.transaction_info.transaction_id.clone()),
                            seq_no: Some(event.seq_no),
                            id_token: event.id_token.as_ref().map(|token| token.id_token.clone()),
                            meter_start_wh: energy,
                            timestamp: event.timestamp,
                        },
                        v::TransactionEventEnum::Ended => DomainEvent::TransactionEnded {
                            transaction_id: event.transaction_info.transaction_id.clone(),
                            seq_no: Some(event.seq_no),
                            stopped_reason: event
                                .transaction_info
                                .stopped_reason
                                .as_ref()
                                .map(|reason| reason.as_str().to_string()),
                            meter_stop_wh: energy,
                            timestamp: event.timestamp,
                        },
                        _ => DomainEvent::TransactionUpdated {
                            transaction_id: event.transaction_info.transaction_id.clone(),
                            seq_no: Some(event.seq_no),
                            meter_wh: energy,
                            offline: event.offline.unwrap_or(false),
                            timestamp: event.timestamp,
                        },
                    }
                }
                v::CsRequest::MeterValues(values) => {
                    let samples: usize = values
                        .meter_value
                        .iter()
                        .map(|entry| entry.sampled_value.len())
                        .sum();
                    DomainEvent::MeterValues {
                        evse_id: Some(values.evse_id),
                        transaction_id: None,
                        samples,
                        energy_wh: energy_from_samples(values.meter_value.iter().flat_map(
                            |entry| {
                                entry.sampled_value.iter().map(|sample| {
                                    (
                                        sample.measurand.as_ref().map(v::Measurand::as_str),
                                        sample
                                            .unit_of_measure
                                            .as_ref()
                                            .and_then(|unit| unit.unit.as_deref()),
                                        sample.value.get(),
                                    )
                                })
                            },
                        )),
                    }
                }
                v::CsRequest::SecurityEventNotification(security) => DomainEvent::SecurityEvent {
                    event_type: security.r#type.clone(),
                    timestamp: security.timestamp,
                    info: security.tech_info.clone(),
                },
                v::CsRequest::FirmwareStatusNotification(status) => DomainEvent::FirmwareStatus {
                    status: status.status.as_str().to_string(),
                },
                other => DomainEvent::Other {
                    action: other.action().as_str().to_string(),
                },
            };
            Observed::new($version, event)
        }
    };
}

#[cfg(feature = "v2_0_1")]
observe_2x!(v2_0_1, Version::V2_0_1, observe_v201);
#[cfg(feature = "v2_1")]
observe_2x!(v2_1, Version::V2_1, observe_v21);

/// Turns a domain event into the ledger's neutral shape, when it is transaction-related.
///
/// The bridge between [`DomainEvent`] and [`Ledger`](super::ledger::Ledger): one call, and a
/// CSMS has an idempotent transaction record regardless of which version the station speaks.
#[must_use]
pub fn to_ledger_event(
    identity: &crate::types::Identity,
    observed: &Observed,
) -> Option<super::ledger::TransactionEvent> {
    use super::ledger::{EventKind, TransactionEvent};
    let event = match &observed.event {
        DomainEvent::TransactionStarted {
            transaction_id,
            seq_no,
            id_token,
            meter_start_wh,
            timestamp,
        } => {
            let mut event = TransactionEvent::new(
                identity.clone(),
                transaction_id.clone()?,
                seq_no.unwrap_or(0),
                EventKind::Started,
                *timestamp,
            );
            event.id_token.clone_from(id_token);
            event.meter_wh = *meter_start_wh;
            event
        }
        DomainEvent::TransactionUpdated {
            transaction_id,
            seq_no,
            meter_wh,
            offline,
            timestamp,
        } => {
            let mut event = TransactionEvent::new(
                identity.clone(),
                transaction_id.clone(),
                seq_no.unwrap_or(0),
                EventKind::Updated,
                *timestamp,
            );
            event.meter_wh = *meter_wh;
            event.offline = *offline;
            event
        }
        DomainEvent::TransactionEnded {
            transaction_id,
            seq_no,
            stopped_reason,
            meter_stop_wh,
            timestamp,
        } => {
            let mut event = TransactionEvent::new(
                identity.clone(),
                transaction_id.clone(),
                seq_no.unwrap_or(0),
                EventKind::Ended,
                *timestamp,
            );
            event.stopped_reason.clone_from(stopped_reason);
            event.meter_wh = *meter_stop_wh;
            event
        }
        _ => return None,
    };
    Some(event)
}

/// Every action that maps to something other than [`DomainEvent::Other`].
#[must_use]
pub fn covered_actions() -> Vec<&'static str> {
    alloc::vec![
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
    ]
}

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
