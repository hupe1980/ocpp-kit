//! A WebSocket implementation for OCPP-J.
//!
//! OCPP 2.1 makes RFC 7692 `permessage-deflate` **required** for a CSMS and a Local Controller
//! (Part 4 §3.4 Table 2), and no general-purpose Rust WebSocket crate implements it — the
//! frame layer has to surface `RSV1`, and the ones that do not simply reject it. So the frame
//! layer lives here.
//!
//! It is deliberately narrow: OCPP-J uses text messages, ping/pong and close, over a
//! handshake this crate already performs itself in order to answer 404 and 401 correctly. What
//! is *not* narrow is the validation — every rule RFC 6455 places on a frame is enforced, and
//! each has a test named after it.

pub(crate) mod codec;
pub(crate) mod frame;
pub(crate) mod handshake;

#[cfg(feature = "compression")]
pub mod deflate;

pub use codec::{Config, Message, ProtocolError, Role, WsCodec, WsError};
pub use frame::{CloseCode, CloseFrame};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, FramedParts};

/// A WebSocket connection: a `Stream` of [`Message`]s and a `Sink` for them.
pub type WebSocket<S> = Framed<S, WsCodec>;

/// Wraps a socket whose handshake is already done.
///
/// `leftover` is whatever arrived in the same read as the end of the HTTP head — on a fast
/// link the first OCPP message routinely does, and dropping it would lose a `BootNotification`.
pub(crate) fn attach<S>(io: S, codec: WsCodec, leftover: &[u8]) -> WebSocket<S>
where
    S: AsyncRead + AsyncWrite,
{
    let mut parts = FramedParts::new(io, codec);
    parts.read_buf = bytes::BytesMut::from(leftover);
    Framed::from_parts(parts)
}
