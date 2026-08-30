//! OCPP-J frame parsing and serialization (Part 4 §4.2, 1.6J §4.2).
//!
//! Parsing is two-stage and zero-copy: the JSON array skeleton is split into elements while
//! the payload stays an unparsed [`RawValue`]. The payload is only deserialized once the
//! action *and* the direction are known — which is what makes a Local Controller able to
//! relay a signed message byte-for-byte, and what keeps the "unknown action" path from
//! paying for a payload parse it will throw away.

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use core::fmt;

use serde_json::value::RawValue;

use crate::types::MessageId;
use crate::version::Version;

use super::error::{CallErrorRef, ErrorCode};

/// The OCPP-J message type numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageTypeId {
    /// `CALL` — a request.
    Call = 2,
    /// `CALLRESULT` — a successful answer.
    CallResult = 3,
    /// `CALLERROR` — a failed answer.
    CallError = 4,
    /// `CALLRESULTERROR` — the answer itself was unusable (OCPP 2.1).
    CallResultError = 5,
    /// `SEND` — an unconfirmed message that is never answered (OCPP 2.1).
    Send = 6,
}

impl MessageTypeId {
    /// Maps a message type number, if it is one of the five defined types.
    #[must_use]
    pub const fn from_number(number: u8) -> Option<Self> {
        Some(match number {
            2 => MessageTypeId::Call,
            3 => MessageTypeId::CallResult,
            4 => MessageTypeId::CallError,
            5 => MessageTypeId::CallResultError,
            6 => MessageTypeId::Send,
            _ => return None,
        })
    }

    /// The message type number.
    #[must_use]
    pub const fn number(self) -> u8 {
        self as u8
    }

    /// Whether `version` defines this message type.
    ///
    /// `CALLRESULTERROR` and `SEND` are new in 2.1 (Part 4 §4.2).
    #[must_use]
    pub const fn is_defined_in(self, version: Version) -> bool {
        match self {
            MessageTypeId::CallResultError | MessageTypeId::Send => {
                version.has_extended_message_types()
            }
            _ => true,
        }
    }
}

impl fmt::Display for MessageTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MessageTypeId::Call => "CALL",
            MessageTypeId::CallResult => "CALLRESULT",
            MessageTypeId::CallError => "CALLERROR",
            MessageTypeId::CallResultError => "CALLRESULTERROR",
            MessageTypeId::Send => "SEND",
        })
    }
}

/// One OCPP-J frame.
///
/// Borrows from the text it was parsed from; [`Frame::into_owned`] detaches it.
#[derive(Clone, Debug)]
pub enum Frame<'a> {
    /// `[2, "<id>", "<action>", {payload}]`
    Call {
        /// Correlation id.
        id: MessageId,
        /// Action name, exactly as received.
        action: Cow<'a, str>,
        /// Unparsed payload object.
        payload: Cow<'a, RawValue>,
    },
    /// `[3, "<id>", {payload}]`
    CallResult {
        /// The id of the `CALL` being answered.
        id: MessageId,
        /// Unparsed payload object.
        payload: Cow<'a, RawValue>,
    },
    /// `[4, "<id>", "<code>", "<description>", {details}]`
    CallError {
        /// The id of the `CALL` being answered.
        id: MessageId,
        /// The error.
        error: CallErrorRef<'a>,
    },
    /// `[5, "<id>", "<code>", "<description>", {details}]` — OCPP 2.1.
    ///
    /// Sent when a received `CALLRESULT` could not be processed, so the original sender
    /// learns that its request did not really succeed.
    CallResultError {
        /// The id of the `CALLRESULT` being rejected.
        id: MessageId,
        /// The error.
        error: CallErrorRef<'a>,
    },
    /// `[6, "<id>", "<action>", {payload}]` — OCPP 2.1.
    ///
    /// Unconfirmed: it is never answered, and it may be transmitted while a `CALL` is
    /// outstanding (Part 4 §4.2.4).
    Send {
        /// Identifies the message; never correlated with a response.
        id: MessageId,
        /// Action name, exactly as received.
        action: Cow<'a, str>,
        /// Unparsed payload object.
        payload: Cow<'a, RawValue>,
    },
}

impl PartialEq for Frame<'_> {
    /// Payloads compare by their raw JSON text, so a frame is equal to itself after a
    /// parse/serialize round trip only when the bytes match.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Frame::Call {
                    id: a,
                    action: aa,
                    payload: ap,
                },
                Frame::Call {
                    id: b,
                    action: ba,
                    payload: bp,
                },
            )
            | (
                Frame::Send {
                    id: a,
                    action: aa,
                    payload: ap,
                },
                Frame::Send {
                    id: b,
                    action: ba,
                    payload: bp,
                },
            ) => a == b && aa == ba && ap.get() == bp.get(),
            (
                Frame::CallResult { id: a, payload: ap },
                Frame::CallResult { id: b, payload: bp },
            ) => a == b && ap.get() == bp.get(),
            (Frame::CallError { id: a, error: ae }, Frame::CallError { id: b, error: be })
            | (
                Frame::CallResultError { id: a, error: ae },
                Frame::CallResultError { id: b, error: be },
            ) => a == b && ae == be,
            _ => false,
        }
    }
}

impl<'a> Frame<'a> {
    /// The message type of this frame.
    #[must_use]
    pub const fn message_type(&self) -> MessageTypeId {
        match self {
            Frame::Call { .. } => MessageTypeId::Call,
            Frame::CallResult { .. } => MessageTypeId::CallResult,
            Frame::CallError { .. } => MessageTypeId::CallError,
            Frame::CallResultError { .. } => MessageTypeId::CallResultError,
            Frame::Send { .. } => MessageTypeId::Send,
        }
    }

    /// The correlation id.
    #[must_use]
    pub fn id(&self) -> &MessageId {
        match self {
            Frame::Call { id, .. }
            | Frame::CallResult { id, .. }
            | Frame::CallError { id, .. }
            | Frame::CallResultError { id, .. }
            | Frame::Send { id, .. } => id,
        }
    }

    /// The action name, for the frame types that carry one.
    #[must_use]
    pub fn action(&self) -> Option<&str> {
        match self {
            Frame::Call { action, .. } | Frame::Send { action, .. } => Some(action),
            _ => None,
        }
    }

    /// The unparsed payload, for the frame types that carry one.
    #[must_use]
    pub fn payload(&self) -> Option<&RawValue> {
        match self {
            Frame::Call { payload, .. }
            | Frame::CallResult { payload, .. }
            | Frame::Send { payload, .. } => Some(payload),
            _ => None,
        }
    }

    /// Parses one OCPP-J text frame.
    ///
    /// `version` decides which message types are accepted; a frame whose type number is not
    /// defined at all yields [`FrameError::UnknownMessageType`], which the caller must handle
    /// per version (2.0.1 answers `MessageTypeNotSupported`, 1.6J and 2.1 ignore it).
    pub fn parse(text: &'a str, version: Version) -> Result<Self, FrameError> {
        let elements: alloc::vec::Vec<&'a RawValue> =
            serde_json::from_str(text).map_err(|_| FrameError::NotAnArray)?;

        // Read the type element as a full-width integer: `[300, …]` is a badly chosen
        // *number*, not a frame that stopped being an array, and §4.4 answers it differently.
        let number = elements
            .first()
            .and_then(|e| serde_json::from_str::<i64>(e.get()).ok())
            .ok_or(FrameError::NotAnArray)?;

        let Some(message_type) = u8::try_from(number)
            .ok()
            .and_then(MessageTypeId::from_number)
        else {
            return Err(FrameError::UnknownMessageType {
                number: raw_number(&elements),
            });
        };
        if !message_type.is_defined_in(version) {
            return Err(FrameError::MessageTypeNotInVersion {
                message_type,
                version,
            });
        }

        // Extra trailing elements are tolerated: 1.6J and 2.x both say "at least", and a
        // peer that appends a future element must not take the connection down.
        //
        // The two error frames also tolerate a *short* tail. §4.2.3 requires five elements,
        // but implementations routinely omit `errorDescription`, `errorDetails` or both, and
        // refusing one leaves the CALL it answers outstanding until the message timeout.
        let (arity, minimum) = match message_type {
            MessageTypeId::Call | MessageTypeId::Send => (4, 4),
            MessageTypeId::CallResult => (3, 3),
            MessageTypeId::CallError | MessageTypeId::CallResultError => (5, 3),
        };
        if elements.len() < minimum {
            return Err(FrameError::WrongArity {
                message_type,
                found: elements.len(),
                expected: arity,
            });
        }

        let id = string_at(&elements, 1).ok_or(FrameError::UnreadableMessageId { message_type })?;
        if id.is_empty() {
            return Err(FrameError::UnreadableMessageId { message_type });
        }
        let id = MessageId::from_wire(&id);

        Ok(match message_type {
            MessageTypeId::Call | MessageTypeId::Send => {
                let action = string_at(&elements, 2)
                    .ok_or(FrameError::UnreadableAction { id: id.clone() })?;
                let payload = Cow::Borrowed(elements[3]);
                if message_type == MessageTypeId::Call {
                    Frame::Call {
                        id,
                        action: Cow::Owned(action),
                        payload,
                    }
                } else {
                    Frame::Send {
                        id,
                        action: Cow::Owned(action),
                        payload,
                    }
                }
            }
            MessageTypeId::CallResult => Frame::CallResult {
                id,
                payload: Cow::Borrowed(elements[2]),
            },
            MessageTypeId::CallError | MessageTypeId::CallResultError => {
                let code = string_at(&elements, 2)
                    .ok_or_else(|| FrameError::UnreadableErrorCode { id: id.clone() })?;
                let description = string_at(&elements, 3).unwrap_or_default();
                let details = match elements.get(4) {
                    Some(raw) => Cow::Borrowed(*raw),
                    // §4.2.3 requires `{}` when there are no details; a peer that left the
                    // element out meant exactly that.
                    None => Cow::Owned(
                        RawValue::from_string(String::from("{}")).expect("`{}` is valid JSON"),
                    ),
                };
                let error = CallErrorRef {
                    code: ErrorCode::parse(&code),
                    description: Cow::Owned(description),
                    details,
                };
                if message_type == MessageTypeId::CallError {
                    Frame::CallError { id, error }
                } else {
                    Frame::CallResultError { id, error }
                }
            }
        })
    }

    /// Serializes the frame using `version`'s spelling of the error codes.
    pub fn to_json(&self, version: Version) -> Result<String, serde_json::Error> {
        match self {
            Frame::Call {
                id,
                action,
                payload,
            } => serde_json::to_string(&(2u8, id.as_str(), action.as_ref(), payload.as_ref())),
            Frame::CallResult { id, payload } => {
                serde_json::to_string(&(3u8, id.as_str(), payload.as_ref()))
            }
            Frame::CallError { id, error } => serde_json::to_string(&(
                4u8,
                id.as_str(),
                error.code.as_wire(version),
                error.description.as_ref(),
                error.details.as_ref(),
            )),
            Frame::CallResultError { id, error } => serde_json::to_string(&(
                5u8,
                id.as_str(),
                error.code.as_wire(version),
                error.description.as_ref(),
                error.details.as_ref(),
            )),
            Frame::Send {
                id,
                action,
                payload,
            } => serde_json::to_string(&(6u8, id.as_str(), action.as_ref(), payload.as_ref())),
        }
    }

    /// Detaches the frame from the buffer it was parsed from.
    #[must_use]
    pub fn into_owned(self) -> Frame<'static> {
        fn own(value: Cow<'_, RawValue>) -> Cow<'static, RawValue> {
            Cow::Owned(value.into_owned())
        }
        match self {
            Frame::Call {
                id,
                action,
                payload,
            } => Frame::Call {
                id,
                action: Cow::Owned(action.into_owned()),
                payload: own(payload),
            },
            Frame::CallResult { id, payload } => Frame::CallResult {
                id,
                payload: own(payload),
            },
            Frame::Send {
                id,
                action,
                payload,
            } => Frame::Send {
                id,
                action: Cow::Owned(action.into_owned()),
                payload: own(payload),
            },
            Frame::CallError { id, error } => Frame::CallError {
                id,
                error: error.into_owned(),
            },
            Frame::CallResultError { id, error } => Frame::CallResultError {
                id,
                error: error.into_owned(),
            },
        }
    }
}

fn string_at(elements: &[&RawValue], index: usize) -> Option<String> {
    let raw = elements.get(index)?;
    serde_json::from_str::<String>(raw.get()).ok()
}

fn raw_number(elements: &[&RawValue]) -> String {
    elements
        .first()
        .map_or_else(|| "?".to_string(), |e| e.get().to_string())
}

/// Why a text frame could not be turned into a [`Frame`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// Not valid JSON, or not a JSON array with a numeric message type first.
    NotAnArray,
    /// The array has fewer elements than the message type requires.
    WrongArity {
        /// The message type that was read.
        message_type: MessageTypeId,
        /// How many elements the array had.
        found: usize,
        /// How many it needed.
        expected: usize,
    },
    /// A message type number outside 2–6.
    ///
    /// Part 4 §4.4: 1.6J and 2.1 ignore the message; 2.0.1 answers `MessageTypeNotSupported`.
    UnknownMessageType {
        /// The raw JSON of the type element.
        number: String,
    },
    /// A message type that exists in OCPP but not in the negotiated version
    /// (`CALLRESULTERROR` and `SEND` before 2.1).
    MessageTypeNotInVersion {
        /// The message type that was read.
        message_type: MessageTypeId,
        /// The negotiated version.
        version: Version,
    },
    /// The `MessageId` element is missing, empty, or not a JSON string.
    ///
    /// 2.x answers with id `"-1"` (Part 4 §4.1.1); the message type decides *which* answer,
    /// so it is kept even though the id could not be read.
    UnreadableMessageId {
        /// The message type that was read.
        message_type: MessageTypeId,
    },
    /// The action element is not a JSON string.
    UnreadableAction {
        /// The id to answer with.
        id: MessageId,
    },
    /// The error-code element is not a JSON string.
    UnreadableErrorCode {
        /// The id to answer with.
        id: MessageId,
    },
}

/// How a frame that could not be parsed must be answered (Part 4 §4.2.3).
///
/// `CALLERROR` "is sent in response to a CALL", `CALLRESULTERROR` "on receipt of a CALLRESULT
/// that contains errors" — and an unparseable error frame gets neither, or two peers end up
/// trading error frames over a message that was already an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameReply {
    /// Say nothing. The frame was a `SEND`, or was itself an error frame, or the version
    /// says to ignore it.
    Ignore,
    /// Answer with a `CALLERROR`.
    CallError,
    /// Answer with a `CALLRESULTERROR` (OCPP 2.1 only; before that the failure stays local).
    CallResultError,
}

impl FrameError {
    /// The message type of the frame that failed to parse, when it got far enough to be read.
    #[must_use]
    pub fn message_type(&self) -> Option<MessageTypeId> {
        match self {
            FrameError::WrongArity { message_type, .. }
            | FrameError::MessageTypeNotInVersion { message_type, .. }
            | FrameError::UnreadableMessageId { message_type } => Some(*message_type),
            _ => None,
        }
    }

    /// What the receiver must send back, per Part 4 §4.2.3.
    #[must_use]
    pub fn reply(&self, version: Version) -> FrameReply {
        if self.is_ignorable(version) {
            return FrameReply::Ignore;
        }
        // A type this version does not define is, to this version, an unknown type number,
        // so §4.4 applies unchanged and `message_type()` must not be consulted: a type-6 frame
        // on 2.0.1 is not "a SEND", because 2.0.1 has no SEND to recognise it as.
        if matches!(self, FrameError::MessageTypeNotInVersion { .. }) {
            return FrameReply::CallError;
        }
        match self.message_type() {
            // "A CALLRESULTERROR is sent back on receipt of a CALLRESULT that contains
            // errors" — and only 2.1 has one to send.
            Some(MessageTypeId::CallResult) => {
                if version.has_extended_message_types() {
                    FrameReply::CallResultError
                } else {
                    FrameReply::Ignore
                }
            }
            // A SEND is never answered (§4.2.4, FR.07), and a broken error frame has no
            // answer defined for it.
            Some(
                MessageTypeId::Send | MessageTypeId::CallError | MessageTypeId::CallResultError,
            ) => FrameReply::Ignore,
            // A CALL, or a frame too broken to classify: §4.2.3's "-1" case.
            Some(MessageTypeId::Call) | None => FrameReply::CallError,
        }
    }

    /// Whether the specification says to silently ignore this frame rather than answer it.
    ///
    /// True only for a message type this version does not define, on 1.6J (§4.1.3) and 2.1
    /// (§4.4). OCPP 2.0.1 §4.4 instead requires a `CALLERROR: MessageTypeNotSupported`.
    #[must_use]
    pub fn is_ignorable(&self, version: Version) -> bool {
        matches!(
            self,
            FrameError::UnknownMessageType { .. } | FrameError::MessageTypeNotInVersion { .. }
        ) && !matches!(version, Version::V2_0_1)
    }

    /// The `CALLERROR` code to answer with, if the frame must be answered at all.
    #[must_use]
    pub fn error_code(&self) -> ErrorCode {
        match self {
            FrameError::UnknownMessageType { .. } | FrameError::MessageTypeNotInVersion { .. } => {
                ErrorCode::MessageTypeNotSupported
            }
            _ => ErrorCode::RpcFrameworkError,
        }
    }

    /// The id to use in the answer. `"-1"` when the incoming id could not be read.
    #[must_use]
    pub fn reply_id(&self) -> MessageId {
        match self {
            FrameError::UnreadableAction { id } | FrameError::UnreadableErrorCode { id } => {
                id.clone()
            }
            _ => MessageId::unreadable(),
        }
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::NotAnArray => f.write_str("frame is not a JSON array with a message type"),
            FrameError::WrongArity {
                message_type,
                found,
                expected,
            } => {
                write!(f, "{message_type} needs {expected} elements, got {found}")
            }
            FrameError::UnknownMessageType { number } => {
                write!(f, "unknown message type number {number}")
            }
            FrameError::MessageTypeNotInVersion {
                message_type,
                version,
            } => {
                write!(f, "{message_type} is not defined in OCPP {version}")
            }
            FrameError::UnreadableMessageId { message_type } => {
                write!(f, "the MessageId of a {message_type} could not be read")
            }
            FrameError::UnreadableAction { .. } => f.write_str("action could not be read"),
            FrameError::UnreadableErrorCode { .. } => f.write_str("error code could not be read"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FrameError {}
