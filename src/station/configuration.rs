//! OCPP 1.6 configuration keys (`GetConfiguration` / `ChangeConfiguration`).
//!
//! 1.6 has no device model — it has a flat list of string-valued keys, each of which is
//! either read-only or writable and may require a reboot before a change takes effect. The
//! specification names them, gives each a type, and marks which belong to which feature
//! profile; this registry carries that table so a station does not have to re-derive it, and
//! so `ChangeConfiguration` can answer `Rejected`, `RebootRequired` or `NotSupported`
//! correctly instead of always answering `Accepted`.
//!
//! ```
//! use ocpp_kit::station::configuration::{ConfigurationKeys, ConfigurationStatus};
//!
//! let mut config = ConfigurationKeys::with_defaults();
//! assert_eq!(config.set("HeartbeatInterval", "600"), ConfigurationStatus::Accepted);
//! // Read-only keys are refused, not silently ignored.
//! assert_eq!(config.set("NumberOfConnectors", "4"), ConfigurationStatus::Rejected);
//! // …and a key this station does not implement says so.
//! assert_eq!(config.set("WhateverKey", "1"), ConfigurationStatus::NotSupported);
//! ```

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// What `ChangeConfiguration` answers (`ConfigurationStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigurationStatus {
    /// Stored and in effect.
    Accepted,
    /// The key is read-only, or the value does not fit its type.
    Rejected,
    /// Stored, but only in effect after a reboot.
    RebootRequired,
    /// This station does not implement the key.
    NotSupported,
}

/// The type a key's value has to parse as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueType {
    /// Free text.
    Text,
    /// A whole number.
    Integer,
    /// `true` or `false`.
    Boolean,
    /// A comma-separated list.
    CsvList,
}

impl ValueType {
    /// Whether `value` is a valid instance of this type.
    #[must_use]
    pub fn accepts(self, value: &str) -> bool {
        match self {
            ValueType::Text | ValueType::CsvList => true,
            ValueType::Integer => value.parse::<i64>().is_ok(),
            // 1.6 §Configuration: booleans are the strings "true" and "false".
            ValueType::Boolean => matches!(value, "true" | "false"),
        }
    }
}

/// How a key behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Reported, never written.
    ReadOnly,
    /// Writable, in effect immediately.
    ReadWrite,
    /// Writable, in effect after a reboot.
    RebootRequired,
}

/// One configuration key.
#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    /// The key name, exactly as 1.6 spells it.
    pub name: &'static str,
    /// Whether and how it can be written.
    pub access: Access,
    /// The type of its value.
    pub value_type: ValueType,
    /// The feature profile it belongs to.
    pub profile: &'static str,
    /// The current value. `None` means "implemented but unset", which
    /// `GetConfiguration` reports as a key with no value.
    pub value: Option<String>,
}

/// The registry of a station's 1.6 configuration keys.
#[derive(Clone, Debug, Default)]
pub struct ConfigurationKeys {
    keys: BTreeMap<&'static str, Key>,
}

impl ConfigurationKeys {
    /// An empty registry — a station that implements no keys at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The standard keys, with the specification's own defaults where it gives one.
    ///
    /// Declare only what the station really implements: `GetConfiguration` reports every
    /// key it is asked about that is *not* in the registry under `unknownKey`, and that list
    /// is how a CSMS discovers what a station supports.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut keys = BTreeMap::new();
        for key in defaults() {
            keys.insert(key.name, key);
        }
        Self { keys }
    }

    /// Declares (or replaces) a key.
    pub fn declare(&mut self, key: Key) {
        self.keys.insert(key.name, key);
    }

    /// Removes a key, so the station reports it as unknown.
    pub fn remove(&mut self, name: &str) {
        self.keys.retain(|key, _| *key != name);
    }

    /// How many keys are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Reads a key's value.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Key> {
        self.keys.get(name)
    }

    /// Applies a `ChangeConfiguration`.
    pub fn set(&mut self, name: &str, value: &str) -> ConfigurationStatus {
        let Some(key) = self.keys.get_mut(name) else {
            return ConfigurationStatus::NotSupported;
        };
        if key.access == Access::ReadOnly || !key.value_type.accepts(value) {
            return ConfigurationStatus::Rejected;
        }
        key.value = Some(value.to_owned());
        if key.access == Access::RebootRequired {
            ConfigurationStatus::RebootRequired
        } else {
            ConfigurationStatus::Accepted
        }
    }

    /// Answers a `GetConfiguration`.
    ///
    /// With no `requested` keys, every declared key is returned. Otherwise the result is
    /// split into the keys that are known and the names that are not — which is exactly the
    /// `configurationKey` / `unknownKey` split the response has.
    #[must_use]
    pub fn report<'a>(&'a self, requested: &[&str]) -> (Vec<&'a Key>, Vec<String>) {
        if requested.is_empty() {
            return (self.keys.values().collect(), Vec::new());
        }
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for name in requested {
            match self.keys.get(*name) {
                Some(key) => known.push(key),
                None => unknown.push((*name).to_string()),
            }
        }
        (known, unknown)
    }
}

const fn key(
    name: &'static str,
    access: Access,
    value_type: ValueType,
    profile: &'static str,
) -> Key {
    Key {
        name,
        access,
        value_type,
        profile,
        value: None,
    }
}

fn with(mut key: Key, value: &str) -> Key {
    key.value = Some(value.to_owned());
    key
}

/// The standard 1.6 configuration keys, by feature profile.
#[allow(clippy::too_many_lines)]
fn defaults() -> Vec<Key> {
    use Access::{ReadOnly, ReadWrite, RebootRequired};
    use ValueType::{Boolean, CsvList, Integer, Text};
    alloc::vec![
        // --- Core ------------------------------------------------------------
        with(key("HeartbeatInterval", ReadWrite, Integer, "Core"), "300"),
        with(
            key("AllowOfflineTxForUnknownId", ReadWrite, Boolean, "Core"),
            "false"
        ),
        with(
            key("AuthorizationCacheEnabled", ReadWrite, Boolean, "Core"),
            "true"
        ),
        with(
            key("AuthorizeRemoteTxRequests", ReadOnly, Boolean, "Core"),
            "false"
        ),
        with(
            key("ClockAlignedDataInterval", ReadWrite, Integer, "Core"),
            "0"
        ),
        with(key("ConnectionTimeOut", ReadWrite, Integer, "Core"), "60"),
        with(
            key("ConnectorPhaseRotation", ReadWrite, CsvList, "Core"),
            "Unknown"
        ),
        with(
            key("GetConfigurationMaxKeys", ReadOnly, Integer, "Core"),
            "50"
        ),
        with(
            key("LocalAuthorizeOffline", ReadWrite, Boolean, "Core"),
            "true"
        ),
        with(
            key("LocalPreAuthorize", ReadWrite, Boolean, "Core"),
            "false"
        ),
        with(
            key("MeterValuesAlignedData", ReadWrite, CsvList, "Core"),
            ""
        ),
        with(
            key("MeterValuesSampledData", ReadWrite, CsvList, "Core"),
            "Energy.Active.Import.Register",
        ),
        with(
            key("MeterValueSampleInterval", ReadWrite, Integer, "Core"),
            "60"
        ),
        with(key("NumberOfConnectors", ReadOnly, Integer, "Core"), "1"),
        with(key("ResetRetries", ReadWrite, Integer, "Core"), "3"),
        with(
            key(
                "StopTransactionOnEVSideDisconnect",
                ReadWrite,
                Boolean,
                "Core"
            ),
            "true"
        ),
        with(
            key("StopTransactionOnInvalidId", ReadWrite, Boolean, "Core"),
            "true"
        ),
        with(key("StopTxnAlignedData", ReadWrite, CsvList, "Core"), ""),
        with(key("StopTxnSampledData", ReadWrite, CsvList, "Core"), ""),
        with(
            key("SupportedFeatureProfiles", ReadOnly, CsvList, "Core"),
            "Core,FirmwareManagement,LocalAuthListManagement,Reservation,SmartCharging,RemoteTrigger",
        ),
        with(
            key("TransactionMessageAttempts", ReadWrite, Integer, "Core"),
            "3"
        ),
        with(
            key(
                "TransactionMessageRetryInterval",
                ReadWrite,
                Integer,
                "Core"
            ),
            "60"
        ),
        with(
            key(
                "UnlockConnectorOnEVSideDisconnect",
                ReadWrite,
                Boolean,
                "Core"
            ),
            "true"
        ),
        with(
            key("WebSocketPingInterval", ReadWrite, Integer, "Core"),
            "60"
        ),
        // --- Local authorization list ---------------------------------------
        with(
            key(
                "LocalAuthListEnabled",
                ReadWrite,
                Boolean,
                "LocalAuthListManagement"
            ),
            "true"
        ),
        with(
            key(
                "LocalAuthListMaxLength",
                ReadOnly,
                Integer,
                "LocalAuthListManagement"
            ),
            "0"
        ),
        with(
            key(
                "SendLocalListMaxLength",
                ReadOnly,
                Integer,
                "LocalAuthListManagement"
            ),
            "0"
        ),
        // --- Reservation ------------------------------------------------------
        with(
            key(
                "ReserveConnectorZeroSupported",
                ReadOnly,
                Boolean,
                "Reservation"
            ),
            "false"
        ),
        // --- Smart charging ---------------------------------------------------
        with(
            key(
                "ChargeProfileMaxStackLevel",
                ReadOnly,
                Integer,
                "SmartCharging"
            ),
            "3"
        ),
        with(
            key(
                "ChargingScheduleAllowedChargingRateUnit",
                ReadOnly,
                CsvList,
                "SmartCharging"
            ),
            "Current,Power",
        ),
        with(
            key(
                "ChargingScheduleMaxPeriods",
                ReadOnly,
                Integer,
                "SmartCharging"
            ),
            "10"
        ),
        with(
            key(
                "MaxChargingProfilesInstalled",
                ReadOnly,
                Integer,
                "SmartCharging"
            ),
            "10"
        ),
        // --- Security (Whitepaper edition 2) ---------------------------------
        // Write-only in the specification: the key is set, never read back.
        key("AuthorizationKey", ReadWrite, Text, "Security"),
        with(key("SecurityProfile", ReadWrite, Integer, "Security"), "1"),
        with(
            key(
                "AdditionalRootCertificateCheck",
                ReadOnly,
                Boolean,
                "Security"
            ),
            "false"
        ),
        with(
            key(
                "CertificateSignedMaxChainSize",
                ReadOnly,
                Integer,
                "Security"
            ),
            "0"
        ),
        with(
            key("CertificateStoreMaxLength", ReadOnly, Integer, "Security"),
            "0"
        ),
        key("CpoName", ReadWrite, Text, "Security"),
        // --- Connection profile (RebootRequired in most implementations) ------
        with(key("ConnectionUrl", RebootRequired, Text, "Core"), ""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_checked_against_the_keys_type_and_access() {
        let mut config = ConfigurationKeys::with_defaults();
        assert_eq!(
            config.set("HeartbeatInterval", "600"),
            ConfigurationStatus::Accepted
        );
        assert_eq!(
            config.get("HeartbeatInterval").unwrap().value.as_deref(),
            Some("600")
        );

        assert_eq!(
            config.set("HeartbeatInterval", "soon"),
            ConfigurationStatus::Rejected
        );
        assert_eq!(
            config.set("NumberOfConnectors", "4"),
            ConfigurationStatus::Rejected
        );
        assert_eq!(
            config.set("LocalAuthListEnabled", "yes"),
            ConfigurationStatus::Rejected
        );
        assert_eq!(
            config.set("LocalAuthListEnabled", "false"),
            ConfigurationStatus::Accepted
        );
        assert_eq!(
            config.set("Nonexistent", "1"),
            ConfigurationStatus::NotSupported
        );
    }

    #[test]
    fn a_reboot_required_key_says_so() {
        let mut config = ConfigurationKeys::with_defaults();
        assert_eq!(
            config.set("ConnectionUrl", "ws://csms.example.com/ocpp"),
            ConfigurationStatus::RebootRequired
        );
    }

    #[test]
    fn get_configuration_splits_known_from_unknown_keys() {
        let config = ConfigurationKeys::with_defaults();
        let (known, unknown) = config.report(&["HeartbeatInterval", "Nonexistent"]);
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].name, "HeartbeatInterval");
        assert_eq!(unknown, alloc::vec!["Nonexistent".to_string()]);

        // With no request, everything declared is reported.
        let (all, unknown) = config.report(&[]);
        assert_eq!(all.len(), config.len());
        assert!(unknown.is_empty());
    }

    #[test]
    fn removing_a_key_makes_the_station_report_it_as_unknown() {
        let mut config = ConfigurationKeys::with_defaults();
        config.remove("ReserveConnectorZeroSupported");
        let (known, unknown) = config.report(&["ReserveConnectorZeroSupported"]);
        assert!(known.is_empty());
        assert_eq!(unknown.len(), 1);
    }
}
