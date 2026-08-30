//! RFC 6455 frame headers.

use core::fmt;

/// The frame types RFC 6455 defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpCode {
    /// A continuation of the previous data frame.
    Continuation,
    /// A UTF-8 text payload.
    Text,
    /// A binary payload.
    Binary,
    /// The close handshake.
    Close,
    /// A ping.
    Ping,
    /// A pong.
    Pong,
}

impl OpCode {
    /// Maps a 4-bit opcode. Reserved values are rejected rather than ignored: RFC 6455 §5.2
    /// requires failing the connection on one.
    pub(crate) const fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            0x0 => OpCode::Continuation,
            0x1 => OpCode::Text,
            0x2 => OpCode::Binary,
            0x8 => OpCode::Close,
            0x9 => OpCode::Ping,
            0xA => OpCode::Pong,
            _ => return None,
        })
    }

    pub(crate) const fn bits(self) -> u8 {
        match self {
            OpCode::Continuation => 0x0,
            OpCode::Text => 0x1,
            OpCode::Binary => 0x2,
            OpCode::Close => 0x8,
            OpCode::Ping => 0x9,
            OpCode::Pong => 0xA,
        }
    }

    /// Control frames must be unfragmented and at most 125 bytes (RFC 6455 §5.5).
    pub(crate) const fn is_control(self) -> bool {
        matches!(self, OpCode::Close | OpCode::Ping | OpCode::Pong)
    }
}

impl fmt::Display for OpCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OpCode::Continuation => "continuation",
            OpCode::Text => "text",
            OpCode::Binary => "binary",
            OpCode::Close => "close",
            OpCode::Ping => "ping",
            OpCode::Pong => "pong",
        })
    }
}

/// A parsed frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Header {
    pub(crate) fin: bool,
    /// Set on the first frame of a compressed message (RFC 7692 §6).
    pub(crate) rsv1: bool,
    pub(crate) rsv2: bool,
    pub(crate) rsv3: bool,
    pub(crate) opcode: OpCode,
    pub(crate) mask: Option<[u8; 4]>,
    pub(crate) length: u64,
    /// How many bytes the header itself occupies.
    pub(crate) prefix: usize,
}

/// Why a frame header could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderError {
    /// A reserved opcode (3–7, 11–15).
    ReservedOpcode(u8),
    /// A 16-bit length below 126, or a 64-bit length below 65 536: RFC 6455 §5.2 requires the
    /// *minimal* length encoding, and accepting a padded one lets two implementations
    /// disagree about frame boundaries.
    NonMinimalLength,
    /// The 64-bit length has its top bit set, which RFC 6455 forbids.
    LengthTooLarge,
}

/// Reads a header from the front of `buffer`.
///
/// Returns `Ok(None)` when the buffer does not yet hold a complete header.
pub(crate) fn read_header(buffer: &[u8]) -> Result<Option<Header>, HeaderError> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let first = buffer[0];
    let second = buffer[1];

    let opcode =
        OpCode::from_bits(first & 0x0F).ok_or(HeaderError::ReservedOpcode(first & 0x0F))?;
    let masked = second & 0x80 != 0;
    let short = second & 0x7F;

    let (length, mut offset) = match short {
        126 => {
            if buffer.len() < 4 {
                return Ok(None);
            }
            let length = u64::from(u16::from_be_bytes([buffer[2], buffer[3]]));
            if length < 126 {
                return Err(HeaderError::NonMinimalLength);
            }
            (length, 4)
        }
        127 => {
            if buffer.len() < 10 {
                return Ok(None);
            }
            let length = u64::from_be_bytes(buffer[2..10].try_into().expect("10 bytes"));
            if length & (1 << 63) != 0 {
                return Err(HeaderError::LengthTooLarge);
            }
            if length < 65536 {
                return Err(HeaderError::NonMinimalLength);
            }
            (length, 10)
        }
        short => (u64::from(short), 2),
    };

    let mask = if masked {
        if buffer.len() < offset + 4 {
            return Ok(None);
        }
        let key = [
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ];
        offset += 4;
        Some(key)
    } else {
        None
    };

    Ok(Some(Header {
        fin: first & 0x80 != 0,
        rsv1: first & 0x40 != 0,
        rsv2: first & 0x20 != 0,
        rsv3: first & 0x10 != 0,
        opcode,
        mask,
        length,
        prefix: offset,
    }))
}

/// Writes a header into `out`.
pub(crate) fn write_header(
    out: &mut Vec<u8>,
    fin: bool,
    rsv1: bool,
    opcode: OpCode,
    mask: Option<[u8; 4]>,
    length: usize,
) {
    let mut first = opcode.bits();
    if fin {
        first |= 0x80;
    }
    if rsv1 {
        first |= 0x40;
    }
    out.push(first);

    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    // The minimal encoding, which is what §5.2 requires.
    if length < 126 {
        out.push(mask_bit | u8::try_from(length).expect("below 126"));
    } else if let Ok(length) = u16::try_from(length) {
        out.push(mask_bit | 0x7E);
        out.extend_from_slice(&length.to_be_bytes());
    } else {
        out.push(mask_bit | 0x7F);
        out.extend_from_slice(&(length as u64).to_be_bytes());
    }
    if let Some(key) = mask {
        out.extend_from_slice(&key);
    }
}

/// Applies the masking key in place. Masking is its own inverse.
pub(crate) fn apply_mask(payload: &mut [u8], key: [u8; 4]) {
    // Word-at-a-time over the aligned prefix, byte-wise over the remainder. The key repeats
    // every four bytes, so the word form is exact rather than an approximation.
    let wide = u32::from_ne_bytes(key);
    let split = payload.len() - payload.len() % 4;
    let (words, rest) = payload.split_at_mut(split);
    for chunk in words.chunks_exact_mut(4) {
        let value = u32::from_ne_bytes(chunk.try_into().expect("4 bytes")) ^ wide;
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    // The remainder starts on a multiple of four, so it restarts at key[0].
    for (index, byte) in rest.iter_mut().enumerate() {
        *byte ^= key[index % 4];
    }
}

/// RFC 6455 §7.4 close codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseCode(pub u16);

impl CloseCode {
    /// 1000 — normal closure.
    pub const NORMAL: CloseCode = CloseCode(1000);
    /// 1001 — the endpoint is going away.
    pub const GOING_AWAY: CloseCode = CloseCode(1001);
    /// 1002 — a protocol error.
    pub const PROTOCOL_ERROR: CloseCode = CloseCode(1002);
    /// 1003 — a data type the endpoint cannot accept.
    pub const UNSUPPORTED: CloseCode = CloseCode(1003);
    /// 1007 — a payload that is not consistent with its type (invalid UTF-8).
    pub const INVALID_PAYLOAD: CloseCode = CloseCode(1007);
    /// 1008 — a message that violates policy.
    pub const POLICY: CloseCode = CloseCode(1008);
    /// 1009 — a message too big to process.
    pub const TOO_BIG: CloseCode = CloseCode(1009);
    /// 1011 — an unexpected condition.
    pub const INTERNAL_ERROR: CloseCode = CloseCode(1011);

    /// Whether a peer may send this code on the wire.
    ///
    /// 1004 is undefined, and 1005/1006/1015 are reserved for local use — a peer that sends
    /// one is signalling something it cannot actually know.
    #[must_use]
    pub const fn is_sendable(self) -> bool {
        matches!(self.0, 1000..=1003 | 1007..=1011 | 3000..=4999)
    }
}

impl fmt::Display for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The body of a close frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseFrame {
    /// Why the connection is closing.
    pub code: CloseCode,
    /// A human-readable reason, at most 123 bytes once encoded.
    pub reason: String,
}

impl CloseFrame {
    /// A close frame with a code and a reason.
    #[must_use]
    pub fn new(code: CloseCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_header_is_incomplete_not_invalid() {
        assert_eq!(read_header(&[]), Ok(None));
        assert_eq!(read_header(&[0x81]), Ok(None));
        // 126 announces a 16-bit length that has not arrived.
        assert_eq!(read_header(&[0x81, 126, 0x00]), Ok(None));
        // A masked frame needs its key too.
        assert_eq!(read_header(&[0x81, 0x83, 0x01, 0x02]), Ok(None));
    }

    #[test]
    fn headers_round_trip_through_every_length_form() {
        for length in [0usize, 1, 125, 126, 65_535, 65_536, 100_000] {
            let mut out = Vec::new();
            write_header(&mut out, true, false, OpCode::Text, None, length);
            let header = read_header(&out).unwrap().expect("complete");
            assert_eq!(header.length, length as u64, "length {length}");
            assert_eq!(header.prefix, out.len());
            assert!(header.fin);
            assert_eq!(header.opcode, OpCode::Text);
            assert_eq!(header.mask, None);
        }
    }

    #[test]
    fn a_padded_length_is_refused() {
        // 126 with a value below 126, and 127 with a value below 65 536: both are legal bytes
        // but illegal framing, and accepting them invites desynchronisation.
        assert_eq!(
            read_header(&[0x81, 126, 0x00, 0x05]),
            Err(HeaderError::NonMinimalLength)
        );
        assert_eq!(
            read_header(&[0x81, 127, 0, 0, 0, 0, 0, 0, 0x01, 0x00]),
            Err(HeaderError::NonMinimalLength)
        );
        // The top bit of a 64-bit length must be zero.
        assert_eq!(
            read_header(&[0x81, 127, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            Err(HeaderError::LengthTooLarge)
        );
    }

    #[test]
    fn reserved_opcodes_are_refused() {
        for bits in [0x3u8, 0x7, 0xB, 0xF] {
            assert_eq!(
                read_header(&[0x80 | bits, 0x00]),
                Err(HeaderError::ReservedOpcode(bits))
            );
        }
    }

    #[test]
    fn masking_is_its_own_inverse_at_every_alignment() {
        let key = [0xDE, 0xAD, 0xBE, 0xEF];
        for length in 0u32..=37 {
            let original: Vec<u8> = (0..length).map(u8::try_from).map(Result::unwrap).collect();
            let mut payload = original.clone();
            apply_mask(&mut payload, key);
            if length > 0 {
                assert_ne!(payload, original, "length {length} was left unmasked");
            }
            apply_mask(&mut payload, key);
            assert_eq!(payload, original, "length {length} did not round-trip");
        }
    }

    #[test]
    fn masking_matches_the_byte_wise_definition() {
        let key = [1, 2, 3, 4];
        let mut payload: Vec<u8> = (0..10).collect();
        let expected: Vec<u8> = (0..10u8).map(|i| i ^ key[usize::from(i) % 4]).collect();
        apply_mask(&mut payload, key);
        assert_eq!(payload, expected);
    }

    #[test]
    fn only_the_close_codes_a_peer_may_send_are_accepted() {
        assert!(CloseCode::NORMAL.is_sendable());
        assert!(CloseCode(3000).is_sendable());
        assert!(CloseCode(4999).is_sendable());
        // Reserved for local use: a peer cannot legitimately put these on the wire.
        for code in [999u16, 1004, 1005, 1006, 1015, 2999, 5000] {
            assert!(!CloseCode(code).is_sendable(), "{code}");
        }
    }
}
