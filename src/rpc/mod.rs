//! Layer 1 — OCPP-J framing.
//!
//! This layer knows about `CALL` / `CALLRESULT` / `CALLERROR` / `CALLRESULTERROR` / `SEND`,
//! about message ids and about error codes. It knows nothing about actions, payload schemas,
//! sockets or state — [`Frame`] in, [`Frame`] out.
//!
//! # Version differences this layer encodes
//!
//! | | 1.6J | 2.0.1 | 2.1 |
//! |---|---|---|---|
//! | Message types | 2, 3, 4 | 2, 3, 4 | + 5 (`CALLRESULTERROR`), 6 (`SEND`) |
//! | Unknown message type number | ignore the payload (§4.1.3) | answer `MessageTypeNotSupported` (§4.4) | ignore the payload (§4.4) |
//! | Unreadable `MessageId` | no rule | answer with id `"-1"` | answer with id `"-1"` |
//! | "Syntactically incorrect" | `FormationViolation` | `FormatViolation` | `FormatViolation` |
//! | Occurrence violation | `OccurenceConstraintViolation` | `OccurrenceConstraintViolation` | `OccurrenceConstraintViolation` |

mod error;
mod frame;
#[cfg(feature = "signed-messages")]
pub mod signed;

pub use crate::RawValue;
pub use error::{CallError, CallErrorRef, ErrorCode};
pub use frame::{Frame, FrameError, FrameReply, MessageTypeId};
