//! Primitive types shared by every OCPP version.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationPath, ViolationKind, Violations};

// ---------------------------------------------------------------------------
// DateTime
// ---------------------------------------------------------------------------

/// An OCPP `dateTime`: an instant, serialized as RFC 3339 with an explicit offset.
///
/// Parsing accepts more than it emits, because the field is full of stations that omit the
/// offset or use a space separator. Which of those are tolerated is decided by
/// [`DecodeOptions::datetime`](crate::decode::DecodeOptions::datetime): the strict
/// `Deserialize` impl below requires an offset, and the decoder repairs the input first when
/// leniency is enabled. Serialization always emits UTC (`…Z`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(jiff::Timestamp);

impl DateTime {
    /// The Unix epoch.
    pub const UNIX_EPOCH: Self = Self(jiff::Timestamp::UNIX_EPOCH);

    #[must_use]
    /// Wraps a `jiff` timestamp.
    pub const fn from_timestamp(timestamp: jiff::Timestamp) -> Self {
        Self(timestamp)
    }

    #[must_use]
    /// The underlying `jiff` timestamp.
    pub const fn timestamp(self) -> jiff::Timestamp {
        self.0
    }

    /// The current time from the system clock.
    #[cfg(feature = "std")]
    #[must_use]
    pub fn now() -> Self {
        Self(jiff::Timestamp::now())
    }

    /// Parses an RFC 3339 timestamp. The offset is mandatory.
    pub fn parse(text: &str) -> Result<Self, DateTimeError> {
        jiff::Timestamp::from_str(text)
            .map(Self)
            .map_err(|_| DateTimeError {
                input: text.to_owned(),
            })
    }

    /// Parses leniently: also accepts a missing offset (interpreted as UTC) and a space
    /// instead of `T`, both of which occur in the field.
    ///
    /// Reachable from the decoder via
    /// [`DateTimeLeniency`](crate::decode::DateTimeLeniency).
    pub fn parse_lenient(text: &str) -> Result<Self, DateTimeError> {
        if let Ok(value) = Self::parse(text) {
            return Ok(value);
        }
        let normalized = normalize_datetime(text);
        Self::parse(&normalized)
    }
}

/// Rewrites the common non-conforming timestamp spellings into strict RFC 3339.
///
/// Returns the input unchanged when it does not look like a bare local timestamp.
#[must_use]
pub fn normalize_datetime(text: &str) -> String {
    let trimmed = text.trim();
    let with_t = match trimmed.find(' ') {
        Some(index) if !trimmed.contains('T') => {
            let mut s = trimmed.to_owned();
            s.replace_range(index..=index, "T");
            s
        }
        _ => trimmed.to_owned(),
    };
    let has_offset = with_t.ends_with('Z')
        || with_t.ends_with('z')
        // `+01:00` / `-05:00`, but not the date's own separators.
        || with_t.rfind(['+', '-']).is_some_and(|i| i > 10);
    if has_offset {
        with_t
    } else {
        format!("{with_t}Z")
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<jiff::Timestamp> for DateTime {
    fn from(value: jiff::Timestamp) -> Self {
        Self(value)
    }
}

impl From<DateTime> for jiff::Timestamp {
    fn from(value: DateTime) -> Self {
        value.0
    }
}

impl FromStr for DateTime {
    type Err = DateTimeError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for DateTime {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DateTime {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = DateTime;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an RFC 3339 date-time with an offset")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<DateTime, E> {
                DateTime::parse(text).map_err(E::custom)
            }
        }

        // A visitor rather than `<&str>::deserialize`: a JSON string containing an escape
        // cannot be borrowed, and asking for a borrowed one would report that as
        // "invalid type: string" — a *type* violation, when the value is really the problem.
        de.deserialize_str(Visitor)
    }
}

impl Validate for DateTime {
    fn validate_at(&self, _path: &mut ValidationPath, _out: &mut Violations) {}
}

/// A timestamp that is not valid RFC 3339.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DateTimeError {
    input: String,
}

impl fmt::Display for DateTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not an RFC 3339 date-time with an offset",
            self.input
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DateTimeError {}

// ---------------------------------------------------------------------------
// Decimal
// ---------------------------------------------------------------------------

/// An OCPP `number`.
///
/// Backed by `f64`, because that is what the schemas say (`"type": "number"`) and every OCPP
/// number is a sensor reading — watts, amperes, watt-hours, percentages.
///
/// Serialized in `serde_json`'s `float_roundtrip` mode, so the *value* survives exactly
/// (`1.15` never drifts to `1.1499999999999999`) but the spelling does not: `1.10` comes back
/// as `1.1`. Non-finite values are refused at serialization rather than becoming `null`.
///
/// Money is the exception, in the 2.1 Tariff and Cost block. Work in the smallest currency
/// unit and call [`Decimal::get`] at the boundary, or carry your own decimal type.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct Decimal(f64);

impl Decimal {
    #[must_use]
    /// Wraps a floating-point value.
    pub const fn new(value: f64) -> Self {
        Self(value)
    }

    #[must_use]
    /// The underlying value.
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl From<f64> for Decimal {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

impl From<f32> for Decimal {
    fn from(value: f32) -> Self {
        Self(f64::from(value))
    }
}

impl From<i32> for Decimal {
    fn from(value: i32) -> Self {
        Self(f64::from(value))
    }
}

impl From<Decimal> for f64 {
    fn from(value: Decimal) -> Self {
        value.0
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Decimal {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if self.0.is_finite() {
            ser.serialize_f64(self.0)
        } else {
            Err(serde::ser::Error::custom("OCPP numbers must be finite"))
        }
    }
}

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        f64::deserialize(de).map(Self)
    }
}

impl Validate for Decimal {
    fn validate_at(&self, path: &mut ValidationPath, out: &mut Violations) {
        if !self.0.is_finite() {
            out.push(
                path,
                ViolationKind::Type,
                "value is not a finite JSON number",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// CustomData
// ---------------------------------------------------------------------------

/// The OCPP 2.x `CustomDataType` extension point.
///
/// This is the one object the schemas deliberately leave open, so the unrecognised members
/// are kept in [`extra`](Self::extra) instead of being dropped — a Local Controller can relay
/// them and a CSMS can round-trip them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomData {
    /// Vendor identifier, at most 255 characters.
    #[serde(rename = "vendorId")]
    pub vendor_id: String,
    /// Everything else the vendor put in this object.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl CustomData {
    /// Creates custom data for one vendor.
    #[must_use]
    pub fn new(vendor_id: impl Into<String>) -> Self {
        Self {
            vendor_id: vendor_id.into(),
            extra: serde_json::Map::new(),
        }
    }

    /// Adds one custom member.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

impl Validate for CustomData {
    fn validate_at(&self, path: &mut ValidationPath, out: &mut Violations) {
        path.push_key("vendorId");
        crate::validate::string(&self.vendor_id, None, Some(255), path, out);
        path.pop();
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A Charging Station identity.
///
/// Carried as the last segment of the WebSocket URL path and, under security profiles 1 and
/// 2, as the HTTP Basic user name. At most 48 characters, and it must not contain `:`
/// because that would make the Basic credentials ambiguous (A00.FR.204).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Identity(String);

/// The maximum length of a Charging Station identity, in characters.
pub const IDENTITY_MAX_LEN: usize = 48;

impl Identity {
    /// Validates and wraps an identity.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError::Empty);
        }
        let len = value.chars().count();
        if len > IDENTITY_MAX_LEN {
            return Err(IdentityError::TooLong(len));
        }
        // A00.FR.204 — the identity doubles as the Basic-Auth user name.
        if value.contains(':') {
            return Err(IdentityError::Colon);
        }
        if value.chars().any(char::is_control) {
            return Err(IdentityError::Control);
        }
        Ok(Self(value))
    }

    #[must_use]
    /// The identity as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the identity.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Identity {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let value = String::deserialize(de)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Why an identity was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentityError {
    /// The identity is empty.
    Empty,
    /// Longer than [`IDENTITY_MAX_LEN`] characters.
    TooLong(usize),
    /// Contains `:`, which HTTP Basic authentication cannot represent (A00.FR.204).
    Colon,
    /// Contains a control character, which cannot appear in a URL path segment.
    Control,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::Empty => f.write_str("charging station identity is empty"),
            IdentityError::TooLong(len) => {
                write!(
                    f,
                    "charging station identity is {len} characters, maximum is {IDENTITY_MAX_LEN}"
                )
            }
            IdentityError::Colon => {
                f.write_str("charging station identity must not contain ':' (A00.FR.204)")
            }
            IdentityError::Control => {
                f.write_str("charging station identity contains a control character")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for IdentityError {}

// ---------------------------------------------------------------------------
// MessageId
// ---------------------------------------------------------------------------

/// The maximum length of an OCPP-J message id, in characters (Part 4 §4.1.1).
pub const MESSAGE_ID_MAX_LEN: usize = 36;

/// An OCPP-J `MessageId` (1.6J calls it `UniqueId`): a string of at most 36 characters that
/// correlates a `CALL` with its `CALLRESULT` / `CALLERROR`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    /// Validates and wraps a message id.
    pub fn new(value: impl Into<String>) -> Result<Self, MessageIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(MessageIdError::Empty);
        }
        let len = value.chars().count();
        if len > MESSAGE_ID_MAX_LEN {
            return Err(MessageIdError::TooLong(len));
        }
        Ok(Self(value))
    }

    /// Wraps an id exactly as a peer sent it, including one that is too long.
    ///
    /// The framing layer must echo the id it received verbatim or the peer cannot correlate
    /// the answer, so length is *reported* — see [`is_conforming`](Self::is_conforming) —
    /// rather than silently changed.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        Self(value.to_owned())
    }

    /// Whether the id respects the 36-character limit.
    #[must_use]
    pub fn is_conforming(&self) -> bool {
        !self.0.is_empty() && self.0.chars().count() <= MESSAGE_ID_MAX_LEN
    }

    /// Wraps a value, truncating it to [`MESSAGE_ID_MAX_LEN`] characters.
    ///
    /// For ids this peer generates, so an over-long prefix cannot produce an invalid id.
    #[must_use]
    pub fn truncating(value: &str) -> Self {
        match value.char_indices().nth(MESSAGE_ID_MAX_LEN) {
            Some((cut, _)) => Self(value[..cut].to_owned()),
            None => Self(value.to_owned()),
        }
    }

    /// The id OCPP 2.x prescribes when the incoming `MessageId` itself could not be read
    /// (Part 4 §4.1.1).
    #[must_use]
    pub fn unreadable() -> Self {
        Self("-1".to_owned())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the id.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MessageId {
    type Err = MessageIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let value = String::deserialize(de)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Why a message id was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageIdError {
    /// The id is empty.
    Empty,
    /// Longer than [`MESSAGE_ID_MAX_LEN`] characters.
    TooLong(usize),
}

impl fmt::Display for MessageIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MessageIdError::Empty => f.write_str("message id is empty"),
            MessageIdError::TooLong(len) => {
                write!(
                    f,
                    "message id is {len} characters, maximum is {MESSAGE_ID_MAX_LEN}"
                )
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MessageIdError {}

/// Source of outgoing [`MessageId`]s.
///
/// Pluggable because constrained devices cannot afford an entropy source, and a Local
/// Controller needs ids that provably cannot collide with the CSMS's (Part 4 §6.4).
///
/// # The uniqueness rule
///
/// Part 4 §4.1.4 is stricter than it first looks: a `MessageId` must differ from every id the
/// same sender has used for a `CALL` or a `SEND` **on any WebSocket connection under the same
/// Charging Station identity** — not merely within one connection. A retransmission of the
/// same message *may* reuse its id, and nothing else may.
pub trait IdGenerator {
    /// Produces the next id.
    ///
    /// Implementations must satisfy the uniqueness rule above.
    fn next_id(&mut self) -> MessageId;
}

/// A random generator: a version 4 UUID per id, 36 characters exactly.
///
/// The default wherever an entropy source is available. 122 random bits make a collision
/// across reboots — which is what Part 4 §4.1.4 actually forbids — not worth reasoning about.
///
/// It also carries a counter, which only matters in the case that should never happen: if the
/// operating system's entropy source fails, the ids fall back to being *distinct* rather than
/// random. A repeating `MessageId` is the one outcome §4.1.4 rules out, and it is exactly what
/// a fixed fallback value would produce.
#[cfg(feature = "getrandom")]
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomIds {
    counter: u64,
}

#[cfg(feature = "getrandom")]
impl RandomIds {
    /// A generator drawing from the operating system's entropy source.
    #[must_use]
    pub fn new() -> Self {
        Self { counter: 0 }
    }
}

#[cfg(feature = "getrandom")]
impl IdGenerator for RandomIds {
    fn next_id(&mut self) -> MessageId {
        self.counter = self.counter.wrapping_add(1);
        MessageId(uuid_v4().unwrap_or_else(|| degraded_uuid(self.counter)))
    }
}

/// A random version 4 UUID, in the canonical 36-character hyphenated form.
///
/// `None` when the operating system's entropy source failed — which on a supported target
/// means it is not merely low on entropy but unavailable. There is no safe constant to return:
/// two OCPP identifiers are required to be unique across a Charging Station's whole lifetime —
/// the `MessageId` (Part 4 §4.1.4) and the `transactionId` (E01.FR.08, which spells out "even
/// when the Charging Station is rebooted, repaired, firmware is updated etc." and recommends
/// UUIDs by name) — so a fixed fallback would break precisely the guarantee callers came for.
/// Callers that must produce *something* use a counter instead; see [`RandomIds`].
#[cfg(feature = "getrandom")]
#[must_use]
pub fn uuid_v4() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant RFC 4122
    Some(hyphenate(bytes))
}

/// A UUID-shaped id built from a counter, for the case where entropy is unavailable.
///
/// Version 4 is *not* claimed — the version nibble is `8`, which no RFC 4122 version uses —
/// so a reader can tell at a glance that these ids are unique within a process run and nothing
/// more. Across a reboot they repeat, which is why this is a last resort and not a mode.
#[cfg(feature = "getrandom")]
#[must_use]
fn degraded_uuid(counter: u64) -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&counter.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x80;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    hyphenate(bytes)
}

/// Renders 16 bytes as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
#[cfg(feature = "getrandom")]
fn hyphenate(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 36];
    let mut index = 0;
    for (position, byte) in bytes.iter().enumerate() {
        if matches!(position, 4 | 6 | 8 | 10) {
            out[index] = b'-';
            index += 1;
        }
        out[index] = HEX[usize::from(byte >> 4)];
        out[index + 1] = HEX[usize::from(byte & 0x0F)];
        index += 2;
    }
    // Every byte written is ASCII hex or `-`.
    core::str::from_utf8(&out)
        .unwrap_or("00000000-0000-8000-8000-000000000000")
        .to_owned()
}

/// A counter-based generator: `<prefix><counter>`, base-36 encoded.
///
/// For targets with no entropy source. It satisfies Part 4 §4.1.4 **only if the prefix is
/// different on every boot** — a persisted boot counter, a serial number plus a session
/// number, anything that does not repeat. With a constant prefix the ids restart at zero
/// after a power cut and collide with the ones used before it, which is exactly what §4.1.4
/// forbids and what makes a CSMS mismatch a response to the wrong request.
///
/// A Local Controller uses the prefix for a second purpose: keeping its own ids disjoint from
/// the CSMS's (Part 4 §6.4).
#[derive(Clone, Debug)]
pub struct CounterIds {
    prefix: String,
    next: u64,
}

impl CounterIds {
    /// Starts a generator with the given prefix. Read the type's documentation first: the
    /// prefix is what makes the ids unique across reboots.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            next: 0,
        }
    }
}

impl Default for CounterIds {
    fn default() -> Self {
        Self::with_prefix("")
    }
}

impl IdGenerator for CounterIds {
    fn next_id(&mut self) -> MessageId {
        let value = self.next;
        self.next = self.next.wrapping_add(1);
        let mut digits = [0u8; 13];
        let mut index = digits.len();
        let mut remaining = value;
        loop {
            index -= 1;
            let digit = u8::try_from(remaining % 36).unwrap_or(0);
            digits[index] = if digit < 10 {
                b'0' + digit
            } else {
                b'a' + digit - 10
            };
            remaining /= 36;
            if remaining == 0 {
                break;
            }
        }
        let suffix = core::str::from_utf8(&digits[index..]).unwrap_or("0");
        MessageId::truncating(&format!("{}{suffix}", self.prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_rejects_colon_per_a00_fr_204() {
        assert_eq!(Identity::new("cs:1"), Err(IdentityError::Colon));
        assert!(Identity::new("CS-0001").is_ok());
        assert_eq!(
            Identity::new("x".repeat(49)),
            Err(IdentityError::TooLong(49))
        );
    }

    #[test]
    fn message_id_truncates_to_36_characters() {
        let id = MessageId::truncating(&"a".repeat(80));
        assert_eq!(id.as_str().chars().count(), 36);
    }

    #[test]
    fn counter_ids_are_unique_and_prefixed() {
        let mut ids = CounterIds::with_prefix("lc-");
        assert_eq!(ids.next_id().as_str(), "lc-0");
        assert_eq!(ids.next_id().as_str(), "lc-1");
        for _ in 0..40 {
            ids.next_id();
        }
        assert_eq!(ids.next_id().as_str(), "lc-16");
    }

    #[cfg(feature = "getrandom")]
    #[test]
    fn random_ids_are_uuids_and_do_not_repeat() {
        let mut ids = RandomIds::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let id = ids.next_id();
            assert_eq!(id.as_str().len(), 36, "{id}");
            assert!(id.is_conforming());
            assert_eq!(&id.as_str()[14..15], "4", "version nibble: {id}");
            assert!(seen.insert(id.into_string()));
        }
    }

    #[test]
    fn datetime_leniency_matches_field_behaviour() {
        assert!(DateTime::parse("2024-01-01T10:00:00").is_err());
        assert_eq!(
            DateTime::parse_lenient("2024-01-01T10:00:00").unwrap(),
            DateTime::parse("2024-01-01T10:00:00Z").unwrap()
        );
        assert_eq!(
            DateTime::parse_lenient("2024-01-01 10:00:00").unwrap(),
            DateTime::parse("2024-01-01T10:00:00Z").unwrap()
        );
        assert_eq!(
            DateTime::parse_lenient("2024-01-01T10:00:00+02:00").unwrap(),
            DateTime::parse("2024-01-01T08:00:00Z").unwrap()
        );
    }
}
