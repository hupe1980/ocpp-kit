//! The OCPP 2.0.1 certification profiles (Part 5, FINAL 2023-06-30).
//!
//! A certification profile is a set of use cases the Open Charge Alliance will certify
//! together. "Are we Core-certifiable?" is a question a project should be able to ask its
//! own test suite, and this table is what makes that possible: each profile names the
//! actions it exercises and the controller components Part 5 §5 makes mandatory for it.
//!
//! The actions are derived from the use cases in Part 5 Table 1; the controller components
//! are Part 5 §5 verbatim. Certification itself is a downstream activity with a test lab —
//! what this gives you is the *coverage question*, answerable in CI.

/// One certification profile.
pub struct Profile {
    /// The name Part 5 uses.
    pub name: &'static str,
    /// A short slug for the command line.
    pub slug: &'static str,
    /// The actions the profile's use cases exercise.
    pub actions: &'static [&'static str],
    /// The controller components Part 5 §5 makes mandatory (2.x only).
    pub components: &'static [&'static str],
}

/// Every certification profile, Core first.
pub static PROFILES: &[Profile] = &[
    Profile {
        name: "Core",
        slug: "core",
        actions: &[
            // Booting, configuring and resetting.
            "BootNotification",
            "Heartbeat",
            "GetBaseReport",
            "GetReport",
            "NotifyReport",
            "GetVariables",
            "SetVariables",
            "SetNetworkProfile",
            "Reset",
            "ChangeAvailability",
            "StatusNotification",
            // Authorization and transactions.
            "Authorize",
            "TransactionEvent",
            "GetTransactionStatus",
            "ClearCache",
            // Remote control.
            "RequestStartTransaction",
            "RequestStopTransaction",
            "UnlockConnector",
            "TriggerMessage",
            // Metering.
            "MeterValues",
            // Certificates and security.
            "InstallCertificate",
            "GetInstalledCertificateIds",
            "DeleteCertificate",
            "SecurityEventNotification",
            // Diagnostics and firmware.
            "GetLog",
            "LogStatusNotification",
            "CustomerInformation",
            "NotifyCustomerInformation",
            "UpdateFirmware",
            "FirmwareStatusNotification",
            "PublishFirmware",
            "PublishFirmwareStatusNotification",
            "UnpublishFirmware",
            // Data transfer is always available.
            "DataTransfer",
        ],
        components: &[
            "OCPPCommCtrlr",
            "TxCtrlr",
            "DeviceDataCtrlr",
            "ClockCtrlr",
            "SecurityCtrlr",
            "SampledDataCtrlr",
            "AlignedDataCtrlr",
            "AuthCtrlr",
        ],
    },
    Profile {
        name: "Advanced Security",
        slug: "advanced-security",
        actions: &[
            "SignCertificate",
            "CertificateSigned",
            "SecurityEventNotification",
        ],
        components: &["SecurityCtrlr"],
    },
    Profile {
        name: "Local Authorization List Management",
        slug: "local-auth-list",
        actions: &["SendLocalList", "GetLocalListVersion", "Authorize"],
        components: &["LocalAuthListCtrlr"],
    },
    Profile {
        name: "Smart Charging",
        slug: "smart-charging",
        actions: &[
            "SetChargingProfile",
            "GetChargingProfiles",
            "ReportChargingProfiles",
            "ClearChargingProfile",
            "GetCompositeSchedule",
            "NotifyChargingLimit",
            "ClearedChargingLimit",
            "RequestStartTransaction",
        ],
        components: &["SmartChargingCtrlr"],
    },
    Profile {
        name: "Advanced Device Management",
        slug: "advanced-device-management",
        actions: &[
            "GetMonitoringReport",
            "NotifyMonitoringReport",
            "SetMonitoringBase",
            "SetMonitoringLevel",
            "SetVariableMonitoring",
            "ClearVariableMonitoring",
            "GetReport",
            "NotifyEvent",
        ],
        components: &["MonitoringCtrlr"],
    },
    Profile {
        name: "Advanced User Interface",
        slug: "advanced-user-interface",
        actions: &[
            "SetDisplayMessage",
            "GetDisplayMessages",
            "NotifyDisplayMessages",
            "ClearDisplayMessage",
            "CostUpdated",
        ],
        components: &["TariffCostCtrlr", "DisplayMessageCtrlr"],
    },
    Profile {
        name: "Reservation",
        slug: "reservation",
        actions: &["ReserveNow", "CancelReservation", "ReservationStatusUpdate"],
        components: &["ReservationCtrlr"],
    },
    Profile {
        name: "ISO 15118 support",
        slug: "iso15118",
        actions: &[
            "Get15118EVCertificate",
            "GetCertificateStatus",
            "InstallCertificate",
            "GetInstalledCertificateIds",
            "DeleteCertificate",
            "SignCertificate",
            "CertificateSigned",
            "Authorize",
            "SetChargingProfile",
            "GetCompositeSchedule",
            "NotifyEVChargingNeeds",
            "NotifyEVChargingSchedule",
        ],
        components: &["ISO15118Ctrlr", "SmartChargingCtrlr"],
    },
];

/// Looks a profile up by slug or by name, case-insensitively.
pub fn find(name: &str) -> Option<&'static Profile> {
    let wanted = name.to_ascii_lowercase();
    PROFILES
        .iter()
        .find(|profile| profile.slug == wanted || profile.name.to_ascii_lowercase() == wanted)
}
