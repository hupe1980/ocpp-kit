//! Per-action metadata that the JSON schemas do not carry: which peer originates a
//! message, whether it is a `CALL` or a `SEND`, and which functional block it belongs to.
//!
//! Sources: OCPP 2.1 Part 2 (functional blocks A–S and their use cases), OCPP 2.1 Part 4
//! §4.2.4 (`SEND`), OCPP 1.6 edition 2 (feature profiles) and the 1.6 Security Whitepaper
//! edition 2. Directions were cross-checked against the use-case scenario descriptions in
//! the vendored specification text (see `xtask/src/registry.rs` history).

use crate::model::VersionId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Charging Station → CSMS.
    ChargingStation,
    /// CSMS → Charging Station.
    Csms,
    /// Either peer may originate it (`DataTransfer`).
    Both,
}

impl Origin {
    pub const fn ident(self) -> &'static str {
        match self {
            Origin::ChargingStation => "ChargingStation",
            Origin::Csms => "Csms",
            Origin::Both => "Both",
        }
    }

    /// Whether a Charging Station may originate an action with this origin.
    #[allow(clippy::wrong_self_convention)]
    pub const fn from_cs(self) -> bool {
        matches!(self, Origin::ChargingStation | Origin::Both)
    }

    /// Whether a CSMS may originate an action with this origin.
    #[allow(clippy::wrong_self_convention)]
    pub const fn from_csms(self) -> bool {
        matches!(self, Origin::Csms | Origin::Both)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A `CALL` that is answered with `CALLRESULT` / `CALLERROR`.
    Call,
    /// An unconfirmed `SEND` (message type 6), 2.1 only. Never answered — Part 4 §4.2.4.
    Send,
}

impl Kind {
    pub const fn ident(self) -> &'static str {
        match self {
            Kind::Call => "Call",
            Kind::Send => "Send",
        }
    }
}

pub struct ActionInfo {
    pub action: &'static str,
    pub origin: Origin,
    pub kind: Kind,
    pub block: &'static str,
}

const fn call(action: &'static str, origin: Origin, block: &'static str) -> ActionInfo {
    ActionInfo {
        action,
        origin,
        kind: Kind::Call,
        block,
    }
}

use Origin::{Both, ChargingStation as Cs, Csms};

/// OCPP 1.6 (edition 2) + Security Whitepaper edition 2 — 39 actions, grouped by feature profile.
static V1_6: &[ActionInfo] = &[
    call("Authorize", Cs, "Core"),
    call("BootNotification", Cs, "Core"),
    call("CancelReservation", Csms, "Reservation"),
    call("CertificateSigned", Csms, "Security"),
    call("ChangeAvailability", Csms, "Core"),
    call("ChangeConfiguration", Csms, "Core"),
    call("ClearCache", Csms, "Core"),
    call("ClearChargingProfile", Csms, "SmartCharging"),
    call("DataTransfer", Both, "Core"),
    call("DeleteCertificate", Csms, "Security"),
    call("DiagnosticsStatusNotification", Cs, "FirmwareManagement"),
    call("ExtendedTriggerMessage", Csms, "Security"),
    call("FirmwareStatusNotification", Cs, "FirmwareManagement"),
    call("GetCompositeSchedule", Csms, "SmartCharging"),
    call("GetConfiguration", Csms, "Core"),
    call("GetDiagnostics", Csms, "FirmwareManagement"),
    call("GetInstalledCertificateIds", Csms, "Security"),
    call("GetLocalListVersion", Csms, "LocalAuthListManagement"),
    call("GetLog", Csms, "Security"),
    call("Heartbeat", Cs, "Core"),
    call("InstallCertificate", Csms, "Security"),
    call("LogStatusNotification", Cs, "Security"),
    call("MeterValues", Cs, "Core"),
    call("RemoteStartTransaction", Csms, "Core"),
    call("RemoteStopTransaction", Csms, "Core"),
    call("ReserveNow", Csms, "Reservation"),
    call("Reset", Csms, "Core"),
    call("SecurityEventNotification", Cs, "Security"),
    call("SendLocalList", Csms, "LocalAuthListManagement"),
    call("SetChargingProfile", Csms, "SmartCharging"),
    call("SignCertificate", Cs, "Security"),
    call("SignedFirmwareStatusNotification", Cs, "Security"),
    call("SignedUpdateFirmware", Csms, "Security"),
    call("StartTransaction", Cs, "Core"),
    call("StatusNotification", Cs, "Core"),
    call("StopTransaction", Cs, "Core"),
    call("TriggerMessage", Csms, "RemoteTrigger"),
    call("UnlockConnector", Csms, "Core"),
    call("UpdateFirmware", Csms, "FirmwareManagement"),
];

/// OCPP 2.0.1 and 2.1. The 2.0.1 action set is a strict subset of 2.1's, so one table
/// serves both; applicability is decided by which schema files exist for a version.
static V2X: &[ActionInfo] = &[
    ActionInfo {
        action: "AFRRSignal",
        origin: Csms,
        kind: Kind::Call,
        block: "Q",
    },
    call("AdjustPeriodicEventStream", Csms, "N"),
    call("Authorize", Cs, "C"),
    call("BatterySwap", Cs, "S"),
    call("BootNotification", Cs, "B"),
    call("CancelReservation", Csms, "H"),
    call("CertificateSigned", Csms, "A"),
    call("ChangeAvailability", Csms, "G"),
    call("ChangeTransactionTariff", Csms, "I"),
    call("ClearCache", Csms, "C"),
    call("ClearChargingProfile", Csms, "K"),
    call("ClearDERControl", Csms, "R"),
    call("ClearDisplayMessage", Csms, "O"),
    call("ClearTariffs", Csms, "I"),
    call("ClearVariableMonitoring", Csms, "N"),
    call("ClearedChargingLimit", Cs, "K"),
    call("ClosePeriodicEventStream", Cs, "N"),
    call("CostUpdated", Csms, "I"),
    call("CustomerInformation", Csms, "N"),
    call("DataTransfer", Both, "P"),
    call("DeleteCertificate", Csms, "M"),
    call("FirmwareStatusNotification", Cs, "L"),
    call("Get15118EVCertificate", Cs, "M"),
    call("GetBaseReport", Csms, "B"),
    call("GetCertificateChainStatus", Cs, "M"),
    call("GetCertificateStatus", Cs, "M"),
    call("GetChargingProfiles", Csms, "K"),
    call("GetCompositeSchedule", Csms, "K"),
    call("GetDERControl", Csms, "R"),
    call("GetDisplayMessages", Csms, "O"),
    call("GetInstalledCertificateIds", Csms, "M"),
    call("GetLocalListVersion", Csms, "D"),
    call("GetLog", Csms, "N"),
    call("GetMonitoringReport", Csms, "N"),
    call("GetPeriodicEventStream", Csms, "N"),
    call("GetReport", Csms, "B"),
    call("GetTariffs", Csms, "I"),
    call("GetTransactionStatus", Csms, "E"),
    call("GetVariables", Csms, "B"),
    call("Heartbeat", Cs, "B"),
    call("InstallCertificate", Csms, "M"),
    call("LogStatusNotification", Cs, "N"),
    call("MeterValues", Cs, "J"),
    call("NotifyAllowedEnergyTransfer", Csms, "Q"),
    call("NotifyChargingLimit", Cs, "K"),
    call("NotifyCustomerInformation", Cs, "N"),
    call("NotifyDERAlarm", Cs, "R"),
    call("NotifyDERStartStop", Cs, "R"),
    call("NotifyDisplayMessages", Cs, "O"),
    call("NotifyEVChargingNeeds", Cs, "K"),
    call("NotifyEVChargingSchedule", Cs, "K"),
    call("NotifyEvent", Cs, "N"),
    call("NotifyMonitoringReport", Cs, "N"),
    // N15.FR.01 — the only `SEND`-only action in OCPP 2.1; it has no response schema.
    ActionInfo {
        action: "NotifyPeriodicEventStream",
        origin: Cs,
        kind: Kind::Send,
        block: "N",
    },
    call("NotifyPriorityCharging", Cs, "K"),
    call("NotifyReport", Cs, "B"),
    call("NotifySettlement", Cs, "I"),
    call("NotifyWebPaymentStarted", Cs, "I"),
    call("OpenPeriodicEventStream", Cs, "N"),
    call("PublishFirmware", Csms, "L"),
    call("PublishFirmwareStatusNotification", Cs, "L"),
    call("PullDynamicScheduleUpdate", Cs, "K"),
    call("ReportChargingProfiles", Cs, "K"),
    call("ReportDERControl", Cs, "R"),
    call("RequestBatterySwap", Csms, "S"),
    call("RequestStartTransaction", Csms, "F"),
    call("RequestStopTransaction", Csms, "F"),
    call("ReservationStatusUpdate", Cs, "H"),
    call("ReserveNow", Csms, "H"),
    call("Reset", Csms, "B"),
    call("SecurityEventNotification", Cs, "A"),
    call("SendLocalList", Csms, "D"),
    call("SetChargingProfile", Csms, "K"),
    call("SetDERControl", Csms, "R"),
    call("SetDefaultTariff", Csms, "I"),
    call("SetDisplayMessage", Csms, "O"),
    call("SetMonitoringBase", Csms, "N"),
    call("SetMonitoringLevel", Csms, "N"),
    call("SetNetworkProfile", Csms, "B"),
    call("SetVariableMonitoring", Csms, "N"),
    call("SetVariables", Csms, "B"),
    call("SignCertificate", Cs, "A"),
    call("StatusNotification", Cs, "G"),
    call("TransactionEvent", Cs, "E"),
    call("TriggerMessage", Csms, "F"),
    call("UnlockConnector", Csms, "F"),
    call("UnpublishFirmware", Csms, "L"),
    call("UpdateDynamicSchedule", Csms, "K"),
    call("UpdateFirmware", Csms, "L"),
    call("UsePriorityCharging", Csms, "K"),
    call("VatNumberValidation", Cs, "I"),
];

pub fn table(version: VersionId) -> &'static [ActionInfo] {
    match version {
        VersionId::V1_6 => V1_6,
        VersionId::V2_0_1 | VersionId::V2_1 => V2X,
    }
}

/// Human-readable name of a functional block / feature profile, for generated docs.
pub fn block_name(version: VersionId, block: &str) -> &'static str {
    if version == VersionId::V1_6 {
        return match block {
            "Core" => "Core",
            "FirmwareManagement" => "Firmware Management",
            "LocalAuthListManagement" => "Local Auth List Management",
            "Reservation" => "Reservation",
            "SmartCharging" => "Smart Charging",
            "RemoteTrigger" => "Remote Trigger",
            "Security" => "Security (Whitepaper ed. 2)",
            _ => "Unknown",
        };
    }
    match block {
        "A" => "A — Security",
        "B" => "B — Provisioning",
        "C" => "C — Authorization",
        "D" => "D — Local Authorization List Management",
        "E" => "E — Transactions",
        "F" => "F — Remote Control",
        "G" => "G — Availability",
        "H" => "H — Reservation",
        "I" => "I — Tariff and Cost",
        "J" => "J — Meter Values",
        "K" => "K — Smart Charging",
        "L" => "L — Firmware Management",
        "M" => "M — Certificate Management",
        "N" => "N — Diagnostics",
        "O" => "O — Display Message",
        "P" => "P — Data Transfer",
        "Q" => "Q — Bidirectional Power Transfer",
        "R" => "R — DER Control",
        "S" => "S — Battery Swapping",
        _ => "Unknown",
    }
}
