//! The traits that tie a payload type to its action, direction and message kind.

use core::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::validate::{Validate, ValidationPath, Violations};

/// Prevents downstream crates from implementing the payload traits, so new actions and new
/// versions are not breaking changes for them.
pub trait Sealed {}

/// Whether a message is a confirmed `CALL` or an unconfirmed `SEND`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// A `CALL` (message type 2), answered with `CALLRESULT`, `CALLERROR` or —
    /// in 2.1 — `CALLRESULTERROR`.
    Call,
    /// A `SEND` (message type 6, OCPP 2.1 only): never answered, and exempt from the
    /// one-outstanding-`CALL` rule (Part 4 §4.2.4, FR.07).
    Send,
}

/// Which peer originates an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// Charging Station → CSMS.
    ChargingStation,
    /// CSMS → Charging Station.
    Csms,
    /// Either peer may originate it. Only `DataTransfer`.
    Both,
}

impl Origin {
    /// Whether a Charging Station may send this action.
    #[must_use]
    pub const fn from_charging_station(self) -> bool {
        matches!(self, Origin::ChargingStation | Origin::Both)
    }

    /// Whether a CSMS may send this action.
    #[must_use]
    pub const fn from_csms(self) -> bool {
        matches!(self, Origin::Csms | Origin::Both)
    }
}

/// The behaviour every generated per-version `Action` enum provides.
pub trait ActionName:
    Copy + Eq + Ord + core::hash::Hash + fmt::Debug + fmt::Display + Sized + 'static
{
    /// Every action this version defines, in specification order.
    fn all() -> &'static [Self];

    /// The action name as it appears on the wire.
    fn as_str(&self) -> &'static str;

    /// Parses a wire action name; `None` if this version does not define it.
    fn from_wire(name: &str) -> Option<Self>;

    /// Whether the action is a `CALL` or a `SEND`.
    fn kind(&self) -> MessageKind;

    /// Which peer originates the action.
    fn origin(&self) -> Origin;

    /// The functional block (2.x) or feature profile (1.6) the action belongs to.
    fn block(&self) -> &'static str;
}

/// A request payload.
pub trait Request: Serialize + DeserializeOwned + Validate + Sealed {
    /// This version's action enum.
    type Action: ActionName;
    /// The action this payload belongs to.
    const ACTION: Self::Action;
    /// `CALL` or `SEND`.
    const KIND: MessageKind;
    /// Which peer originates this request.
    const ORIGIN: Origin;
    /// The matching response payload, or [`NoResponse`] for a `SEND`.
    type Response: Response;
}

/// A request that is answered — everything except an OCPP 2.1 `SEND`.
///
/// The bound exists so that awaiting a response the specification forbids the peer from ever
/// sending is a *compile* error rather than a message timeout. It is implemented by the code
/// generator for exactly those actions whose schema directory contains a `…Response.json`.
pub trait Confirmed: Request {}

/// An unconfirmed request — an OCPP 2.1 `SEND` (Part 4 §4.2.4).
///
/// Disjoint from [`Confirmed`]: an action is one or the other, never both.
pub trait Unconfirmed: Request<Response = NoResponse> {}

/// A response payload.
pub trait Response: Serialize + DeserializeOwned + Validate + Sealed {}

/// The response type of a `SEND`, which by definition never has one (Part 4 §4.2.4).
///
/// It is uninhabited, so a `SEND` cannot be awaited by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoResponse {}

impl Sealed for NoResponse {}
impl Response for NoResponse {}

impl Validate for NoResponse {
    fn validate_at(&self, _path: &mut ValidationPath, _out: &mut Violations) {}
}

impl Serialize for NoResponse {
    fn serialize<S: serde::Serializer>(&self, _ser: S) -> Result<S::Ok, S::Error> {
        match *self {}
    }
}

impl<'de> serde::Deserialize<'de> for NoResponse {
    fn deserialize<D: serde::Deserializer<'de>>(_de: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom("a SEND message has no response"))
    }
}

/// An action name this version does not define.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownAction;

impl fmt::Display for UnknownAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown OCPP action for this version")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for UnknownAction {}

/// An enumeration value this version does not define.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownVariant {
    /// The enumeration that rejected the value.
    pub type_name: &'static str,
}

impl fmt::Display for UnknownVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a defined value of {}", self.type_name)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for UnknownVariant {}
