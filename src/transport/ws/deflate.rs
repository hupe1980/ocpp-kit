//! RFC 7692 `permessage-deflate` (feature `compression`).
//!
//! OCPP 2.1 Part 4 §3.4 Table 2 makes this **required** for a CSMS and a Local Controller and
//! optional (but recommended) for a Charging Station: it is the cheap way to cut the mobile
//! data bill of a fleet that sends `TransactionEvent`s all day.
//!
//! The mechanism is small. A compressed message sets `RSV1` on its first frame; its payload is
//! a raw DEFLATE stream with the four trailing bytes `00 00 FF FF` removed. Unless
//! `no_context_takeover` was negotiated, the compressor's window carries over between
//! messages, which is where most of the ratio on repetitive JSON comes from.

use core::fmt;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

/// The four bytes RFC 7692 §7.2.1 removes from the end of every compressed message.
const TAIL: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

/// The widest LZ77 window, which is the only one this implementation compresses with.
///
/// The pure-Rust DEFLATE backend does not expose the window size, so a *narrower* window
/// cannot be produced. That is not a conformance problem: RFC 7692 §7.1.2.2 says a server
/// declines an offer whose `server_max_window_bits` it cannot honour, which is exactly what
/// [`accept_offer`] does — and a 15-bit *decompressor* reads a stream produced with any
/// smaller window, so the receive path is unconstrained either way.
const MAX_WINDOW_BITS: u8 = 15;

/// The narrowest window RFC 7692 can express in a raw DEFLATE stream. The RFC allows 8, but
/// DEFLATE does not, and every implementation widens it to 9.
const MIN_WINDOW_BITS: u8 = 9;

/// The negotiated `permessage-deflate` parameters.
///
/// Only the two context-takeover parameters are negotiable here. The window size is not:
/// the pure-Rust DEFLATE backend does not expose it, so an offer that would narrow this
/// implementation's window is declined rather than accepted and violated — which is what
/// RFC 7692 §7.1.2.2 prescribes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeflateParams {
    /// Whether the *server* resets its compressor after every message.
    pub server_no_context_takeover: bool,
    /// Whether the *client* resets its compressor after every message.
    pub client_no_context_takeover: bool,
}

impl DeflateParams {
    /// Renders the parameters as a `Sec-WebSocket-Extensions` value.
    #[must_use]
    pub fn to_header(self) -> String {
        let mut out = String::from("permessage-deflate");
        if self.server_no_context_takeover {
            out.push_str("; server_no_context_takeover");
        }
        if self.client_no_context_takeover {
            out.push_str("; client_no_context_takeover");
        }
        out
    }

    /// The offer a Charging Station makes.
    ///
    /// Deliberately without `client_max_window_bits`: announcing it would entitle the server
    /// to answer with a value, and RFC 7692 then requires the client to honour it or fail the
    /// connection. Not announcing it means the server *must not* send one (§7.1.2.1), so the
    /// negotiation cannot land somewhere this implementation has to refuse.
    #[must_use]
    pub fn client_offer(self) -> String {
        self.to_header()
    }
}

/// One `Sec-WebSocket-Extensions` offer.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Offer {
    server_no_context_takeover: bool,
    client_no_context_takeover: bool,
    server_max_window_bits: Option<u8>,
    /// How the client announced `client_max_window_bits`, which RFC 7692 gives three
    /// distinct meanings.
    client_max_window_bits: ClientWindow,
}

/// How a `client_max_window_bits` parameter appeared in an offer or a response.
///
/// The three cases mean different things in RFC 7692: absent forbids the peer from sending a
/// value at all, present-without-a-value announces support, and present-with-a-value binds
/// the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientWindow {
    /// The parameter did not appear.
    Absent,
    /// It appeared with no value.
    Announced,
    /// It appeared with a value.
    Bound(u8),
}

/// Why an offer could not be accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NegotiationError {
    /// A parameter appeared twice, which RFC 7692 §7.1 makes a failure.
    Duplicate(String),
    /// A parameter value is not a window size between 8 and 15.
    BadWindowBits(String),
    /// A parameter this extension does not define.
    Unknown(String),
    /// The server answered with an extension that was not offered.
    NotOffered,
}

impl fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NegotiationError::Duplicate(name) => {
                write!(f, "permessage-deflate parameter {name:?} appears twice")
            }
            NegotiationError::BadWindowBits(value) => {
                write!(f, "{value:?} is not a window size between 8 and 15")
            }
            NegotiationError::Unknown(name) => {
                write!(f, "unknown permessage-deflate parameter {name:?}")
            }
            NegotiationError::NotOffered => {
                f.write_str("the server accepted an extension that was not offered")
            }
        }
    }
}

impl std::error::Error for NegotiationError {}

/// Parses one `permessage-deflate` offer from a `Sec-WebSocket-Extensions` element.
fn parse_offer(element: &str) -> Result<Option<Offer>, NegotiationError> {
    let mut parts = element.split(';').map(str::trim);
    if parts.next() != Some("permessage-deflate") {
        return Ok(None);
    }
    let mut offer = Offer {
        server_no_context_takeover: false,
        client_no_context_takeover: false,
        server_max_window_bits: None,
        client_max_window_bits: ClientWindow::Absent,
    };
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let (name, value) = match part.split_once('=') {
            Some((name, value)) => (name.trim(), Some(value.trim().trim_matches('"'))),
            None => (part, None),
        };
        match name {
            "server_no_context_takeover" => {
                if offer.server_no_context_takeover {
                    return Err(NegotiationError::Duplicate(name.to_owned()));
                }
                offer.server_no_context_takeover = true;
            }
            "client_no_context_takeover" => {
                if offer.client_no_context_takeover {
                    return Err(NegotiationError::Duplicate(name.to_owned()));
                }
                offer.client_no_context_takeover = true;
            }
            "server_max_window_bits" => {
                if offer.server_max_window_bits.is_some() {
                    return Err(NegotiationError::Duplicate(name.to_owned()));
                }
                let value = value.ok_or_else(|| NegotiationError::BadWindowBits(String::new()))?;
                offer.server_max_window_bits = Some(window_bits(value)?);
            }
            "client_max_window_bits" => {
                if offer.client_max_window_bits != ClientWindow::Absent {
                    return Err(NegotiationError::Duplicate(name.to_owned()));
                }
                offer.client_max_window_bits = match value {
                    Some(value) => ClientWindow::Bound(window_bits(value)?),
                    None => ClientWindow::Announced,
                };
            }
            other => return Err(NegotiationError::Unknown(other.to_owned())),
        }
    }
    Ok(Some(offer))
}

fn window_bits(value: &str) -> Result<u8, NegotiationError> {
    let bits: u8 = value
        .parse()
        .map_err(|_| NegotiationError::BadWindowBits(value.to_owned()))?;
    if !(8..=MAX_WINDOW_BITS).contains(&bits) {
        return Err(NegotiationError::BadWindowBits(value.to_owned()));
    }
    // 8 is legal in the header but not in a raw DEFLATE stream.
    Ok(bits.max(MIN_WINDOW_BITS))
}

/// The server side of negotiation: picks the first offer it can accept.
///
/// Returns the parameters and the `Sec-WebSocket-Extensions` value to answer with. An offer
/// this server cannot honour is *skipped* rather than fatal — RFC 7692 §5.1 lets a client
/// list several, and the next may work — which is also how an offer constraining
/// `server_max_window_bits` is handled.
#[must_use]
pub fn accept_offer(header: Option<&str>) -> Option<(DeflateParams, String)> {
    for element in header?.split(',') {
        let Ok(Some(offer)) = parse_offer(element) else {
            continue;
        };
        // §7.1.2.2: a server that cannot use a narrower window declines the offer.
        if offer
            .server_max_window_bits
            .is_some_and(|bits| bits < MAX_WINDOW_BITS)
        {
            continue;
        }
        let params = DeflateParams {
            server_no_context_takeover: offer.server_no_context_takeover,
            client_no_context_takeover: offer.client_no_context_takeover,
        };
        // §7.1.2.1: `client_max_window_bits` must not appear in the response unless the
        // client offered it — and there is nothing to gain by sending it when it did, since
        // a 15-bit decompressor reads any narrower stream.
        return Some((params, params.to_header()));
    }
    None
}

/// The client side of negotiation: reads the server's answer.
///
/// A response that constrains the *client's* window is refused: RFC 7692 requires the client
/// to honour it, and this implementation cannot. The offer never announces
/// `client_max_window_bits`, so a conforming server will not send one.
pub fn accept_response(header: Option<&str>) -> Result<Option<DeflateParams>, NegotiationError> {
    let Some(header) = header else {
        return Ok(None);
    };
    let element = header.split(',').next().unwrap_or_default();
    let Some(offer) = parse_offer(element)? else {
        return Err(NegotiationError::NotOffered);
    };
    if let ClientWindow::Bound(bits) = offer.client_max_window_bits {
        if bits < MAX_WINDOW_BITS {
            return Err(NegotiationError::BadWindowBits(bits.to_string()));
        }
    }
    Ok(Some(DeflateParams {
        server_no_context_takeover: offer.server_no_context_takeover,
        client_no_context_takeover: offer.client_no_context_takeover,
    }))
}

/// Compresses outgoing messages and decompresses incoming ones.
pub(crate) struct Deflate {
    compress: Compress,
    decompress: Decompress,
    reset_compressor: bool,
    reset_decompressor: bool,
    /// Messages below this many bytes are sent uncompressed: DEFLATE on a short payload costs
    /// more bytes than it saves.
    threshold: usize,
}

impl fmt::Debug for Deflate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deflate")
            .field("reset_compressor", &self.reset_compressor)
            .field("reset_decompressor", &self.reset_decompressor)
            .finish_non_exhaustive()
    }
}

/// Which side of the connection a codec is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The Charging Station, or a Local Controller talking upstream.
    Client,
    /// The CSMS, or a Local Controller talking downstream.
    Server,
}

impl Deflate {
    /// Builds the codec for one side of a negotiated connection.
    pub(crate) fn new(params: DeflateParams, role: Role, threshold: usize) -> Self {
        // "server_no_context_takeover" constrains the server's *compressor*, which is the
        // client's *decompressor* — getting this the wrong way round produces a stream the
        // peer cannot read, so it is spelled out rather than inferred.
        let (out_reset, in_reset) = match role {
            Role::Server => (
                params.server_no_context_takeover,
                params.client_no_context_takeover,
            ),
            Role::Client => (
                params.client_no_context_takeover,
                params.server_no_context_takeover,
            ),
        };
        Self {
            // Raw DEFLATE, no zlib header — RFC 7692 §7.2.1.
            compress: Compress::new(Compression::default(), false),
            decompress: Decompress::new(false),
            reset_compressor: out_reset,
            reset_decompressor: in_reset,
            threshold,
        }
    }

    /// Whether a payload of this size is worth compressing.
    pub(crate) fn worth_compressing(&self, len: usize) -> bool {
        len >= self.threshold
    }

    /// Compresses one message, returning the payload for a frame with `RSV1` set.
    pub(crate) fn compress(&mut self, payload: &[u8]) -> Result<Vec<u8>, DeflateError> {
        if self.reset_compressor {
            self.compress.reset();
        }
        let mut out = Vec::with_capacity(payload.len() / 2 + 16);
        let mut input = payload;
        loop {
            let before_in = self.compress.total_in();
            let before_out = self.compress.total_out();
            out.reserve(256);
            let status = self
                .compress
                .compress_vec(input, &mut out, FlushCompress::Sync)
                .map_err(|_| DeflateError::Compress)?;
            let consumed = usize::try_from(self.compress.total_in() - before_in).unwrap_or(0);
            input = &input[consumed..];
            let produced = self.compress.total_out() - before_out;
            if input.is_empty() && (produced == 0 || status == Status::StreamEnd) {
                break;
            }
            if input.is_empty() && out.len() >= 4 && out.ends_with(&TAIL) {
                break;
            }
            if consumed == 0 && produced == 0 {
                return Err(DeflateError::Compress);
            }
        }
        // §7.2.1: an empty DEFLATE block is written as 0x00 when the tail is absent.
        if out.ends_with(&TAIL) {
            out.truncate(out.len() - TAIL.len());
        } else if out.is_empty() {
            out.push(0x00);
        }
        Ok(out)
    }

    /// Decompresses one message that arrived with `RSV1` set.
    pub(crate) fn decompress(
        &mut self,
        payload: &[u8],
        limit: usize,
    ) -> Result<Vec<u8>, DeflateError> {
        if self.reset_decompressor {
            self.decompress.reset(false);
        }
        // §7.2.2: the four bytes the sender removed are put back before inflating.
        let mut input = Vec::with_capacity(payload.len() + TAIL.len());
        input.extend_from_slice(payload);
        input.extend_from_slice(&TAIL);

        let mut out: Vec<u8> = Vec::with_capacity(payload.len() * 4);
        let mut cursor = &input[..];
        loop {
            let before_in = self.decompress.total_in();
            let before_out = self.decompress.total_out();
            // A compressed payload can expand enormously, so the limit is enforced as it
            // inflates rather than after: a zip bomb cannot allocate its way through, and
            // overshoot is bounded by one reservation. `>` rather than `>=`, to match what
            // the uncompressed path accepts.
            if out.len() > limit {
                return Err(DeflateError::TooLarge);
            }
            out.reserve(
                (limit.saturating_sub(out.len()))
                    .clamp(1, 64 * 1024)
                    .max(256),
            );
            let status = self
                .decompress
                .decompress_vec(cursor, &mut out, FlushDecompress::Sync)
                .map_err(|_| DeflateError::Decompress)?;
            let consumed = usize::try_from(self.decompress.total_in() - before_in).unwrap_or(0);
            cursor = &cursor[consumed..];
            let produced = self.decompress.total_out() - before_out;
            if status == Status::StreamEnd {
                break;
            }
            if cursor.is_empty() && produced == 0 {
                break;
            }
            if consumed == 0 && produced == 0 {
                return Err(DeflateError::Decompress);
            }
        }
        if out.len() > limit {
            return Err(DeflateError::TooLarge);
        }
        Ok(out)
    }
}

/// Why compression or decompression failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeflateError {
    /// The DEFLATE stream could not be produced.
    Compress,
    /// The DEFLATE stream is corrupt.
    Decompress,
    /// The message inflated past the configured limit.
    TooLarge,
}

impl fmt::Display for DeflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DeflateError::Compress => "could not compress the message",
            DeflateError::Decompress => "the compressed message is corrupt",
            DeflateError::TooLarge => "the compressed message inflates past the size limit",
        })
    }
}

impl std::error::Error for DeflateError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(params: DeflateParams) -> (Deflate, Deflate) {
        (
            Deflate::new(params, Role::Client, 0),
            Deflate::new(params, Role::Server, 0),
        )
    }

    /// A message of exactly the limit is within it. The uncompressed path accepts one
    /// (`len > max` is what it refuses), so this one has to as well — otherwise whether a
    /// peer's message is refused depends on whether it happened to compress, which is not a
    /// property anyone can reason about.
    #[test]
    fn the_size_limit_is_inclusive_and_matches_the_uncompressed_path() {
        let (mut client, mut server) = pair(DeflateParams::default());
        // Highly compressible, so the guard is reached with room to spare.
        let payload = vec![b'x'; 100_000];
        let compressed = client.compress(&payload).unwrap();

        assert_eq!(
            server.decompress(&compressed, 100_000).unwrap().len(),
            100_000,
            "exactly the limit is within it"
        );

        let (mut client, mut server) = pair(DeflateParams::default());
        let compressed = client.compress(&payload).unwrap();
        assert_eq!(
            server.decompress(&compressed, 99_999),
            Err(DeflateError::TooLarge),
            "one byte over is not"
        );
    }

    #[test]
    fn a_message_round_trips_between_the_two_sides() {
        let (mut client, mut server) = pair(DeflateParams::default());
        let payload = br#"[2,"1","BootNotification",{"reason":"PowerUp"}]"#;

        let compressed = client.compress(payload).unwrap();
        assert!(
            !compressed.ends_with(&TAIL),
            "the tail is removed on the wire"
        );
        assert_eq!(server.decompress(&compressed, 1 << 20).unwrap(), payload);

        // …and in the other direction.
        let compressed = server.compress(payload).unwrap();
        assert_eq!(client.decompress(&compressed, 1 << 20).unwrap(), payload);
    }

    #[test]
    fn context_takeover_makes_repeated_messages_much_smaller() {
        let (mut client, mut server) = pair(DeflateParams::default());
        let payload = br#"[2,"19223201","TransactionEvent",{"eventType":"Updated","seqNo":1}]"#;

        let first = client.compress(payload).unwrap().len();
        let second = client.compress(payload).unwrap().len();
        assert!(second < first / 2, "{second} should be far below {first}");

        // The decompressor must carry its context over too, or the second message is garbage.
        let mut round = Deflate::new(DeflateParams::default(), Role::Client, 0);
        let a = round.compress(payload).unwrap();
        let b = round.compress(payload).unwrap();
        assert_eq!(server.decompress(&a, 1 << 20).unwrap(), payload);
        assert_eq!(server.decompress(&b, 1 << 20).unwrap(), payload);
    }

    #[test]
    fn no_context_takeover_resets_between_messages() {
        let params = DeflateParams {
            client_no_context_takeover: true,
            server_no_context_takeover: true,
        };
        let (mut client, mut server) = pair(params);
        let payload = br#"[2,"19223201","TransactionEvent",{"eventType":"Updated","seqNo":1}]"#;

        let first = client.compress(payload).unwrap();
        let second = client.compress(payload).unwrap();
        assert_eq!(
            first.len(),
            second.len(),
            "each message starts from a clean window"
        );
        assert_eq!(server.decompress(&first, 1 << 20).unwrap(), payload);
        assert_eq!(server.decompress(&second, 1 << 20).unwrap(), payload);
    }

    #[test]
    fn an_empty_message_round_trips() {
        let (mut client, mut server) = pair(DeflateParams::default());
        let compressed = client.compress(b"").unwrap();
        assert!(
            !compressed.is_empty(),
            "an empty DEFLATE block is still a byte"
        );
        assert_eq!(server.decompress(&compressed, 1 << 20).unwrap(), b"");
    }

    #[test]
    fn a_message_that_inflates_past_the_limit_is_refused() {
        let (mut client, mut server) = pair(DeflateParams::default());
        // A megabyte of zeroes compresses to a few hundred bytes.
        let bomb = vec![0u8; 1024 * 1024];
        let compressed = client.compress(&bomb).unwrap();
        assert!(compressed.len() < 4096, "the bomb is small on the wire");
        assert_eq!(
            server.decompress(&compressed, 64 * 1024),
            Err(DeflateError::TooLarge)
        );
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic() {
        let (_, mut server) = pair(DeflateParams::default());
        assert_eq!(
            server.decompress(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 1 << 20),
            Err(DeflateError::Decompress)
        );
    }

    #[test]
    fn the_server_answers_an_offer_it_can_accept() {
        let (params, response) =
            accept_offer(Some("permessage-deflate; client_max_window_bits")).unwrap();
        assert_eq!(response, "permessage-deflate");
        assert_eq!(params, DeflateParams::default());

        let (params, response) =
            accept_offer(Some("permessage-deflate; server_no_context_takeover")).unwrap();
        assert!(params.server_no_context_takeover);
        assert_eq!(response, "permessage-deflate; server_no_context_takeover");

        // §7.1.2.1: `client_max_window_bits` is never echoed, so the client is never bound to
        // a window it might not be able to use.
        let (_, response) =
            accept_offer(Some("permessage-deflate; client_max_window_bits=10")).unwrap();
        assert!(!response.contains("client_max_window_bits"));

        // An offer with an unknown parameter is skipped, and the next one is tried.
        let (_, response) =
            accept_offer(Some("permessage-deflate; nonsense=1, permessage-deflate")).unwrap();
        assert_eq!(response, "permessage-deflate");

        assert!(accept_offer(None).is_none());
        assert!(accept_offer(Some("x-some-other-extension")).is_none());
    }

    #[test]
    fn an_offer_constraining_the_servers_window_is_declined() {
        // §7.1.2.2: this backend cannot narrow its window, so the offer is declined rather
        // than accepted-and-violated — and a second offer without the parameter still works.
        assert!(accept_offer(Some("permessage-deflate; server_max_window_bits=10")).is_none());
        let (_, response) = accept_offer(Some(
            "permessage-deflate; server_max_window_bits=10, permessage-deflate",
        ))
        .unwrap();
        assert_eq!(response, "permessage-deflate");
        // 15 is what we use anyway, so an offer naming it is accepted.
        assert!(accept_offer(Some("permessage-deflate; server_max_window_bits=15")).is_some());
    }

    #[test]
    fn window_sizes_outside_the_defined_range_are_refused() {
        assert!(matches!(
            window_bits("16"),
            Err(NegotiationError::BadWindowBits(_))
        ));
        assert!(matches!(
            window_bits("7"),
            Err(NegotiationError::BadWindowBits(_))
        ));
        assert_eq!(
            window_bits("8"),
            Ok(9),
            "8 is legal in the header but not in DEFLATE"
        );
        assert_eq!(window_bits("15"), Ok(15));
    }

    #[test]
    fn a_duplicate_parameter_is_a_negotiation_failure() {
        assert_eq!(
            parse_offer(
                "permessage-deflate; server_no_context_takeover; server_no_context_takeover"
            ),
            Err(NegotiationError::Duplicate(
                "server_no_context_takeover".to_owned()
            ))
        );
    }

    #[test]
    fn the_client_reads_the_servers_answer() {
        // A server may narrow *its own* window; a 15-bit decompressor reads it regardless.
        let params = accept_response(Some("permessage-deflate; server_max_window_bits=12"))
            .unwrap()
            .unwrap();
        assert!(!params.server_no_context_takeover);

        let params = accept_response(Some("permessage-deflate; client_no_context_takeover"))
            .unwrap()
            .unwrap();
        assert!(params.client_no_context_takeover);

        // A response that narrows *our* window is one we cannot honour, and RFC 7692 says to
        // fail rather than silently violate it. The offer never invites this.
        assert_eq!(
            accept_response(Some("permessage-deflate; client_max_window_bits=10")),
            Err(NegotiationError::BadWindowBits("10".to_owned()))
        );

        assert_eq!(accept_response(None).unwrap(), None);
        assert_eq!(
            accept_response(Some("x-other")),
            Err(NegotiationError::NotOffered)
        );
    }
}
