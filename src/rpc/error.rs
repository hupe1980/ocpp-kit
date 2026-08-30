//! OCPP-J error codes and `CALLERROR` payloads.

use alloc::borrow::{Cow, ToOwned};
use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

use crate::decode::{DecodeError, DecodeErrorKind};
use crate::version::Version;

/// An OCPP-J RPC framework error code (2.1 Part 4 Table 9; 1.6J Table 7).
///
/// The wire spelling is version-dependent and the differences are not cosmetic:
///
/// | Variant | 1.6J | 2.0.1 / 2.1 |
/// |---|---|---|
/// | [`FormatViolation`](Self::FormatViolation) | `FormationViolation` | `FormatViolation` |
/// | [`OccurrenceConstraintViolation`](Self::OccurrenceConstraintViolation) | `OccurenceConstraintViolation` (one `r`, as printed in the specification) | `OccurrenceConstraintViolation` |
/// | [`RpcFrameworkError`](Self::RpcFrameworkError) | not defined — sent as `GenericError` | `RpcFrameworkError` |
/// | [`MessageTypeNotSupported`](Self::MessageTypeNotSupported) | not defined — sent as `GenericError` | `MessageTypeNotSupported` |
///
/// Parsing accepts every spelling on every version, because peers mix them up constantly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// The payload is syntactically incorrect.
    FormatViolation,
    /// Any error not covered by a more specific code.
    GenericError,
    /// The receiver failed internally while processing the action.
    InternalError,
    /// A message type number the receiver does not support (2.x only).
    MessageTypeNotSupported,
    /// The requested action is not known to the receiver.
    NotImplemented,
    /// The action is known but not supported by the receiver.
    NotSupported,
    /// A field violates an occurrence constraint.
    OccurrenceConstraintViolation,
    /// A field contains an invalid value.
    PropertyConstraintViolation,
    /// The payload does not conform to the action's PDU structure.
    ProtocolError,
    /// The message is not a valid RPC request — for example the `MessageId` could not be
    /// read (2.x only).
    RpcFrameworkError,
    /// A security issue prevented the receiver from completing the action.
    SecurityError,
    /// A field violates a data-type constraint.
    TypeConstraintViolation,
    /// A code this crate does not define, kept verbatim.
    Other(String),
}

impl ErrorCode {
    /// The spelling this code uses on the wire for `version`.
    ///
    /// Codes that a version does not define are downgraded to `GenericError`, which is the
    /// only code 1.6J offers for "something else went wrong".
    #[must_use]
    pub fn as_wire(&self, version: Version) -> &str {
        let legacy = version == Version::V1_6;
        match self {
            ErrorCode::FormatViolation => {
                if legacy {
                    "FormationViolation"
                } else {
                    "FormatViolation"
                }
            }
            ErrorCode::GenericError => "GenericError",
            ErrorCode::InternalError => "InternalError",
            ErrorCode::MessageTypeNotSupported => {
                if legacy {
                    "GenericError"
                } else {
                    "MessageTypeNotSupported"
                }
            }
            ErrorCode::NotImplemented => "NotImplemented",
            ErrorCode::NotSupported => "NotSupported",
            ErrorCode::OccurrenceConstraintViolation => {
                if legacy {
                    // The 1.6J specification prints it with a single `r`.
                    "OccurenceConstraintViolation"
                } else {
                    "OccurrenceConstraintViolation"
                }
            }
            ErrorCode::PropertyConstraintViolation => "PropertyConstraintViolation",
            ErrorCode::ProtocolError => "ProtocolError",
            ErrorCode::RpcFrameworkError => {
                if legacy {
                    "GenericError"
                } else {
                    "RpcFrameworkError"
                }
            }
            ErrorCode::SecurityError => "SecurityError",
            ErrorCode::TypeConstraintViolation => "TypeConstraintViolation",
            ErrorCode::Other(code) => code.as_str(),
        }
    }

    /// Parses a wire code, accepting every version's spelling.
    #[must_use]
    pub fn parse(code: &str) -> Self {
        match code {
            "FormatViolation" | "FormationViolation" => ErrorCode::FormatViolation,
            "GenericError" => ErrorCode::GenericError,
            "InternalError" => ErrorCode::InternalError,
            "MessageTypeNotSupported" => ErrorCode::MessageTypeNotSupported,
            "NotImplemented" => ErrorCode::NotImplemented,
            "NotSupported" => ErrorCode::NotSupported,
            "OccurrenceConstraintViolation" | "OccurenceConstraintViolation" => {
                ErrorCode::OccurrenceConstraintViolation
            }
            "PropertyConstraintViolation" => ErrorCode::PropertyConstraintViolation,
            "ProtocolError" => ErrorCode::ProtocolError,
            "RpcFrameworkError" => ErrorCode::RpcFrameworkError,
            "SecurityError" => ErrorCode::SecurityError,
            "TypeConstraintViolation" => ErrorCode::TypeConstraintViolation,
            other => ErrorCode::Other(other.to_owned()),
        }
    }

    /// Whether `version` defines this code at all.
    #[must_use]
    pub fn is_defined_in(&self, version: Version) -> bool {
        match self {
            ErrorCode::RpcFrameworkError | ErrorCode::MessageTypeNotSupported => {
                version != Version::V1_6
            }
            ErrorCode::Other(_) => false,
            _ => true,
        }
    }
}

impl fmt::Display for ErrorCode {
    /// Uses the 2.x spelling; call [`as_wire`](Self::as_wire) when the version matters.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire(Version::V2_1))
    }
}

/// The longest `errorDescription` the specification allows (Part 4, Table 7).
pub const ERROR_DESCRIPTION_MAX_LEN: usize = 255;

/// Truncates a description to the length the wire format allows.
///
/// Descriptions are built from decoding failures, which quote paths and values and can run
/// long. Emitting an over-long one would be a violation in the very message that reports a
/// violation.
fn cap_description(text: &str) -> String {
    match text.char_indices().nth(ERROR_DESCRIPTION_MAX_LEN) {
        Some((cut, _)) => text[..cut].to_owned(),
        None => text.to_owned(),
    }
}

/// The body of a `CALLERROR` (or, in 2.1, a `CALLRESULTERROR`).
#[derive(Clone, Debug, PartialEq)]
pub struct CallError {
    /// The RPC framework error code.
    pub code: ErrorCode,
    /// Short human-readable description. Empty is allowed; the spec suggests `""`.
    pub description: String,
    /// Free-form details object. Never `null` on the wire — an empty object is used.
    pub details: serde_json::Value,
}

impl CallError {
    /// Builds a `CALLERROR` body with an empty details object.
    #[must_use]
    pub fn new(code: ErrorCode, description: impl Into<String>) -> Self {
        Self {
            code,
            description: cap_description(&description.into()),
            details: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Attaches a details object.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    /// The action is not known to this implementation.
    #[must_use]
    pub fn not_implemented(action: &str) -> Self {
        Self::new(
            ErrorCode::NotImplemented,
            format!("unknown action {action:?}"),
        )
    }

    /// The action is known but this peer does not handle it.
    #[must_use]
    pub fn not_supported(action: &str) -> Self {
        Self::new(
            ErrorCode::NotSupported,
            format!("action {action:?} is not supported"),
        )
    }

    /// The handler failed.
    #[must_use]
    pub fn internal(description: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, description)
    }

    /// A security check failed while processing the action.
    #[must_use]
    pub fn security(description: impl Into<String>) -> Self {
        Self::new(ErrorCode::SecurityError, description)
    }
}

impl From<DecodeError> for CallError {
    /// Maps a decoding failure onto the error code the specification prescribes, and puts
    /// the offending JSON pointer into `errorDetails` so the peer can find it.
    fn from(error: DecodeError) -> Self {
        let code = match error.kind {
            DecodeErrorKind::Format => ErrorCode::FormatViolation,
            DecodeErrorKind::Occurrence => ErrorCode::OccurrenceConstraintViolation,
            DecodeErrorKind::Type => ErrorCode::TypeConstraintViolation,
            DecodeErrorKind::Property => ErrorCode::PropertyConstraintViolation,
            // "Payload for Action is not conform the PDU structure" — an undefined member is
            // exactly that, since the 2.x schemas set `additionalProperties: false`.
            DecodeErrorKind::UnknownField | DecodeErrorKind::Protocol => ErrorCode::ProtocolError,
            DecodeErrorKind::UnsupportedAction => ErrorCode::NotSupported,
        };
        let mut details = serde_json::Map::new();
        if !error.path.is_empty() {
            details.insert(
                "path".to_string(),
                serde_json::Value::String(error.path.clone()),
            );
        }
        details.insert(
            "reason".to_string(),
            serde_json::Value::String(error.reason.clone()),
        );
        Self {
            code,
            description: cap_description(&error.to_string()),
            details: serde_json::Value::Object(details),
        }
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.description.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.description)
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CallError {}

/// Borrowed form of [`CallError`], as produced by frame parsing.
///
/// The details object stays unparsed so a Local Controller can relay it untouched.
#[derive(Clone, Debug)]
pub struct CallErrorRef<'a> {
    /// The RPC framework error code.
    pub code: ErrorCode,
    /// Short human-readable description.
    pub description: Cow<'a, str>,
    /// Free-form details object, still unparsed.
    pub details: Cow<'a, serde_json::value::RawValue>,
}

impl CallErrorRef<'_> {
    /// Copies the borrowed error into an owned [`CallError`].
    #[must_use]
    pub fn to_call_error(&self) -> CallError {
        CallError {
            code: self.code.clone(),
            description: self.description.clone().into_owned(),
            details: serde_json::from_str(self.details.get())
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        }
    }

    /// Detaches the error from the buffer it was parsed from.
    #[must_use]
    pub fn into_owned(self) -> CallErrorRef<'static> {
        CallErrorRef {
            code: self.code,
            description: Cow::Owned(self.description.into_owned()),
            details: Cow::Owned(self.details.into_owned()),
        }
    }
}

impl PartialEq for CallErrorRef<'_> {
    /// Compares the details object by its raw JSON text.
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
            && self.description == other.description
            && self.details.get() == other.details.get()
    }
}

impl From<CallError> for CallErrorRef<'static> {
    fn from(error: CallError) -> Self {
        CallErrorRef {
            code: error.code,
            description: Cow::Owned(cap_description(&error.description)),
            details: Cow::Owned(
                serde_json::value::to_raw_value(&error.details).unwrap_or_else(|_| {
                    serde_json::value::RawValue::from_string("{}".to_string()).expect("valid")
                }),
            ),
        }
    }
}

impl From<&CallError> for CallErrorRef<'static> {
    fn from(error: &CallError) -> Self {
        CallErrorRef {
            code: error.code.clone(),
            description: Cow::Owned(cap_description(&error.description)),
            details: Cow::Owned(
                serde_json::value::to_raw_value(&error.details).unwrap_or_else(|_| {
                    serde_json::value::RawValue::from_string("{}".to_string()).expect("valid")
                }),
            ),
        }
    }
}
