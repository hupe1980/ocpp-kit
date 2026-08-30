//! Version-aware action metadata, without generics.
//!
//! The engine works at the frame level — action *names*, not typed payloads — so that one
//! implementation serves every version. It still needs to know what an action name means:
//! whether it is a `CALL` or a `SEND`, which peer may originate it, and whether it is
//! transaction-related (and therefore subject to the retry and offline-queue rules). These
//! lookups provide exactly that, backed by the generated per-version tables.

use crate::message::{MessageKind, Origin};
use crate::version::Version;

/// Whether `version` defines `action` at all.
#[must_use]
pub fn is_known(version: Version, action: &str) -> bool {
    kind(version, action).is_some()
}

/// Whether the action is a `CALL` or an unconfirmed `SEND`.
///
/// `None` when this version does not define the action.
#[must_use]
pub fn kind(version: Version, action: &str) -> Option<MessageKind> {
    // Every arm is feature-gated; with no version enabled, nothing is known.
    let _ = action;
    match version {
        #[cfg(feature = "v1_6")]
        Version::V1_6 => crate::v1_6::Action::from_wire(action).map(crate::v1_6::Action::kind),
        #[cfg(feature = "v2_0_1")]
        Version::V2_0_1 => {
            crate::v2_0_1::Action::from_wire(action).map(crate::v2_0_1::Action::kind)
        }
        #[cfg(feature = "v2_1")]
        Version::V2_1 => crate::v2_1::Action::from_wire(action).map(crate::v2_1::Action::kind),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Which peer may originate the action.
///
/// `None` when this version does not define the action.
#[must_use]
pub fn origin(version: Version, action: &str) -> Option<Origin> {
    let _ = action;
    match version {
        #[cfg(feature = "v1_6")]
        Version::V1_6 => crate::v1_6::Action::from_wire(action).map(crate::v1_6::Action::origin),
        #[cfg(feature = "v2_0_1")]
        Version::V2_0_1 => {
            crate::v2_0_1::Action::from_wire(action).map(crate::v2_0_1::Action::origin)
        }
        #[cfg(feature = "v2_1")]
        Version::V2_1 => crate::v2_1::Action::from_wire(action).map(crate::v2_1::Action::origin),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Whether the action is transaction-related, and therefore
///
/// * queued durably while offline and replayed in order on reconnect
///   (E04.FR.01–03, E08.FR.05–07, E12.FR.01–02), and
/// * retried on the linear schedule the specification prescribes
///   (1.6 §3.7.1; 2.x `OCPPCommCtrlr.MessageAttempts[TransactionEvent]`).
///
/// No other message is ever retried by the engine.
#[must_use]
pub fn is_transaction_related(version: Version, action: &str) -> bool {
    match version {
        // 1.6 §3.7.1 names these three explicitly.
        Version::V1_6 => {
            matches!(
                action,
                "StartTransaction" | "StopTransaction" | "MeterValues"
            )
        }
        // 2.x folded all three into TransactionEvent.
        Version::V2_0_1 | Version::V2_1 => action == "TransactionEvent",
    }
}

/// The `CALL` a Charging Station is expected to send after the CSMS sent `requested`.
///
/// Used by the CSMS side of the boot state machine: B02.FR.09 says an *unrequested* `CALL`
/// from a station that is still `Pending` must be answered with `SecurityError`, so the
/// engine has to know which follow-up calls it asked for.
#[must_use]
pub fn solicited_by(version: Version, requested: &str) -> &'static [&'static str] {
    let _ = version;
    match requested {
        "GetBaseReport" | "GetReport" => &["NotifyReport"],
        "GetMonitoringReport" => &["NotifyMonitoringReport"],
        "CustomerInformation" => &["NotifyCustomerInformation"],
        "GetDisplayMessages" => &["NotifyDisplayMessages"],
        "GetChargingProfiles" => &["ReportChargingProfiles"],
        "GetDERControl" => &["ReportDERControl"],
        "GetLog" => &["LogStatusNotification"],
        "UpdateFirmware" | "SignedUpdateFirmware" => &[
            "FirmwareStatusNotification",
            "SignedFirmwareStatusNotification",
        ],
        "PublishFirmware" => &["PublishFirmwareStatusNotification"],
        "GetDiagnostics" => &["DiagnosticsStatusNotification"],
        _ => &[],
    }
}
