//! Payload decoding with spec-exact failure classification.
//!
//! OCPP does not have one "bad payload" error. It has five, and answering with the wrong one
//! is a conformance failure that most implementations make because they collapse every
//! `serde` error into a single code. This module keeps them apart:
//!
//! | What went wrong | [`DecodeErrorKind`] |
//! |---|---|
//! | The payload is not a JSON object, or the JSON is malformed | [`Format`](DecodeErrorKind::Format) |
//! | A required member is missing, or an array is too short/long | [`Occurrence`](DecodeErrorKind::Occurrence) |
//! | A member has the wrong JSON type | [`Type`](DecodeErrorKind::Type) |
//! | A value is too long, out of range, or an undefined enum value | [`Property`](DecodeErrorKind::Property) |
//! | A member the schema does not define was present (strict mode) | [`UnknownField`](DecodeErrorKind::UnknownField) |
//! | Cross-field rules broken | [`Protocol`](DecodeErrorKind::Protocol) |
//!
//! [`DecodeOptions`] additionally carries the leniency knobs that make it possible to talk to
//! the substantial share of field devices that do not follow the schemas. Leniency is
//! implemented as a bounded *repair loop*: the strict parse runs first, and only when it
//! fails is the offending member — identified by path — rewritten and the parse retried. A
//! conforming payload therefore costs exactly one strict parse.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::validate::{Validate, ViolationKind};

/// How to treat enumeration values the schema does not define.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnknownEnumValues {
    /// Fail with [`DecodeErrorKind::Property`].
    #[default]
    Reject,
    /// Keep them in the enum's open `UnknownValue` variant.
    Preserve,
}

/// How to treat members the schema does not define.
///
/// The 2.x schemas say `additionalProperties: false`, but rejecting is not the safe default
/// in the field: stations routinely add vendor members outside `customData`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnknownFields {
    /// Fail with [`DecodeErrorKind::UnknownField`].
    Reject,
    /// Drop them.
    #[default]
    Ignore,
}

/// How much to forgive in `dateTime` members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateTimeLeniency {
    /// RFC 3339 with a mandatory offset, as the schemas require.
    #[default]
    Strict,
    /// Also accept a missing offset, interpreted as UTC.
    AllowNaive,
    /// Also accept a space instead of `T`, and a missing offset.
    AllowSpace,
}

/// How to treat numbers sent as JSON strings (`"42"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumericStrings {
    /// Fail with [`DecodeErrorKind::Type`].
    #[default]
    Reject,
    /// Convert them, when the string is a valid number.
    Coerce,
}

/// Decoding policy. The strict default is exactly what the specification requires.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Enumeration values the schema does not define.
    pub unknown_enum_values: UnknownEnumValues,
    /// Members the schema does not define.
    pub unknown_fields: UnknownFields,
    /// `dateTime` spellings.
    pub datetime: DateTimeLeniency,
    /// Numbers sent as strings.
    pub numeric_strings: NumericStrings,
    /// Upper bound on one *payload*, in bytes — the fourth element of a `CALL`, not the
    /// whole frame.
    ///
    /// The frame is bounded earlier and elsewhere, at the socket, by
    /// [`Csms::max_message_size_bytes`](crate::transport::CsmsBuilder::max_message_size_bytes);
    /// this is the second line, and it is what a handler that decodes a payload of its own
    /// gets for free. OCPP sets no limit of its own, and `NotifyReport` and
    /// `GetCompositeSchedule` payloads can be large, so the default is generous.
    pub max_payload_size: usize,
    /// Upper bound on the number of leniency repairs applied to one payload.
    pub max_repairs: usize,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self::strict()
    }
}

impl DecodeOptions {
    /// Exactly what the specification requires. The default.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            unknown_enum_values: UnknownEnumValues::Reject,
            unknown_fields: UnknownFields::Ignore,
            datetime: DateTimeLeniency::Strict,
            numeric_strings: NumericStrings::Reject,
            max_payload_size: 1024 * 1024,
            max_repairs: 32,
        }
    }

    /// Everything the schemas require, plus rejection of undefined members.
    ///
    /// Useful in tests and for a CSMS that wants to hold its fleet to the letter of
    /// `additionalProperties: false`.
    #[must_use]
    pub const fn pedantic() -> Self {
        Self {
            unknown_fields: UnknownFields::Reject,
            ..Self::strict()
        }
    }

    /// The knobs field devices actually need: undefined enum values and members are kept,
    /// offset-less and space-separated timestamps are accepted, and numeric strings are
    /// coerced.
    #[must_use]
    pub const fn lenient() -> Self {
        Self {
            unknown_enum_values: UnknownEnumValues::Preserve,
            unknown_fields: UnknownFields::Ignore,
            datetime: DateTimeLeniency::AllowSpace,
            numeric_strings: NumericStrings::Coerce,
            max_payload_size: 1024 * 1024,
            max_repairs: 32,
        }
    }

    /// Sets the maximum accepted payload size.
    #[must_use]
    pub const fn with_max_payload_size(mut self, bytes: usize) -> Self {
        self.max_payload_size = bytes;
        self
    }

    fn repairs_enabled(&self) -> bool {
        self.datetime != DateTimeLeniency::Strict || self.numeric_strings == NumericStrings::Coerce
    }
}

/// The category of a decoding failure, which decides the OCPP error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecodeErrorKind {
    /// Malformed JSON, or a payload that is not a JSON object.
    Format,
    /// A required member is absent, or an array's cardinality is wrong.
    Occurrence,
    /// A member has the wrong JSON type.
    Type,
    /// A value violates a `maxLength`, range, or enumeration constraint.
    Property,
    /// A member the schema does not define was present.
    UnknownField,
    /// Individually valid members that break a cross-field rule.
    Protocol,
    /// The action is not defined for this direction or version.
    UnsupportedAction,
}

/// A decoding failure, carrying enough detail to fill an OCPP `CALLERROR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    /// Which OCPP error code this maps to.
    pub kind: DecodeErrorKind,
    /// RFC 6901 pointer to the offending member, empty for whole-payload failures.
    pub path: String,
    /// Human-readable reason, suitable for `errorDescription`.
    pub reason: String,
}

impl DecodeError {
    /// Builds a decoding failure.
    #[must_use]
    pub fn new(kind: DecodeErrorKind, path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// The action is not valid for this direction or version.
    #[must_use]
    pub fn unsupported_action(action: &str) -> Self {
        Self::new(
            DecodeErrorKind::UnsupportedAction,
            "",
            format!("action {action:?} is not valid in this direction"),
        )
    }

    #[must_use]
    fn from_violation(violation: &crate::validate::Violation) -> Self {
        let kind = match violation.kind {
            ViolationKind::Occurrence => DecodeErrorKind::Occurrence,
            ViolationKind::Type => DecodeErrorKind::Type,
            ViolationKind::Property | ViolationKind::UnknownEnumValue => DecodeErrorKind::Property,
            ViolationKind::UnknownField => DecodeErrorKind::UnknownField,
            ViolationKind::Protocol => DecodeErrorKind::Protocol,
        };
        Self::new(kind, violation.path.clone(), violation.reason.clone())
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.path, self.reason)
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

/// Decodes and validates one payload according to `options`.
///
/// Used by the generated `CsRequest::decode` / `CsmsRequest::decode` dispatchers, and usable
/// directly when the action is known statically.
pub fn decode_payload<T: DeserializeOwned + serde::Serialize + Validate>(
    payload: &RawValue,
    options: &DecodeOptions,
) -> Result<T, DecodeError> {
    let text = payload.get();
    if text.len() > options.max_payload_size {
        return Err(DecodeError::new(
            DecodeErrorKind::Format,
            "",
            format!(
                "payload is {} bytes, limit is {}",
                text.len(),
                options.max_payload_size
            ),
        ));
    }
    // Part 4 §4.2.1: the payload of a CALL / CALLRESULT is a JSON object.
    if !text.trim_start().starts_with('{') {
        return Err(DecodeError::new(
            DecodeErrorKind::Format,
            "",
            "payload is not a JSON object",
        ));
    }

    let value: T = match parse(text) {
        Ok(value) => value,
        Err(error) if options.repairs_enabled() => repair_and_parse(text, options, &error)?,
        Err(error) => return Err(error),
    };

    if options.unknown_fields == UnknownFields::Reject {
        if let Some(path) = first_unknown_field(text, &value)? {
            return Err(DecodeError::new(
                DecodeErrorKind::UnknownField,
                path,
                "member is not defined by the schema",
            ));
        }
    }

    let mut violations = match value.validate() {
        Ok(()) => return Ok(value),
        Err(violations) => violations,
    };
    if options.unknown_enum_values == UnknownEnumValues::Preserve {
        violations.retain_kinds(|kind| kind != ViolationKind::UnknownEnumValue);
    }
    match violations.first() {
        Some(violation) => Err(DecodeError::from_violation(violation)),
        None => Ok(value),
    }
}

/// Decodes a payload and serializes the typed value straight back.
///
/// Used by [`v2_1::transcode_request`](crate::v2_1::transcode_request) and friends: what comes
/// out is what the Rust types actually model, so comparing it with the input reveals any
/// member the types drop.
pub fn transcode<T: DeserializeOwned + serde::Serialize + Validate>(
    payload: &RawValue,
    options: &DecodeOptions,
) -> Result<alloc::boxed::Box<RawValue>, DecodeError> {
    let value = decode_payload::<T>(payload, options)?;
    serde_json::value::to_raw_value(&value)
        .map_err(|error| DecodeError::new(DecodeErrorKind::Format, "", error.to_string()))
}

fn parse<T: DeserializeOwned>(text: &str) -> Result<T, DecodeError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    match serde_path_to_error::deserialize(&mut deserializer) {
        Ok(value) => Ok(value),
        Err(error) => Err(classify(&error)),
    }
}

/// Maps a `serde` failure onto the OCPP error taxonomy.
///
/// `serde`'s messages are the only structured signal available for derived impls, so the
/// mapping is pinned by tests (see the module's unit tests) — a `serde` change that altered
/// the wording would fail CI rather than silently downgrade every error to `FormatViolation`.
fn classify(error: &serde_path_to_error::Error<serde_json::Error>) -> DecodeError {
    let path = pointer_from(&error.path().to_string());
    let inner = error.inner();
    let message = inner.to_string();
    let kind = match inner.classify() {
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
            DecodeErrorKind::Format
        }
        serde_json::error::Category::Io => DecodeErrorKind::Format,
        serde_json::error::Category::Data => {
            // A missing member and a wrong array length are both occurrence constraints.
            if message.starts_with("missing field") || message.starts_with("invalid length") {
                DecodeErrorKind::Occurrence
            } else if message.starts_with("invalid type") {
                DecodeErrorKind::Type
            } else if message.starts_with("unknown field") {
                DecodeErrorKind::UnknownField
            } else if message.starts_with("duplicate field") {
                DecodeErrorKind::Format
            } else {
                // `invalid value: …` and every custom error our own impls raise
                // (undefined enum value, bad RFC 3339 timestamp).
                DecodeErrorKind::Property
            }
        }
    };
    // The trailing " at line L column C" is noise once a path is available.
    let reason = match message.rfind(" at line ") {
        Some(cut) => message[..cut].to_owned(),
        None => message,
    };
    DecodeError::new(kind, path, reason)
}

/// `serde_path_to_error` renders paths as `a.b.0.c`; OCPP error details use JSON pointers.
fn pointer_from(path: &str) -> String {
    if path.is_empty() || path == "." {
        return String::new();
    }
    let mut out = String::with_capacity(path.len() + 1);
    for segment in path.split('.') {
        out.push('/');
        out.push_str(&segment.replace('~', "~0").replace('/', "~1"));
    }
    out
}

// ---------------------------------------------------------------------------
// Leniency repairs
// ---------------------------------------------------------------------------

/// Re-parses `text` after rewriting the member the strict parse tripped over, repeating for
/// as long as progress is made and `options.max_repairs` allows.
fn repair_and_parse<T: DeserializeOwned>(
    text: &str,
    options: &DecodeOptions,
    first: &DecodeError,
) -> Result<T, DecodeError> {
    let mut json: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| DecodeError::new(DecodeErrorKind::Format, "", error.to_string()))?;

    let mut error = first.clone();
    for _ in 0..options.max_repairs {
        if !apply_repair(&mut json, &error, options) {
            return Err(error);
        }
        match serde_path_to_error::deserialize::<_, T>(&json) {
            Ok(value) => return Ok(value),
            Err(next) => {
                let next = classify(&next);
                if next == error {
                    return Err(next);
                }
                error = next;
            }
        }
    }
    Err(error)
}

/// Rewrites the single member `error` points at. Returns `false` when no repair applies.
fn apply_repair(
    json: &mut serde_json::Value,
    error: &DecodeError,
    options: &DecodeOptions,
) -> bool {
    let Some(slot) = pointer_mut(json, &error.path) else {
        return false;
    };
    let Some(text) = slot.as_str().map(ToOwned::to_owned) else {
        return false;
    };

    // A number that arrived as a string.
    if options.numeric_strings == NumericStrings::Coerce && error.kind == DecodeErrorKind::Type {
        if let Ok(number) = text.parse::<serde_json::Number>() {
            *slot = serde_json::Value::Number(number);
            return true;
        }
    }

    // A timestamp without an offset, or with a space instead of `T`.
    if options.datetime != DateTimeLeniency::Strict && error.kind == DecodeErrorKind::Property {
        let allow_space = options.datetime == DateTimeLeniency::AllowSpace;
        if !allow_space && text.contains(' ') {
            return false;
        }
        let normalized = crate::types::normalize_datetime(&text);
        if normalized != text && crate::types::DateTime::parse(&normalized).is_ok() {
            *slot = serde_json::Value::String(normalized);
            return true;
        }
    }
    false
}

/// Resolves an RFC 6901 pointer to a mutable slot.
fn pointer_mut<'a>(
    value: &'a mut serde_json::Value,
    pointer: &str,
) -> Option<&'a mut serde_json::Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    let mut current = value;
    for raw in pointer.trim_start_matches('/').split('/') {
        let key = raw.replace("~1", "/").replace("~0", "~");
        current = match current {
            serde_json::Value::Object(map) => map.get_mut(&key)?,
            serde_json::Value::Array(items) => items.get_mut(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

// ---------------------------------------------------------------------------
// Strict unknown-field detection
// ---------------------------------------------------------------------------

/// Finds the first member present on the wire but absent from the decoded value.
///
/// Implemented by re-serializing the decoded payload and diffing, which needs no schema
/// descriptors and cannot drift from the types. It runs only when
/// [`UnknownFields::Reject`] is configured.
fn first_unknown_field<T: serde::Serialize>(
    text: &str,
    value: &T,
) -> Result<Option<String>, DecodeError> {
    let wire: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| DecodeError::new(DecodeErrorKind::Format, "", error.to_string()))?;
    let ours = serde_json::to_value(value)
        .map_err(|error| DecodeError::new(DecodeErrorKind::Format, "", error.to_string()))?;
    let mut path = Vec::new();
    Ok(diff(&wire, &ours, &mut path))
}

fn diff(
    wire: &serde_json::Value,
    ours: &serde_json::Value,
    path: &mut Vec<String>,
) -> Option<String> {
    match (wire, ours) {
        (serde_json::Value::Object(w), serde_json::Value::Object(o)) => {
            for (key, value) in w {
                if value.is_null() {
                    continue; // an explicit null is indistinguishable from an absent member
                }
                path.push(key.replace('~', "~0").replace('/', "~1"));
                let found = match o.get(key) {
                    None => Some(render(path)),
                    Some(mine) => diff(value, mine, path),
                };
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        (serde_json::Value::Array(w), serde_json::Value::Array(o)) => {
            for (index, (value, mine)) in w.iter().zip(o).enumerate() {
                path.push(index.to_string());
                let found = diff(value, mine, path);
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        _ => None,
    }
}

fn render(path: &[String]) -> String {
    let mut out = String::new();
    for segment in path {
        out.push('/');
        out.push_str(segment);
    }
    out
}
