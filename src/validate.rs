//! Constraint validation, kept deliberately separate from `serde`.
//!
//! OCPP's JSON schemas carry constraints that Rust's type system cannot express
//! (`maxLength`, `minItems`, numeric ranges, closed enumerations). Running them inside
//! `Deserialize` would make every failure a `serde` error with no structure, and would tax
//! the hot path. Instead, deserialization is pure shape-checking and validation is a second,
//! explicit pass whose output is a list of [`Violation`]s carrying a JSON pointer and a
//! category — which is exactly what the OCPP error codes are keyed on
//! (see [`crate::rpc::ErrorCode`]).

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

/// One step of a JSON pointer into a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathSegment {
    /// An object member, always a schema-defined name.
    Key(&'static str),
    /// An array index.
    Index(usize),
}

/// A cursor into the payload being validated.
///
/// The path is only rendered into a [`String`] when a violation is actually recorded, so
/// validating a valid payload allocates nothing beyond the (reusable) segment stack.
#[derive(Clone, Debug, Default)]
pub struct ValidationPath {
    segments: Vec<PathSegment>,
}

impl ValidationPath {
    /// An empty path, pointing at the payload root.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Descends into an object member.
    pub fn push_key(&mut self, key: &'static str) {
        self.segments.push(PathSegment::Key(key));
    }

    /// Descends into an array element.
    pub fn push_index(&mut self, index: usize) {
        self.segments.push(PathSegment::Index(index));
    }

    /// Returns to the parent.
    pub fn pop(&mut self) {
        self.segments.pop();
    }

    /// Renders the current position as an RFC 6901 JSON pointer, e.g. `/evse/0/connectorId`.
    #[must_use]
    pub fn to_pointer(&self) -> String {
        if self.segments.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for segment in &self.segments {
            out.push('/');
            match segment {
                PathSegment::Key(k) => {
                    // RFC 6901 escaping; OCPP member names never contain these, but be exact.
                    out.push_str(&k.replace('~', "~0").replace('/', "~1"));
                }
                PathSegment::Index(i) => {
                    out.push_str(&i.to_string());
                }
            }
        }
        out
    }
}

/// Why a payload is invalid. Maps 1:1 onto the OCPP error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ViolationKind {
    /// A required member is absent, or an array has too few/too many elements.
    ///
    /// → `OccurrenceConstraintViolation`.
    Occurrence,
    /// A member has the wrong JSON type.
    ///
    /// → `TypeConstraintViolation`.
    Type,
    /// A value is out of range, too long, or otherwise outside its constraint.
    ///
    /// → `PropertyConstraintViolation`.
    Property,
    /// A string was accepted into an enumeration's open `UnknownValue` variant.
    ///
    /// → `PropertyConstraintViolation`, unless
    /// [`DecodeOptions::unknown_enum_values`](crate::decode::DecodeOptions::unknown_enum_values)
    /// says to preserve it.
    UnknownEnumValue,
    /// A member that the schema does not define was present.
    ///
    /// → `PropertyConstraintViolation`, unless
    /// [`DecodeOptions::unknown_fields`](crate::decode::DecodeOptions::unknown_fields)
    /// says to ignore it.
    UnknownField,
    /// The payload is individually well-formed but breaks a cross-field rule.
    ///
    /// → `ProtocolError`.
    Protocol,
}

impl fmt::Display for ViolationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ViolationKind::Occurrence => "occurrence",
            ViolationKind::Type => "type",
            ViolationKind::Property => "property",
            ViolationKind::UnknownEnumValue => "unknown enum value",
            ViolationKind::UnknownField => "unknown field",
            ViolationKind::Protocol => "protocol",
        })
    }
}

/// A single constraint failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// RFC 6901 pointer to the offending member, e.g. `/chargingStation/model`.
    pub path: String,
    /// Category, which decides the OCPP error code.
    pub kind: ViolationKind,
    /// Human-readable reason, e.g. `maxLength 20 exceeded (got 27)`.
    pub reason: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.path, self.reason)
        }
    }
}

/// The complete result of validating one payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Violations {
    items: Vec<Violation>,
}

impl Violations {
    /// An empty violation list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    /// Whether the payload is free of violations.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    /// How many violations were recorded.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    /// All recorded violations.
    pub fn as_slice(&self) -> &[Violation] {
        &self.items
    }

    /// The first violation, which is the one reported back to the peer.
    #[must_use]
    pub fn first(&self) -> Option<&Violation> {
        self.items.first()
    }

    /// Records one violation at the current position.
    pub fn push(&mut self, path: &ValidationPath, kind: ViolationKind, reason: impl Into<String>) {
        self.items.push(Violation {
            path: path.to_pointer(),
            kind,
            reason: reason.into(),
        });
    }

    /// Drops violations of kinds the caller has opted to tolerate.
    pub fn retain_kinds(&mut self, mut keep: impl FnMut(ViolationKind) -> bool) {
        self.items.retain(|v| keep(v.kind));
    }

    #[must_use]
    /// Unwraps the recorded violations.
    pub fn into_vec(self) -> Vec<Violation> {
        self.items
    }
}

impl IntoIterator for Violations {
    type Item = Violation;
    type IntoIter = alloc::vec::IntoIter<Violation>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl fmt::Display for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, v) in self.items.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{v}")?;
        }
        Ok(())
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Violations {}

/// Checks the constraints the JSON schema places on a value.
///
/// Implemented by every generated payload and data type.
pub trait Validate {
    /// Appends this value's violations to `out`, relative to `path`.
    fn validate_at(&self, path: &mut ValidationPath, out: &mut Violations);

    /// Validates this value from the payload root.
    fn validate(&self) -> Result<(), Violations> {
        let mut path = ValidationPath::new();
        let mut out = Violations::new();
        self.validate_at(&mut path, &mut out);
        if out.is_empty() { Ok(()) } else { Err(out) }
    }
}

impl<T: Validate> Validate for Option<T> {
    fn validate_at(&self, path: &mut ValidationPath, out: &mut Violations) {
        if let Some(v) = self {
            v.validate_at(path, out);
        }
    }
}

impl<T: Validate> Validate for Vec<T> {
    fn validate_at(&self, path: &mut ValidationPath, out: &mut Violations) {
        for (index, item) in self.iter().enumerate() {
            path.push_index(index);
            item.validate_at(path, out);
            path.pop();
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers used by generated code.
// ---------------------------------------------------------------------------

/// `minLength` / `maxLength`, counted in characters as the schemas intend.
pub fn string(
    value: &str,
    min: Option<u32>,
    max: Option<u32>,
    path: &ValidationPath,
    out: &mut Violations,
) {
    let len = value.chars().count();
    if let Some(max) = max {
        if len > max as usize {
            out.push(
                path,
                ViolationKind::Property,
                format!("maxLength {max} exceeded (got {len} characters)"),
            );
        }
    }
    if let Some(min) = min {
        if len < min as usize {
            out.push(
                path,
                ViolationKind::Property,
                format!("minLength {min} not reached (got {len} characters)"),
            );
        }
    }
}

/// `minimum` / `maximum` for integers.
pub fn int_range(
    value: i64,
    min: Option<f64>,
    max: Option<f64>,
    path: &ValidationPath,
    out: &mut Violations,
) {
    #[allow(clippy::cast_precision_loss)]
    range(value as f64, min, max, path, out);
}

/// `minimum` / `maximum` for numbers, and the JSON-representability of the value.
pub fn range(
    value: f64,
    min: Option<f64>,
    max: Option<f64>,
    path: &ValidationPath,
    out: &mut Violations,
) {
    if !value.is_finite() {
        out.push(
            path,
            ViolationKind::Type,
            "value is not a finite JSON number".to_owned(),
        );
        return;
    }
    if let Some(min) = min {
        if value < min {
            out.push(
                path,
                ViolationKind::Property,
                format!("minimum {min} violated (got {value})"),
            );
        }
    }
    if let Some(max) = max {
        if value > max {
            out.push(
                path,
                ViolationKind::Property,
                format!("maximum {max} violated (got {value})"),
            );
        }
    }
}

/// `minItems` / `maxItems`.
pub fn list_len(
    len: usize,
    min: Option<u32>,
    max: Option<u32>,
    path: &ValidationPath,
    out: &mut Violations,
) {
    if let Some(min) = min {
        if len < min as usize {
            out.push(
                path,
                ViolationKind::Occurrence,
                format!("minItems {min} not reached (got {len})"),
            );
        }
    }
    if let Some(max) = max {
        if len > max as usize {
            out.push(
                path,
                ViolationKind::Occurrence,
                format!("maxItems {max} exceeded (got {len})"),
            );
        }
    }
}

/// Records a value that was accepted into an enumeration's open `UnknownValue` variant.
pub fn unknown_enum_value(
    value: &str,
    type_name: &'static str,
    path: &ValidationPath,
    out: &mut Violations,
) {
    out.push(
        path,
        ViolationKind::UnknownEnumValue,
        format!("{value:?} is not a defined value of {type_name}"),
    );
}
