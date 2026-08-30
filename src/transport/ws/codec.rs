//! The RFC 6455 framing codec.
//!
//! One [`Decoder`] / [`Encoder`] pair, driven by `tokio_util::codec::Framed`. It owns every
//! rule the WebSocket protocol places on a frame — masking, fragmentation, control-frame
//! limits, reserved bits, close codes, UTF-8 — and, when the `compression` feature negotiated
//! it, the per-message DEFLATE of RFC 7692.

use std::fmt;
use std::io;

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use super::frame::{self, CloseCode, CloseFrame, Header, HeaderError, OpCode};

/// Which side of the connection this codec is on.
///
/// It decides who masks: RFC 6455 §5.3 says a client masks every frame it sends and a server
/// masks none, and each side must reject the other's mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A Charging Station, or a Local Controller talking upstream.
    Client,
    /// A CSMS, or a Local Controller talking downstream.
    Server,
}

/// One complete WebSocket message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// A UTF-8 text message. OCPP-J uses only these (Part 4 §4.1).
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// A ping. The peer must answer with a matching pong.
    Ping(Vec<u8>),
    /// A pong.
    Pong(Vec<u8>),
    /// The close handshake.
    Close(Option<CloseFrame>),
}

impl Message {
    /// A text message.
    pub fn text(text: impl Into<String>) -> Self {
        Message::Text(text.into())
    }

    /// How many bytes the payload occupies.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Message::Text(text) => text.len(),
            Message::Binary(data) | Message::Ping(data) | Message::Pong(data) => data.len(),
            Message::Close(frame) => frame.as_ref().map_or(0, |frame| frame.reason.len() + 2),
        }
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How the codec is tuned.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The largest message that will be assembled, in bytes, before or after decompression.
    pub max_message_size: usize,
    /// The largest single frame that will be accepted, in bytes.
    pub max_frame_size: usize,
    /// Messages below this size are sent uncompressed, because DEFLATE on a short payload
    /// costs more bytes than it saves.
    pub compression_threshold: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_message_size: 1024 * 1024,
            max_frame_size: 1024 * 1024,
            compression_threshold: 256,
        }
    }
}

/// A rule of RFC 6455 or RFC 7692 that the peer broke.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A reserved bit was set with no extension to give it meaning.
    ReservedBits,
    /// A reserved opcode.
    ReservedOpcode(u8),
    /// A client sent an unmasked frame.
    UnmaskedFrameFromClient,
    /// A server sent a masked frame.
    MaskedFrameFromServer,
    /// A control frame was fragmented.
    FragmentedControlFrame,
    /// A control frame carried more than 125 bytes.
    ControlFrameTooBig,
    /// A continuation frame arrived with no message in progress.
    UnexpectedContinuation,
    /// A new data frame arrived while a message was still in progress.
    ExpectedContinuation,
    /// A frame exceeded [`Config::max_frame_size`].
    FrameTooBig(u64),
    /// A message exceeded [`Config::max_message_size`].
    MessageTooBig,
    /// A length that is not encoded in its minimal form.
    NonMinimalLength,
    /// A close frame whose payload is one byte, or whose code a peer may not send.
    InvalidCloseFrame,
    /// A close reason that is not UTF-8.
    InvalidCloseReason,
    /// A text message whose payload is not UTF-8.
    InvalidUtf8,
    /// `RSV1` was set without `permessage-deflate` having been negotiated.
    CompressionNotNegotiated,
}

impl ProtocolError {
    /// The close code to answer with (RFC 6455 §7.4.1).
    #[must_use]
    pub const fn close_code(&self) -> CloseCode {
        match self {
            ProtocolError::InvalidUtf8 | ProtocolError::InvalidCloseReason => {
                CloseCode::INVALID_PAYLOAD
            }
            ProtocolError::FrameTooBig(_) | ProtocolError::MessageTooBig => CloseCode::TOO_BIG,
            _ => CloseCode::PROTOCOL_ERROR,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::ReservedBits => f.write_str("a reserved bit was set"),
            ProtocolError::ReservedOpcode(bits) => write!(f, "reserved opcode {bits:#x}"),
            ProtocolError::UnmaskedFrameFromClient => {
                f.write_str("a client must mask every frame it sends")
            }
            ProtocolError::MaskedFrameFromServer => {
                f.write_str("a server must not mask the frames it sends")
            }
            ProtocolError::FragmentedControlFrame => {
                f.write_str("a control frame must not be fragmented")
            }
            ProtocolError::ControlFrameTooBig => {
                f.write_str("a control frame must be at most 125 bytes")
            }
            ProtocolError::UnexpectedContinuation => {
                f.write_str("a continuation frame with no message in progress")
            }
            ProtocolError::ExpectedContinuation => {
                f.write_str("a new data frame while a message was still in progress")
            }
            ProtocolError::FrameTooBig(length) => write!(f, "a frame of {length} bytes is too big"),
            ProtocolError::MessageTooBig => f.write_str("the message is too big"),
            ProtocolError::NonMinimalLength => {
                f.write_str("a payload length that is not minimally encoded")
            }
            ProtocolError::InvalidCloseFrame => f.write_str("a malformed close frame"),
            ProtocolError::InvalidCloseReason => f.write_str("a close reason that is not UTF-8"),
            ProtocolError::InvalidUtf8 => f.write_str("a text message that is not UTF-8"),
            ProtocolError::CompressionNotNegotiated => {
                f.write_str("RSV1 was set but permessage-deflate was not negotiated")
            }
        }
    }
}

/// Anything that can go wrong on a WebSocket.
#[derive(Debug)]
#[non_exhaustive]
pub enum WsError {
    /// The socket failed.
    Io(io::Error),
    /// The peer broke a protocol rule.
    Protocol(ProtocolError),
    /// Compression or decompression failed.
    #[cfg(feature = "compression")]
    Deflate(super::deflate::DeflateError),
}

impl fmt::Display for WsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WsError::Io(error) => write!(f, "i/o error: {error}"),
            WsError::Protocol(error) => write!(f, "websocket protocol error: {error}"),
            #[cfg(feature = "compression")]
            WsError::Deflate(error) => write!(f, "permessage-deflate: {error}"),
        }
    }
}

impl std::error::Error for WsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WsError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for WsError {
    fn from(error: io::Error) -> Self {
        WsError::Io(error)
    }
}

impl From<ProtocolError> for WsError {
    fn from(error: ProtocolError) -> Self {
        WsError::Protocol(error)
    }
}

impl WsError {
    /// The close code to answer with, for errors that call for one.
    #[must_use]
    pub fn close_code(&self) -> Option<CloseCode> {
        match self {
            WsError::Protocol(error) => Some(error.close_code()),
            #[cfg(feature = "compression")]
            WsError::Deflate(super::deflate::DeflateError::TooLarge) => Some(CloseCode::TOO_BIG),
            #[cfg(feature = "compression")]
            WsError::Deflate(_) => Some(CloseCode::INVALID_PAYLOAD),
            WsError::Io(_) => None,
        }
    }
}

/// A message being assembled from fragments.
struct Fragment {
    /// The opcode of the first frame, which is what the message actually is.
    opcode: OpCode,
    /// Whether the first frame set `RSV1`.
    compressed: bool,
    payload: Vec<u8>,
}

/// Masking keys, drawn from the operating system in batches.
///
/// RFC 6455 §5.3 requires an unpredictable key per frame — it is what stops a hostile script
/// from steering a proxy into caching an attacker-chosen response. Drawing one syscall's
/// worth at a time keeps that property without a syscall per frame.
struct MaskKeys {
    buffer: [u8; 256],
    used: usize,
}

impl MaskKeys {
    fn new() -> Self {
        Self {
            buffer: [0; 256],
            used: 256,
        }
    }

    fn next(&mut self) -> Result<[u8; 4], WsError> {
        if self.used + 4 > self.buffer.len() {
            // No safe fallback: RFC 6455 §5.3 needs an unpredictable key, and reusing the
            // previous batch would make them cycle. A connection that cannot be masked
            // correctly must not be written to.
            getrandom::fill(&mut self.buffer).map_err(|error| {
                WsError::Io(io::Error::other(format!(
                    "no entropy for a WebSocket masking key: {error}"
                )))
            })?;
            self.used = 0;
        }
        let key = [
            self.buffer[self.used],
            self.buffer[self.used + 1],
            self.buffer[self.used + 2],
            self.buffer[self.used + 3],
        ];
        self.used += 4;
        Ok(key)
    }
}

/// The RFC 6455 framing codec.
pub struct WsCodec {
    role: Role,
    config: Config,
    fragment: Option<Fragment>,
    keys: MaskKeys,
    #[cfg(feature = "compression")]
    deflate: Option<super::deflate::Deflate>,
}

impl fmt::Debug for WsCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsCodec")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl WsCodec {
    /// A codec for one side of a connection, without compression.
    #[must_use]
    pub fn new(role: Role, config: Config) -> Self {
        Self {
            role,
            config,
            fragment: None,
            keys: MaskKeys::new(),
            #[cfg(feature = "compression")]
            deflate: None,
        }
    }

    /// Turns on `permessage-deflate` with the negotiated parameters.
    #[cfg(feature = "compression")]
    #[must_use]
    pub fn with_deflate(mut self, params: super::deflate::DeflateParams) -> Self {
        let role = match self.role {
            Role::Client => super::deflate::Role::Client,
            Role::Server => super::deflate::Role::Server,
        };
        self.deflate = Some(super::deflate::Deflate::new(
            params,
            role,
            self.config.compression_threshold,
        ));
        self
    }

    /// Whether messages on this connection are compressed.
    #[must_use]
    pub fn is_compressed(&self) -> bool {
        #[cfg(feature = "compression")]
        {
            self.deflate.is_some()
        }
        #[cfg(not(feature = "compression"))]
        {
            false
        }
    }

    /// Whether this side masks the frames it sends.
    const fn masks(&self) -> bool {
        matches!(self.role, Role::Client)
    }

    /// Validates a header against the role, the configuration and the message in progress.
    fn check(&self, header: &Header) -> Result<(), ProtocolError> {
        if header.rsv2 || header.rsv3 {
            return Err(ProtocolError::ReservedBits);
        }
        if header.rsv1 {
            // RSV1 means "compressed". RFC 7692 §6 puts it on the *first* frame of a data
            // message and nowhere else: not on a continuation, and never on a control frame.
            if !self.is_compressed() {
                return Err(ProtocolError::CompressionNotNegotiated);
            }
            if header.opcode == OpCode::Continuation || header.opcode.is_control() {
                return Err(ProtocolError::ReservedBits);
            }
        }
        match (self.role, header.mask.is_some()) {
            (Role::Server, false) => return Err(ProtocolError::UnmaskedFrameFromClient),
            (Role::Client, true) => return Err(ProtocolError::MaskedFrameFromServer),
            _ => {}
        }
        if header.opcode.is_control() {
            if !header.fin {
                return Err(ProtocolError::FragmentedControlFrame);
            }
            if header.length > 125 {
                return Err(ProtocolError::ControlFrameTooBig);
            }
        } else {
            if header.length > self.config.max_frame_size as u64 {
                return Err(ProtocolError::FrameTooBig(header.length));
            }
            match (header.opcode, self.fragment.is_some()) {
                (OpCode::Continuation, false) => {
                    return Err(ProtocolError::UnexpectedContinuation);
                }
                (OpCode::Text | OpCode::Binary, true) => {
                    return Err(ProtocolError::ExpectedContinuation);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Turns an assembled payload into a message.
    fn finish(
        &mut self,
        opcode: OpCode,
        compressed: bool,
        payload: Vec<u8>,
    ) -> Result<Message, WsError> {
        let payload = if compressed {
            #[cfg(feature = "compression")]
            {
                let limit = self.config.max_message_size;
                self.deflate
                    .as_mut()
                    .ok_or(ProtocolError::CompressionNotNegotiated)?
                    .decompress(&payload, limit)
                    .map_err(WsError::Deflate)?
            }
            #[cfg(not(feature = "compression"))]
            {
                return Err(ProtocolError::CompressionNotNegotiated.into());
            }
        } else {
            payload
        };
        Ok(match opcode {
            OpCode::Text => {
                Message::Text(String::from_utf8(payload).map_err(|_| ProtocolError::InvalidUtf8)?)
            }
            _ => Message::Binary(payload),
        })
    }

    /// Reads a close frame's body.
    fn close_frame(payload: &[u8]) -> Result<Option<CloseFrame>, ProtocolError> {
        match payload.len() {
            0 => Ok(None),
            // A single byte cannot hold a 16-bit code.
            1 => Err(ProtocolError::InvalidCloseFrame),
            _ => {
                let code = CloseCode(u16::from_be_bytes([payload[0], payload[1]]));
                if !code.is_sendable() {
                    return Err(ProtocolError::InvalidCloseFrame);
                }
                let reason = core::str::from_utf8(&payload[2..])
                    .map_err(|_| ProtocolError::InvalidCloseReason)?;
                Ok(Some(CloseFrame {
                    code,
                    reason: reason.to_owned(),
                }))
            }
        }
    }
}

impl Decoder for WsCodec {
    type Item = Message;
    type Error = WsError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, WsError> {
        loop {
            let header = match frame::read_header(src) {
                Ok(Some(header)) => header,
                Ok(None) => {
                    src.reserve(2);
                    return Ok(None);
                }
                Err(HeaderError::ReservedOpcode(bits)) => {
                    return Err(ProtocolError::ReservedOpcode(bits).into());
                }
                Err(HeaderError::NonMinimalLength) => {
                    return Err(ProtocolError::NonMinimalLength.into());
                }
                Err(HeaderError::LengthTooLarge) => {
                    return Err(ProtocolError::FrameTooBig(u64::MAX).into());
                }
            };
            self.check(&header)?;

            let length = usize::try_from(header.length)
                .map_err(|_| ProtocolError::FrameTooBig(header.length))?;
            let total = header.prefix + length;
            if src.len() < total {
                // Ask for the whole frame in one go rather than growing byte by byte.
                src.reserve(total - src.len());
                return Ok(None);
            }

            src.advance(header.prefix);
            let mut payload = src.split_to(length);
            if let Some(key) = header.mask {
                frame::apply_mask(&mut payload, key);
            }

            if header.opcode.is_control() {
                return Ok(Some(match header.opcode {
                    OpCode::Close => Message::Close(Self::close_frame(&payload)?),
                    OpCode::Ping => Message::Ping(payload.to_vec()),
                    _ => Message::Pong(payload.to_vec()),
                }));
            }

            // A data frame: start or extend the message.
            let fragment = match self.fragment.as_mut() {
                Some(fragment) => fragment,
                None => self.fragment.insert(Fragment {
                    opcode: header.opcode,
                    compressed: header.rsv1,
                    payload: Vec::with_capacity(length),
                }),
            };
            if fragment.payload.len() + length > self.config.max_message_size {
                self.fragment = None;
                return Err(ProtocolError::MessageTooBig.into());
            }
            fragment.payload.extend_from_slice(&payload);

            if header.fin {
                let fragment = self.fragment.take().expect("just inserted");
                return self
                    .finish(fragment.opcode, fragment.compressed, fragment.payload)
                    .map(Some);
            }
            // Not the last frame: keep reading.
        }
    }
}

impl Encoder<Message> for WsCodec {
    type Error = WsError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), WsError> {
        let (opcode, payload) = match item {
            Message::Text(text) => (OpCode::Text, text.into_bytes()),
            Message::Binary(data) => (OpCode::Binary, data),
            Message::Ping(data) => (OpCode::Ping, data),
            Message::Pong(data) => (OpCode::Pong, data),
            Message::Close(frame) => {
                let mut payload = Vec::new();
                if let Some(frame) = frame {
                    payload.extend_from_slice(&frame.code.0.to_be_bytes());
                    // §5.5: a control frame is at most 125 bytes, so a long reason is cut
                    // rather than making the frame unsendable.
                    let room = 123.min(frame.reason.len());
                    let reason = &frame.reason[..floor_char_boundary(&frame.reason, room)];
                    payload.extend_from_slice(reason.as_bytes());
                }
                (OpCode::Close, payload)
            }
        };

        // RFC 7692 §6: only a data message is compressed, and `RSV1` marks its first frame.
        #[cfg(feature = "compression")]
        let (payload, compressed) = match self.deflate.as_mut() {
            Some(deflate)
                if matches!(opcode, OpCode::Text | OpCode::Binary)
                    && deflate.worth_compressing(payload.len()) =>
            {
                (deflate.compress(&payload).map_err(WsError::Deflate)?, true)
            }
            _ => (payload, false),
        };
        #[cfg(not(feature = "compression"))]
        let (payload, compressed) = (payload, false);

        let key = self.masks().then(|| self.keys.next()).transpose()?;
        let mut header = Vec::with_capacity(14);
        frame::write_header(&mut header, true, compressed, opcode, key, payload.len());

        dst.reserve(header.len() + payload.len());
        dst.put_slice(&header);
        let start = dst.len();
        dst.put_slice(&payload);
        if let Some(key) = key {
            frame::apply_mask(&mut dst[start..], key);
        }
        Ok(())
    }
}

/// The largest index at or below `index` that is a character boundary.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client and a server codec, wired to each other's buffers.
    fn pair() -> (WsCodec, WsCodec) {
        (
            WsCodec::new(Role::Client, Config::default()),
            WsCodec::new(Role::Server, Config::default()),
        )
    }

    fn round_trip(from: &mut WsCodec, to: &mut WsCodec, message: Message) -> Message {
        let mut buffer = BytesMut::new();
        from.encode(message, &mut buffer).expect("encodes");
        to.decode(&mut buffer)
            .expect("decodes")
            .expect("a whole message")
    }

    #[test]
    fn messages_round_trip_in_both_directions() {
        let (mut client, mut server) = pair();
        for message in [
            Message::text("[2,\"1\",\"Heartbeat\",{}]"),
            Message::Text(String::new()),
            Message::Binary(vec![0, 1, 2, 250]),
            Message::Ping(b"ping".to_vec()),
            Message::Pong(Vec::new()),
            Message::Close(Some(CloseFrame::new(CloseCode::NORMAL, "bye"))),
            Message::Close(None),
        ] {
            assert_eq!(
                round_trip(&mut client, &mut server, message.clone()),
                message
            );
            assert_eq!(
                round_trip(&mut server, &mut client, message.clone()),
                message
            );
        }
    }

    #[test]
    fn rsv1_is_refused_on_control_frames_and_continuations() {
        // RFC 7692 §6: RSV1 marks the first frame of a compressed *data* message. Anywhere
        // else it is a reserved bit with no meaning, and §5.2 of RFC 6455 says to fail.
        let mut server = WsCodec::new(Role::Server, Config::default());
        // A masked ping with RSV1 set.
        let mut buffer = BytesMut::from(&[0xC9u8, 0x80, 0, 0, 0, 0][..]);
        assert!(
            matches!(
                server.decode(&mut buffer),
                Err(WsError::Protocol(
                    ProtocolError::CompressionNotNegotiated | ProtocolError::ReservedBits
                ))
            ),
            "RSV1 on a ping must not be accepted"
        );
    }

    #[test]
    fn a_client_masks_and_a_server_does_not() {
        let (mut client, mut server) = pair();
        let mut buffer = BytesMut::new();
        client.encode(Message::text("hello"), &mut buffer).unwrap();
        assert_eq!(buffer[1] & 0x80, 0x80, "the mask bit is set");
        // The payload must not appear in the clear.
        assert!(!buffer.windows(5).any(|w| w == b"hello"));

        let mut buffer = BytesMut::new();
        server.encode(Message::text("hello"), &mut buffer).unwrap();
        assert_eq!(buffer[1] & 0x80, 0, "a server never masks");
        assert!(buffer.windows(5).any(|w| w == b"hello"));
    }

    #[test]
    fn a_server_rejects_an_unmasked_frame_and_a_client_a_masked_one() {
        let (mut client, mut server) = pair();
        // A server frame (unmasked) fed to a server decoder.
        let mut buffer = BytesMut::new();
        WsCodec::new(Role::Server, Config::default())
            .encode(Message::text("x"), &mut buffer)
            .unwrap();
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::UnmaskedFrameFromClient))
        ));

        // …and the mirror image.
        let mut buffer = BytesMut::new();
        WsCodec::new(Role::Client, Config::default())
            .encode(Message::text("x"), &mut buffer)
            .unwrap();
        assert!(matches!(
            client.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::MaskedFrameFromServer))
        ));
    }

    #[test]
    fn a_partial_frame_is_buffered_until_it_is_whole() {
        let (mut client, mut server) = pair();
        let mut whole = BytesMut::new();
        client
            .encode(Message::text("a longer payload"), &mut whole)
            .unwrap();

        let mut buffer = BytesMut::new();
        for byte in whole.iter().take(whole.len() - 1) {
            buffer.extend_from_slice(&[*byte]);
            assert_eq!(
                server.decode(&mut buffer).unwrap(),
                None,
                "still incomplete"
            );
        }
        buffer.extend_from_slice(&whole[whole.len() - 1..]);
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::text("a longer payload"))
        );
        assert!(buffer.is_empty());
    }

    /// Builds a raw frame, bypassing the encoder so illegal ones can be constructed.
    fn raw(fin: bool, rsv1: bool, opcode: OpCode, payload: &[u8], mask: bool) -> BytesMut {
        let key = [1u8, 2, 3, 4];
        let mut out = Vec::new();
        frame::write_header(
            &mut out,
            fin,
            rsv1,
            opcode,
            mask.then_some(key),
            payload.len(),
        );
        let start = out.len();
        out.extend_from_slice(payload);
        if mask {
            frame::apply_mask(&mut out[start..], key);
        }
        BytesMut::from(&out[..])
    }

    #[test]
    fn a_fragmented_message_is_reassembled() {
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(false, false, OpCode::Text, b"one ", true);
        assert_eq!(server.decode(&mut buffer).unwrap(), None);

        buffer.extend_from_slice(&raw(false, false, OpCode::Continuation, b"two ", true));
        assert_eq!(server.decode(&mut buffer).unwrap(), None);

        buffer.extend_from_slice(&raw(true, false, OpCode::Continuation, b"three", true));
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::text("one two three"))
        );
    }

    #[test]
    fn a_control_frame_may_interleave_with_a_fragmented_message() {
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(false, false, OpCode::Text, b"half", true);
        assert_eq!(server.decode(&mut buffer).unwrap(), None);

        // RFC 6455 §5.4: a control frame may be injected between fragments.
        buffer.extend_from_slice(&raw(true, false, OpCode::Ping, b"hi", true));
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::Ping(b"hi".to_vec()))
        );

        buffer.extend_from_slice(&raw(true, false, OpCode::Continuation, b"-done", true));
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::text("half-done"))
        );
    }

    #[test]
    fn fragmentation_rules_are_enforced() {
        let mut server = WsCodec::new(Role::Server, Config::default());
        // A continuation with nothing in progress.
        let mut buffer = raw(true, false, OpCode::Continuation, b"x", true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::UnexpectedContinuation))
        ));

        // A new data frame while one is in progress.
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(false, false, OpCode::Text, b"x", true);
        assert_eq!(server.decode(&mut buffer).unwrap(), None);
        buffer.extend_from_slice(&raw(true, false, OpCode::Text, b"y", true));
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::ExpectedContinuation))
        ));
    }

    #[test]
    fn control_frames_must_be_short_and_unfragmented() {
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(false, false, OpCode::Ping, b"x", true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::FragmentedControlFrame))
        ));

        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(true, false, OpCode::Ping, &[0u8; 126], true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::ControlFrameTooBig))
        ));
    }

    #[test]
    fn reserved_bits_are_refused_when_no_extension_defines_them() {
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(true, true, OpCode::Text, b"x", true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::CompressionNotNegotiated))
        ));
    }

    #[test]
    fn a_text_message_that_is_not_utf8_is_refused() {
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(true, false, OpCode::Text, &[0xF0, 0x28, 0x8C, 0x28], true);
        let error = server.decode(&mut buffer).unwrap_err();
        assert!(matches!(
            error,
            WsError::Protocol(ProtocolError::InvalidUtf8)
        ));
        // §7.4.1: 1007 is the code for a payload inconsistent with its type.
        assert_eq!(error.close_code(), Some(CloseCode::INVALID_PAYLOAD));

        // The same bytes as a *binary* message are perfectly fine.
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(true, false, OpCode::Binary, &[0xF0, 0x28, 0x8C, 0x28], true);
        assert!(server.decode(&mut buffer).unwrap().is_some());
    }

    #[test]
    fn malformed_close_frames_are_refused() {
        // One byte cannot hold a code.
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(true, false, OpCode::Close, &[0x03], true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::InvalidCloseFrame))
        ));

        // 1005 is reserved for local use and must never appear on the wire.
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut buffer = raw(true, false, OpCode::Close, &1005u16.to_be_bytes(), true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::InvalidCloseFrame))
        ));

        // A reason that is not UTF-8.
        let mut server = WsCodec::new(Role::Server, Config::default());
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.push(0xFF);
        let mut buffer = raw(true, false, OpCode::Close, &payload, true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::InvalidCloseReason))
        ));
    }

    #[test]
    fn size_limits_are_enforced_on_frames_and_messages() {
        let config = Config {
            max_frame_size: 64,
            max_message_size: 128,
            ..Config::default()
        };
        let mut server = WsCodec::new(Role::Server, config);
        let mut buffer = raw(true, false, OpCode::Text, &[b'x'; 100], true);
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::FrameTooBig(100)))
        ));

        // Frames that individually fit but together do not.
        let mut server = WsCodec::new(Role::Server, config);
        let mut buffer = raw(false, false, OpCode::Text, &[b'x'; 64], true);
        assert_eq!(server.decode(&mut buffer).unwrap(), None);
        buffer.extend_from_slice(&raw(false, false, OpCode::Continuation, &[b'x'; 64], true));
        assert_eq!(server.decode(&mut buffer).unwrap(), None);
        buffer.extend_from_slice(&raw(true, false, OpCode::Continuation, &[b'x'; 64], true));
        let error = server.decode(&mut buffer).unwrap_err();
        assert!(matches!(
            error,
            WsError::Protocol(ProtocolError::MessageTooBig)
        ));
        assert_eq!(error.close_code(), Some(CloseCode::TOO_BIG));
    }

    #[test]
    fn an_over_long_close_reason_is_trimmed_to_fit_a_control_frame() {
        let (mut client, mut server) = pair();
        let reason = "é".repeat(200);
        let message = round_trip(
            &mut client,
            &mut server,
            Message::Close(Some(CloseFrame::new(CloseCode::NORMAL, reason))),
        );
        let Message::Close(Some(frame)) = message else {
            panic!("expected a close frame")
        };
        assert!(frame.reason.len() <= 123);
        // Trimmed on a character boundary, so the reason is still valid UTF-8.
        assert!(frame.reason.chars().all(|c| c == 'é'));
    }

    #[test]
    fn several_messages_arrive_from_one_read() {
        let (mut client, mut server) = pair();
        let mut buffer = BytesMut::new();
        client.encode(Message::text("one"), &mut buffer).unwrap();
        client.encode(Message::text("two"), &mut buffer).unwrap();
        client
            .encode(Message::Ping(Vec::new()), &mut buffer)
            .unwrap();

        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::text("one"))
        );
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::text("two"))
        );
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::Ping(Vec::new()))
        );
        assert_eq!(server.decode(&mut buffer).unwrap(), None);
    }

    #[test]
    fn masking_keys_differ_between_frames() {
        let mut client = WsCodec::new(Role::Client, Config::default());
        let mut keys = std::collections::HashSet::new();
        for _ in 0..64 {
            let mut buffer = BytesMut::new();
            client.encode(Message::text(""), &mut buffer).unwrap();
            keys.insert(buffer[2..6].to_vec());
        }
        assert!(
            keys.len() > 60,
            "masking keys must not repeat: {} distinct",
            keys.len()
        );
    }

    #[cfg(feature = "compression")]
    #[test]
    fn compressed_messages_round_trip_and_set_rsv1() {
        use super::super::deflate::DeflateParams;
        let params = DeflateParams::default();
        let mut client = WsCodec::new(Role::Client, Config::default()).with_deflate(params);
        let mut server = WsCodec::new(Role::Server, Config::default()).with_deflate(params);

        let payload = "[2,\"1\",\"TransactionEvent\",{\"eventType\":\"Updated\"}]".repeat(20);
        let mut buffer = BytesMut::new();
        client
            .encode(Message::text(payload.clone()), &mut buffer)
            .unwrap();
        assert_eq!(buffer[0] & 0x40, 0x40, "RSV1 marks a compressed message");
        assert!(
            buffer.len() < payload.len() / 2,
            "it should actually be smaller"
        );
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::Text(payload))
        );
    }

    #[cfg(feature = "compression")]
    #[test]
    fn short_messages_are_sent_uncompressed() {
        use super::super::deflate::DeflateParams;
        let params = DeflateParams::default();
        let mut client = WsCodec::new(Role::Client, Config::default()).with_deflate(params);
        let mut server = WsCodec::new(Role::Server, Config::default()).with_deflate(params);

        let mut buffer = BytesMut::new();
        client
            .encode(Message::text("[3,\"1\",{}]"), &mut buffer)
            .unwrap();
        assert_eq!(buffer[0] & 0x40, 0, "below the threshold, RSV1 stays clear");
        // A negotiated connection must still accept uncompressed messages.
        assert_eq!(
            server.decode(&mut buffer).unwrap(),
            Some(Message::text("[3,\"1\",{}]"))
        );
    }

    #[cfg(feature = "compression")]
    #[test]
    fn a_continuation_frame_may_not_claim_to_be_compressed() {
        use super::super::deflate::DeflateParams;
        let mut server =
            WsCodec::new(Role::Server, Config::default()).with_deflate(DeflateParams::default());
        let mut buffer = raw(false, true, OpCode::Text, b"x", true);
        assert_eq!(server.decode(&mut buffer).unwrap(), None);
        // RSV1 belongs on the first frame only (RFC 7692 §6.1).
        buffer.extend_from_slice(&raw(true, true, OpCode::Continuation, b"y", true));
        assert!(matches!(
            server.decode(&mut buffer),
            Err(WsError::Protocol(ProtocolError::ReservedBits))
        ));
    }
}
