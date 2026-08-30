//! Network connection profiles — the Charging Station's list of ways to reach a CSMS.
//!
//! A 2.x Charging Station does not have *a* CSMS URL; it has numbered **configuration slots**,
//! each holding a `NetworkConnectionProfile`, plus an ordered list of the slots to try
//! (`OCPPCommCtrlr.NetworkConfigurationPriority`) and a number of attempts to spend on each
//! before moving on (`OCPPCommCtrlr.NetworkProfileConnectionAttempts`).
//!
//! That is what makes B10 — migrate to a new CSMS — work without touching the station: the
//! operator reorders the priority list, the station reboots, and it comes up talking to a
//! different CSMS with the old one still configured as the fallback.
//!
//! ```
//! use ocpp_kit::Version;
//! use ocpp_kit::transport::{BasicAuthPassword, NetworkProfile, NetworkProfiles, SecurityProfile};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let profiles = NetworkProfiles::new([
//!     NetworkProfile::new(0, "ws://fallback.example.com/ocpp")
//!         .security_profile(SecurityProfile::BasicAuth)
//!         .password(BasicAuthPassword::utf8("0123456789abcdef")?),
//!     NetworkProfile::new(1, "ws://primary.example.com/ocpp")
//!         .security_profile(SecurityProfile::BasicAuth)
//!         .password(BasicAuthPassword::utf8("0123456789abcdef")?),
//! ])
//! // "1,0": try the primary first, fall back to slot 0.
//! .priority([1, 0])?
//! .connection_attempts(3);
//!
//! assert_eq!(profiles.priority_order(), &[1, 0]);
//! # Ok(()) }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::version::{Subprotocol, Version};

use super::TransportError;
use super::security::{BasicAuthPassword, SecurityProfile};

/// One `NetworkConnectionProfile`, in one configuration slot.
#[derive(Clone)]
pub struct NetworkProfile {
    pub(crate) slot: i32,
    pub(crate) url: String,
    pub(crate) versions: Option<Subprotocol>,
    pub(crate) security: SecurityProfile,
    pub(crate) password: Option<BasicAuthPassword>,
    #[cfg(feature = "rustls")]
    pub(crate) tls: Option<super::tls::ClientTls>,
    pub(crate) message_timeout: Option<Duration>,
}

impl NetworkProfile {
    /// A profile in `slot`, reaching the CSMS at `url` (without the identity, which is
    /// appended).
    #[must_use]
    pub fn new(slot: i32, url: impl Into<String>) -> Self {
        Self {
            slot,
            url: url.into(),
            versions: None,
            security: SecurityProfile::BasicAuth,
            password: None,
            #[cfg(feature = "rustls")]
            tls: None,
            message_timeout: None,
        }
    }

    /// The configuration slot this profile occupies.
    #[must_use]
    pub const fn slot(&self) -> i32 {
        self.slot
    }

    /// The CSMS endpoint.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The security profile this connection uses.
    #[must_use]
    pub const fn security_profile_number(&self) -> u8 {
        self.security.number()
    }

    /// Restricts the versions offered on this connection. Defaults to the station's list.
    #[must_use]
    pub fn versions(mut self, versions: impl IntoIterator<Item = Version>) -> Self {
        self.versions = Some(Subprotocol::new(versions));
        self
    }

    /// Sets the security profile.
    #[must_use]
    pub fn security_profile(mut self, profile: SecurityProfile) -> Self {
        self.security = profile;
        self
    }

    /// Sets the HTTP Basic password, for profiles 1 and 2.
    #[must_use]
    pub fn password(mut self, password: BasicAuthPassword) -> Self {
        self.password = Some(password);
        self
    }

    /// Sets the TLS configuration, for profiles 2 and 3.
    #[cfg(feature = "rustls")]
    #[must_use]
    pub fn tls(mut self, tls: super::tls::ClientTls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Sets `NetworkConnectionProfile.messageTimeout`, which overrides
    /// `OCPPCommCtrlr.MessageTimeout[Default]` while this profile is active (Part 4 §4.1.1).
    #[must_use]
    pub fn message_timeout(mut self, timeout: Duration) -> Self {
        self.message_timeout = Some(timeout);
        self
    }
}

impl fmt::Debug for NetworkProfile {
    /// Never prints the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkProfile")
            .field("slot", &self.slot)
            .field("url", &self.url)
            .field("security", &self.security)
            .field("message_timeout", &self.message_timeout)
            .finish_non_exhaustive()
    }
}

/// The station's configuration slots, the order to try them in, and how many attempts each
/// one gets.
#[derive(Clone, Debug)]
pub struct NetworkProfiles {
    slots: BTreeMap<i32, NetworkProfile>,
    priority: Vec<i32>,
    attempts: u32,
}

impl NetworkProfiles {
    /// Builds the configuration from a set of profiles.
    ///
    /// The priority defaults to the order the profiles are given in.
    #[must_use]
    pub fn new(profiles: impl IntoIterator<Item = NetworkProfile>) -> Self {
        let mut slots = BTreeMap::new();
        let mut priority = Vec::new();
        for profile in profiles {
            if !priority.contains(&profile.slot) {
                priority.push(profile.slot);
            }
            slots.insert(profile.slot, profile);
        }
        Self {
            slots,
            priority,
            attempts: 3,
        }
    }

    /// Sets `NetworkConfigurationPriority`: the slots to try, in order.
    ///
    /// Every named slot must be configured — the specification is explicit that a slot which
    /// is not fully configured must not appear in the priority list (B09), because a station
    /// that tries a half-configured slot has no way to recover.
    pub fn priority(
        mut self,
        order: impl IntoIterator<Item = i32>,
    ) -> Result<Self, TransportError> {
        let order: Vec<i32> = order.into_iter().collect();
        if order.is_empty() {
            return Err(TransportError::Configuration(
                "NetworkConfigurationPriority must name at least one slot".into(),
            ));
        }
        for slot in &order {
            if !self.slots.contains_key(slot) {
                return Err(TransportError::Configuration(format!(
                    "NetworkConfigurationPriority names configuration slot {slot}, which has no profile"
                )));
            }
        }
        self.priority = order;
        Ok(self)
    }

    /// Sets `NetworkProfileConnectionAttempts`: how many failed attempts one profile gets
    /// before the station moves to the next.
    #[must_use]
    pub fn connection_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    /// The slots to try, in order.
    #[must_use]
    pub fn priority_order(&self) -> &[i32] {
        &self.priority
    }

    /// How many failed attempts each profile gets.
    #[must_use]
    pub const fn attempts_per_profile(&self) -> u32 {
        self.attempts
    }

    /// The `valuesList` of `NetworkConfigurationPriority`: every configured slot.
    #[must_use]
    pub fn configured_slots(&self) -> Vec<i32> {
        self.slots.keys().copied().collect()
    }

    /// Looks a profile up by slot.
    #[must_use]
    pub fn get(&self, slot: i32) -> Option<&NetworkProfile> {
        self.slots.get(&slot)
    }

    /// The profile at position `index` of the priority list, wrapping.
    #[must_use]
    pub(crate) fn at(&self, index: usize) -> &NetworkProfile {
        let slot = self.priority[index % self.priority.len()];
        self.slots
            .get(&slot)
            .expect("priority is validated against the slots")
    }

    /// How many entries the priority list has.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.priority.len()
    }
}

/// Walks the priority list, spending `attempts_per_profile` failures on each.
#[derive(Debug)]
pub(crate) struct ProfileCycler {
    index: usize,
    failures: u32,
    limit: u32,
    len: usize,
}

impl ProfileCycler {
    pub(crate) fn new(profiles: &NetworkProfiles) -> Self {
        Self {
            index: 0,
            failures: 0,
            limit: profiles.attempts_per_profile(),
            len: profiles.len(),
        }
    }

    /// The position in the priority list currently being tried.
    pub(crate) const fn index(&self) -> usize {
        self.index
    }

    /// Records a successful connection: the station stays on this profile.
    pub(crate) fn succeeded(&mut self) {
        self.failures = 0;
    }

    /// Records a failed connection attempt, moving to the next profile once this one has had
    /// its share. Returns `true` if the profile changed.
    pub(crate) fn failed(&mut self) -> bool {
        self.failures += 1;
        if self.failures >= self.limit && self.len > 1 {
            self.failures = 0;
            self.index = (self.index + 1) % self.len;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles() -> NetworkProfiles {
        NetworkProfiles::new([
            NetworkProfile::new(0, "ws://fallback/ocpp"),
            NetworkProfile::new(1, "ws://primary/ocpp"),
        ])
    }

    #[test]
    fn the_priority_list_decides_the_order_and_must_be_configured() {
        let configured = profiles().priority([1, 0]).unwrap();
        assert_eq!(configured.priority_order(), &[1, 0]);
        assert_eq!(configured.at(0).url(), "ws://primary/ocpp");
        assert_eq!(configured.at(1).url(), "ws://fallback/ocpp");
        // Wrapping, so the list is a cycle.
        assert_eq!(configured.at(2).url(), "ws://primary/ocpp");

        // B09: a slot that is not configured must not be in the priority list.
        let error = profiles().priority([1, 7]).unwrap_err();
        assert!(
            error.to_string().contains("configuration slot 7"),
            "{error}"
        );
        assert!(profiles().priority([]).is_err());
    }

    #[test]
    fn a_profile_gets_its_share_of_attempts_before_the_station_moves_on() {
        let configured = profiles().priority([1, 0]).unwrap().connection_attempts(3);
        let mut cycler = ProfileCycler::new(&configured);

        assert_eq!(configured.at(cycler.index()).slot(), 1);
        assert!(!cycler.failed());
        assert!(!cycler.failed());
        // The third failure exhausts NetworkProfileConnectionAttempts.
        assert!(cycler.failed());
        assert_eq!(configured.at(cycler.index()).slot(), 0);

        // A successful connection resets the count, so a flapping link does not walk the list.
        cycler.succeeded();
        assert!(!cycler.failed());
        assert!(!cycler.failed());
        assert!(cycler.failed());
        assert_eq!(configured.at(cycler.index()).slot(), 1, "the list wraps");
    }

    #[test]
    fn a_single_profile_never_switches_away_from_itself() {
        let single = NetworkProfiles::new([NetworkProfile::new(0, "ws://only/ocpp")]);
        let mut cycler = ProfileCycler::new(&single);
        for _ in 0..10 {
            assert!(!cycler.failed());
        }
        assert_eq!(single.at(cycler.index()).slot(), 0);
    }

    #[test]
    fn the_configured_slots_are_the_values_list_of_the_priority_variable() {
        assert_eq!(profiles().configured_slots(), vec![0, 1]);
    }
}
