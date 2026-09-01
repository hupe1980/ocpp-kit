//! Layer 3 — Tokio + WebSocket transport (feature `tokio`).
//!
//! Three roles, one engine:
//!
//! * [`Station`] — a Charging Station client, with subprotocol negotiation, security
//!   profiles 1–3, WebSocket ping, and reconnect back-off exactly as Part 4 §5.4 defines it.
//! * [`Csms`] — a CSMS server, with a per-identity session router, single-active-connection
//!   policy, constant-time Basic authentication and the 404 / 401 / "no subprotocol" rules
//!   of Part 4 §3.1.
//! * [`LocalController`] — the Part 4 chapter 6 proxy: one upstream connection per attached
//!   station under the *same* identity, close propagation in both directions, and
//!   frame-level relaying so signed messages survive untouched.
//!
//! All three drive the same [`Engine`](crate::engine::Engine); the transport only moves
//! bytes and time.

mod connection;
mod csms;
mod local_controller;
mod network_profile;
mod security;
mod station;
mod stream;
#[cfg(feature = "rustls")]
mod tls;
mod ws;

pub use connection::{
    BoxFuture, Ctx, Event, Handle, Handler, Keepalive, NotImplemented, SessionContext, SessionState,
};
pub use csms::{
    AcceptEveryStation, Auth, AuthOutcome, Authenticator, Csms, CsmsBuilder, CsmsHandle,
    SessionEvent,
};
pub use local_controller::{
    Direction, LocalController, LocalControllerBuilder, PassThrough, RefuseActions, Relay,
    RelayDecision, UpstreamCredentials,
};
pub use network_profile::{NetworkProfile, NetworkProfiles};
pub use security::{
    BASIC_AUTH_INTEROPERABLE_MAX_LEN, BASIC_AUTH_MAX_LEN, BASIC_AUTH_MIN_LEN, BasicAuthPassword,
    CredentialError, Credentials, SecurityProfile, basic_auth_header, constant_time_eq,
};
pub use station::{Station, StationBuilder};
pub use stream::MaybeTls;
#[cfg(feature = "compression")]
pub use ws::deflate::{DeflateError, DeflateParams, NegotiationError as DeflateNegotiationError};
pub use ws::{
    CloseCode, CloseFrame, Config as WsConfig, Message as WsMessage, ProtocolError, Role as WsRole,
    WsError,
};

/// Internals the crate's own WebSocket tests reach into.
///
/// Not part of the supported API: it exists so the interop suite can drive a raw socket the
/// way a peer would, and it may change without notice.
#[doc(hidden)]
pub mod ws_test_support {
    /// A client-side codec with `permessage-deflate` already negotiated.
    #[cfg(feature = "compression")]
    #[must_use]
    pub fn client_codec_with_deflate() -> super::ws::WsCodec {
        super::ws::WsCodec::new(super::ws::Role::Client, super::ws::Config::default())
            .with_deflate(super::ws::deflate::DeflateParams::default())
    }
}

/// The WebSocket codec, exposed for fuzzing.
///
/// Not part of the supported API. The frame layer is the crate's most security-sensitive
/// code — masking, fragmentation, length parsing — so `cargo fuzz run websocket` drives it
/// directly rather than through a socket.
#[doc(hidden)]
pub mod ws_fuzz {
    pub use super::ws::{Config, Message, Role, WsCodec, WsError};
    use bytes::BytesMut;
    use tokio_util::codec::{Decoder as _, Encoder as _};

    /// Decodes one message, if the buffer holds a whole one.
    pub fn decode(codec: &mut WsCodec, buffer: &mut BytesMut) -> Result<Option<Message>, WsError> {
        codec.decode(buffer)
    }

    /// Encodes one message.
    pub fn encode(
        codec: &mut WsCodec,
        message: Message,
        buffer: &mut BytesMut,
    ) -> Result<(), WsError> {
        codec.encode(message, buffer)
    }
}
#[cfg(feature = "rustls")]
pub use tls::{ClientTls, ClientTlsBuilder, ServerTls, TlsError};

use core::fmt;

/// Anything that can go wrong while moving OCPP over a socket.
#[derive(Debug)]
#[non_exhaustive]
pub enum TransportError {
    /// The TCP or TLS connection failed.
    Io(std::io::Error),
    /// The WebSocket layer failed.
    WebSocket(ws::WsError),
    /// Subprotocol negotiation failed (Part 4 §3.1.2).
    Negotiation(crate::version::NegotiationError),
    /// The URL could not be used.
    Url(String),
    /// The CSMS rejected the handshake.
    Rejected {
        /// The HTTP status it answered with — 401 for bad credentials, 404 for an unknown
        /// Charging Station identity (Part 4 §3.1.1).
        status: u16,
    },
    /// A credential does not satisfy the rules of the negotiated version.
    Credential(CredentialError),
    /// The configuration is incomplete or contradictory.
    Configuration(String),
    /// The durable message store failed.
    Store(crate::engine::StoreError),
    /// The session ended.
    Closed,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(error) => write!(f, "i/o error: {error}"),
            TransportError::WebSocket(error) => write!(f, "websocket error: {error}"),
            TransportError::Negotiation(error) => write!(f, "{error}"),
            TransportError::Url(url) => write!(f, "unusable URL: {url}"),
            TransportError::Rejected { status } => {
                write!(f, "the CSMS rejected the handshake with HTTP {status}")
            }
            TransportError::Credential(error) => write!(f, "{error}"),
            TransportError::Configuration(what) => write!(f, "configuration error: {what}"),
            TransportError::Store(error) => write!(f, "message store error: {error}"),
            TransportError::Closed => f.write_str("the session is closed"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Io(error) => Some(error),
            TransportError::WebSocket(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(error: std::io::Error) -> Self {
        TransportError::Io(error)
    }
}

impl From<ws::WsError> for TransportError {
    fn from(error: ws::WsError) -> Self {
        TransportError::WebSocket(error)
    }
}

impl From<crate::version::NegotiationError> for TransportError {
    fn from(error: crate::version::NegotiationError) -> Self {
        TransportError::Negotiation(error)
    }
}

impl From<CredentialError> for TransportError {
    fn from(error: CredentialError) -> Self {
        TransportError::Credential(error)
    }
}

impl From<crate::engine::StoreError> for TransportError {
    fn from(error: crate::engine::StoreError) -> Self {
        TransportError::Store(error)
    }
}
