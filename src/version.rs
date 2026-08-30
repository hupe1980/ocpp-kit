//! OCPP protocol versions and WebSocket subprotocol negotiation.

use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

/// A supported OCPP version.
///
/// The ordering is oldest → newest, so `max()` picks the most capable version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Version {
    /// OCPP 1.6J (edition 2 + errata, plus the Security Whitepaper edition 2 extensions).
    V1_6,
    /// OCPP 2.0.1.
    V2_0_1,
    /// OCPP 2.1.
    V2_1,
}

impl Version {
    /// Every version this crate implements, oldest first.
    pub const ALL: &'static [Version] = &[Version::V1_6, Version::V2_0_1, Version::V2_1];

    /// The WebSocket subprotocol token, e.g. `ocpp2.1` (Part 4 §3.1.2).
    #[must_use]
    pub const fn subprotocol(self) -> &'static str {
        match self {
            Version::V1_6 => "ocpp1.6",
            Version::V2_0_1 => "ocpp2.0.1",
            Version::V2_1 => "ocpp2.1",
        }
    }

    /// A short, filesystem-safe identifier: `v1_6`, `v2_0_1`, `v2_1`.
    ///
    /// Used for schema directories, metric labels and log fields.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Version::V1_6 => "v1_6",
            Version::V2_0_1 => "v2_0_1",
            Version::V2_1 => "v2_1",
        }
    }

    /// Parses a WebSocket subprotocol token.
    #[must_use]
    pub fn from_subprotocol(token: &str) -> Option<Self> {
        match token.trim() {
            "ocpp1.6" => Some(Version::V1_6),
            "ocpp2.0.1" => Some(Version::V2_0_1),
            "ocpp2.1" => Some(Version::V2_1),
            _ => None,
        }
    }

    /// Whether this version has `CALLRESULTERROR` (5) and `SEND` (6).
    #[must_use]
    pub const fn has_extended_message_types(self) -> bool {
        matches!(self, Version::V2_1)
    }

    /// Whether this version defines the `RpcFrameworkError` error code.
    ///
    /// 1.6J does not; framing failures are reported as `GenericError` there.
    #[must_use]
    pub const fn has_rpc_framework_error(self) -> bool {
        !matches!(self, Version::V1_6)
    }

    /// Whether an unreadable `MessageId` must be answered with the literal id `"-1"`
    /// (2.x Part 4 §4.1.1). 1.6J has no such rule.
    #[must_use]
    pub const fn uses_unreadable_message_id(self) -> bool {
        !matches!(self, Version::V1_6)
    }

    /// Whether the one-outstanding-`CALL` rule is a `SHALL NOT` (2.x, Part 4 §4.1.1) rather
    /// than 1.6J's `SHOULD NOT`.
    #[must_use]
    pub const fn strict_synchronicity(self) -> bool {
        !matches!(self, Version::V1_6)
    }

    /// Whether RFC 7692 `permessage-deflate` is part of the specification.
    ///
    /// 2.1 Part 4 §3.4 Table 2: the CSMS and a Local Controller **SHALL** support it; a
    /// Charging Station **MAY**. 1.6J §5.1 recommends no compression, and 2.0.1 is silent.
    #[must_use]
    pub const fn supports_compression(self) -> bool {
        matches!(self, Version::V2_1)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Version::V1_6 => "1.6",
            Version::V2_0_1 => "2.0.1",
            Version::V2_1 => "2.1",
        })
    }
}

impl FromStr for Version {
    type Err = UnknownVersion;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "1.6" | "1.6J" | "1.6j" | "ocpp1.6" => Ok(Version::V1_6),
            "2.0.1" | "ocpp2.0.1" => Ok(Version::V2_0_1),
            "2.1" | "ocpp2.1" => Ok(Version::V2_1),
            _ => Err(UnknownVersion),
        }
    }
}

/// A version string this crate does not recognise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownVersion;

impl fmt::Display for UnknownVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown OCPP version")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for UnknownVersion {}

/// The `Sec-WebSocket-Protocol` value a client offers, in preference order.
///
/// OCPP 2.1 Part 4 §3.2 recommends that a 2.1 peer also offer 2.0.1, so a station can talk
/// to a CSMS that has not migrated yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subprotocol {
    offered: Vec<Version>,
}

impl Subprotocol {
    /// Offers the given versions, most preferred first. Duplicates are removed.
    #[must_use]
    pub fn new(versions: impl IntoIterator<Item = Version>) -> Self {
        let mut offered = Vec::new();
        for version in versions {
            if !offered.contains(&version) {
                offered.push(version);
            }
        }
        Self { offered }
    }

    /// The versions offered, most preferred first.
    #[must_use]
    pub fn offered(&self) -> &[Version] {
        &self.offered
    }

    /// The `Sec-WebSocket-Protocol` header value.
    #[must_use]
    pub fn header_value(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for (index, version) in self.offered.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(version.subprotocol());
        }
        out
    }

    /// Picks the version a server should select: the first *client* preference that the
    /// server also supports (Part 4 §3.1.2 — the server selects exactly one).
    #[must_use]
    pub fn select(&self, supported: &[Version]) -> Option<Version> {
        self.offered.iter().copied().find(|v| supported.contains(v))
    }

    /// Validates a server's selection against what was offered.
    ///
    /// Part 4 §3.1.2: the server must echo exactly one of the offered subprotocols. A server
    /// that answers with something else, or with nothing, is a negotiation failure.
    pub fn accept(&self, selected: Option<&str>) -> Result<Version, NegotiationError> {
        let Some(token) = selected else {
            return Err(NegotiationError::NoSubprotocol);
        };
        let version =
            Version::from_subprotocol(token).ok_or(NegotiationError::UnsupportedSubprotocol)?;
        if self.offered.contains(&version) {
            Ok(version)
        } else {
            Err(NegotiationError::NotOffered(version))
        }
    }
}

impl Default for Subprotocol {
    /// Offers every version this crate implements, newest first.
    fn default() -> Self {
        Self::new([Version::V2_1, Version::V2_0_1, Version::V1_6])
    }
}

/// Why subprotocol negotiation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NegotiationError {
    /// The server did not send `Sec-WebSocket-Protocol`. Part 4 §3.1.2 requires it; the
    /// connection must be closed.
    NoSubprotocol,
    /// The server selected a subprotocol that is not an OCPP one.
    UnsupportedSubprotocol,
    /// The server selected a version the client did not offer.
    NotOffered(Version),
}

impl fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NegotiationError::NoSubprotocol => {
                f.write_str("server did not select a WebSocket subprotocol (Part 4 §3.1.2)")
            }
            NegotiationError::UnsupportedSubprotocol => {
                f.write_str("server selected a subprotocol that is not OCPP")
            }
            NegotiationError::NotOffered(version) => {
                write!(f, "server selected OCPP {version}, which was not offered")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for NegotiationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_follows_client_preference() {
        let offer = Subprotocol::new([Version::V2_1, Version::V2_0_1, Version::V1_6]);
        assert_eq!(offer.header_value(), "ocpp2.1, ocpp2.0.1, ocpp1.6");
        assert_eq!(
            offer.select(&[Version::V1_6, Version::V2_0_1]),
            Some(Version::V2_0_1)
        );
        assert_eq!(offer.accept(Some("ocpp2.0.1")), Ok(Version::V2_0_1));
        assert_eq!(offer.accept(None), Err(NegotiationError::NoSubprotocol));
        assert_eq!(
            offer.accept(Some("mqtt")),
            Err(NegotiationError::UnsupportedSubprotocol)
        );

        let narrow = Subprotocol::new([Version::V1_6]);
        assert_eq!(
            narrow.accept(Some("ocpp2.1")),
            Err(NegotiationError::NotOffered(Version::V2_1))
        );
    }
}
