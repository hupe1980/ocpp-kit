//! Signed meter values: the record a customer may actually be billed for.
//!
//! An OCPP meter value is *telemetry*. Under German calibration law (`MessEG` §33) a customer
//! may be billed for a measured value only if they can check it, and what they check is the
//! data set the meter itself signed — an OCMF or EDL record, carried through OCPP as an
//! opaque blob. This module is the protocol knowledge around that blob: how each version
//! carries it, and how to read the public-key field, which is not what its name suggests.
//!
//! # The signed value is not the protocol's number
//!
//! They are not even the same quantity. In the Open Charge Alliance's own example message
//! (*Signed Meter Values in OCPP* v1.0 §5.2) the 1.6 `StopTransaction` reports
//! `meterStop: 108814` — the meter's **lifetime** register, in Wh — while the signed data set
//! in the same message reports the transaction running `0.000 → 0.636` kWh. A CSMS that bills
//! `meterStop − meterStart` is not billing a slightly different number; it is billing a
//! different register. Whatever the protocol's own fields say, the billable figure comes out
//! of the signed record.
//!
//! # Where each version puts it
//!
//! * **2.0.1 and 2.1** — `SampledValue.signedMeterValue`, a typed object. Nothing to do.
//! * **1.6** — there is no such field. The OCA application note (§3.2.1) reuses 2.x's
//!   `SignedMeterValueType` by serializing the whole object into the `value` **string** of a
//!   `SampledValue` whose `format` is `SignedData`: a string holding JSON holding Base64
//!   holding the record. Nothing about that is guessable from the 1.6 schema, and it is how
//!   every calibration-law-compliant 1.6 station sends its billable value. Reading it is
//!   [`v1_6::SampledValue::signed_meter_value`](crate::v1_6::SampledValue::signed_meter_value).
//!
//! Both shapes reach [`SignedMeterValue`], which is what the version-neutral
//! [`csms::events`](crate::csms::events) funnel carries.
//!
//! # The bytes are a claim, not a binding
//!
//! [`SignedMeterValue::public_key`] answers what the station *said* the key is. OCMF requires
//! the public key to reach the customer out of band — from the certified meter, or a
//! registry — precisely because a key arriving on the same socket as the record it signs
//! proves only that whoever holds that socket owns some private key. Verify against a key you
//! obtained elsewhere; use this one to *display*, and to notice when a station's key changes.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Base64 that does not mind whether the padding is there.
///
/// Both shapes are in the field, and refusing one of them would reject a station over a
/// character that carries no information.
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );

/// A meter reading carrying the meter's own digital signature.
///
/// Version-neutral: 2.x's `SignedMeterValueType` and 1.6's `SignedData` string both become
/// this. The fields are carried through **verbatim** — the signature covers those exact
/// bytes, so nothing here re-encodes, re-wraps or normalizes them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedMeterValue {
    /// `signedMeterData`: the signed record, Base64 as sent. An OCMF data set, an EDL file —
    /// whatever [`encoding_method`](Self::encoding_method) names.
    pub signed_meter_data: String,
    /// `encodingMethod`: `OCMF`, `EDL`, …
    ///
    /// Required by both 2.x schemas, so always `Some` there. A 1.6 `SignedData` payload may
    /// omit it; the record is still the billable one, so it is carried rather than refused —
    /// an OCMF data set announces itself with an `OCMF|` prefix in any case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_method: Option<String>,
    /// `signingMethod`: the signature algorithm, when it is not already inside the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_method: Option<String>,
    /// `publicKey`: the key the station claims signed the record.
    ///
    /// Not key bytes — see [`public_key`](Self::public_key) for the two shapes this field is
    /// actually sent in, and for why it is a claim rather than a binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

impl SignedMeterValue {
    /// A signed meter value carrying just the record.
    #[must_use]
    pub fn new(signed_meter_data: impl Into<String>) -> Self {
        Self {
            signed_meter_data: signed_meter_data.into(),
            encoding_method: None,
            signing_method: None,
            public_key: None,
        }
    }

    /// Sets `encodingMethod`.
    #[must_use]
    pub fn with_encoding_method(mut self, method: impl Into<String>) -> Self {
        self.encoding_method = Some(method.into());
        self
    }

    /// Sets `signingMethod`.
    #[must_use]
    pub fn with_signing_method(mut self, method: impl Into<String>) -> Self {
        self.signing_method = Some(method.into());
        self
    }

    /// Sets `publicKey`.
    #[must_use]
    pub fn with_public_key(mut self, key: impl Into<String>) -> Self {
        self.public_key = Some(key.into());
        self
    }

    /// Parses the JSON document a 1.6 station puts in a `SignedData` sampled value.
    ///
    /// # Errors
    ///
    /// [`SignedDataError`] when the string is not the JSON object the application note
    /// describes.
    pub fn from_signed_data(value: &str) -> Result<Self, SignedDataError> {
        let parsed: Self = serde_json::from_str(value).map_err(|error| {
            SignedDataError::new(alloc::format!("not a SignedMeterValue document: {error}"))
        })?;
        if parsed.signed_meter_data.is_empty() {
            return Err(SignedDataError::new("signedMeterData is empty"));
        }
        Ok(parsed)
    }

    /// The record itself: the bytes the meter signed.
    ///
    /// `signedMeterData` is specified as Base64 (2.0.1 Part 2 §2.46) and is usually sent that
    /// way. Stations that put the record in plain are common enough that refusing them is not
    /// an option — and the refusal would be a quiet one, because a station whose record
    /// "is not Base64" simply stops being billable for a reason nobody looks for.
    ///
    /// The two can never collide, so both are read:
    ///
    /// * An **OCMF** record announces itself with the ASCII prefix `OCMF|`, and `|` is not in
    ///   the Base64 alphabet. Nothing Base64 can start that way.
    /// * Anything else containing a character outside the Base64 alphabet is likewise not
    ///   Base64 — an EDL record is XML, which starts `<`.
    /// * What is left is Base64 and is decoded.
    ///
    /// Nothing is re-encoded on the way out: these are the bytes the signature covers.
    ///
    /// # Errors
    ///
    /// [`SignedDataError`] when the field is empty — an empty record is not a record.
    pub fn decoded(&self) -> Result<Vec<u8>, SignedDataError> {
        let text = self.signed_meter_data.trim();
        if text.is_empty() {
            return Err(SignedDataError::new("signedMeterData is empty"));
        }
        if text.starts_with(OCMF_PREFIX) {
            return Ok(text.as_bytes().to_vec());
        }
        // Whitespace inside a Base64 field carries no information, so a line-wrapped record
        // is still one; the decoder does not skip it for us.
        let compact: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        if !is_base64_alphabet(&compact) {
            return Ok(text.as_bytes().to_vec());
        }
        // Padding-indifferent, so a station that omits `=` is not refused over a character
        // that carries no information either.
        BASE64
            .decode(&compact)
            .map_or_else(|_| Ok(text.as_bytes().to_vec()), Ok)
    }

    /// [`decoded`](Self::decoded) as text.
    ///
    /// OCMF is textual — a `|`-separated header, a JSON payload and a JSON signature — so
    /// this is what a consumer of an OCMF record actually wants. EDL is XML, also text.
    ///
    /// # Errors
    ///
    /// [`SignedDataError`] when the field is empty, or when the record is not UTF-8 — which
    /// means it is a binary format, and [`decoded`](Self::decoded) is the right call for it.
    pub fn decoded_str(&self) -> Result<String, SignedDataError> {
        String::from_utf8(self.decoded()?)
            .map_err(|_| SignedDataError::new("the signed record is not UTF-8 text"))
    }

    /// The JSON document a 1.6 station puts in a `SignedData` sampled value.
    ///
    /// The inverse of
    /// [`v1_6::SampledValue::signed_meter_value`](crate::v1_6::SampledValue::signed_meter_value),
    /// and the reason it is here rather than in every station: getting the shape right means
    /// knowing that 1.6 reuses 2.x's type by serialising it into a string, which nothing in
    /// the 1.6 schema says. [`v1_6::SampledValue::signed`](crate::v1_6::SampledValue::signed)
    /// wraps this together with the `format` the shape also requires.
    #[must_use]
    pub fn to_signed_data(&self) -> String {
        // The fields are the ones this type holds and all of them are strings, so this cannot
        // fail; a `Result` here would only make every caller unwrap it.
        serde_json::to_string(self).unwrap_or_else(|_| String::from("{}"))
    }

    /// The public key the station claims, in both shapes the field is sent in.
    ///
    /// `None` when the station sent no key at all — which is normal: OCPP 2.x gates the field
    /// on the `PublicKeyWithSignedMeterValue` configuration variable, and a key that travels
    /// out of band is the stronger arrangement anyway.
    ///
    /// # Errors
    ///
    /// [`PublicKeyError`] when the field is not Base64, or is an `oca:` envelope this cannot
    /// read. See [`PublicKey`] for what the two shapes are and why there are two.
    pub fn public_key(&self) -> Option<Result<PublicKey, PublicKeyError>> {
        self.public_key.as_deref().map(decode_public_key)
    }

    /// Just the key bytes — [`public_key`](Self::public_key) without the provenance.
    ///
    /// # Errors
    ///
    /// As [`public_key`](Self::public_key).
    pub fn public_key_bytes(&self) -> Option<Result<Vec<u8>, PublicKeyError>> {
        self.public_key().map(|result| result.map(|key| key.bytes))
    }
}

/// The `publicKey` field could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublicKeyError {
    /// The field is not Base64.
    NotBase64,
    /// An `oca:` envelope with fewer than four colon-separated parts.
    MalformedEnvelope,
    /// An `oca:` envelope naming an encoding this does not implement. The token is kept, so a
    /// caller that knows it can still act on the printed form.
    UnsupportedEncoding(String),
    /// The printed key does not spell a whole number of bytes in the encoding it names.
    MalformedKey,
}

impl fmt::Display for PublicKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublicKeyError::NotBase64 => f.write_str("publicKey is not Base64"),
            PublicKeyError::MalformedEnvelope => {
                f.write_str("publicKey is an oca: envelope with too few parts")
            }
            PublicKeyError::UnsupportedEncoding(encoding) => {
                write!(f, "publicKey names an unsupported encoding {encoding:?}")
            }
            PublicKeyError::MalformedKey => f.write_str("publicKey does not spell whole bytes"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PublicKeyError {}

/// A signed meter value that could not be read: a 1.6 `SignedData` string that is not the
/// JSON document the application note describes, or a `signedMeterData` field with no record
/// in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedDataError {
    reason: String,
}

impl SignedDataError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// What was wrong, for a log line an operator can act on.
    ///
    /// A station that *says* it is sending signed data and is not looks, from the outside,
    /// exactly like a station sending none — and the difference only surfaces when a month of
    /// sessions turns out to be unbillable. This is the string that stops that happening.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for SignedDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SignedDataError {}

/// How the `publicKey` field was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublicKeyShape {
    /// Base64 over `oca:<encoding>:<content-type>:<printed key>`, as the application note
    /// §3.2.2 specifies.
    Envelope,
    /// Base64 over the key printed as hexadecimal, with no envelope — the shape the same
    /// document's own example message uses.
    PrintedHex,
    /// Base64 over bytes that are not text in either shape; taken as the key itself.
    Opaque,
}

/// The public key a station claims signed a record.
///
/// # Two shapes, both conformant in practice
///
/// The OCA application note §3.2.2 specifies Base64 over a colon-separated envelope —
/// `oca:<encoding>:<content-type>:<printed public key>` — where the last part is the key **as
/// printed on the certified meter**, so that a customer can compare it against the label on
/// the cabinet. For `base16` it adds that non-hexadecimal characters and a leading `0x`
/// "SHALL be ignored", because a printed key has spaces in it.
///
/// The same document's example message (§5.2) then sends Base64 over plain uppercase
/// hexadecimal with no envelope at all. A reader that implements only the specification
/// rejects the whitepaper's own example; one that implements only the example rejects every
/// conformant station. Both are in the field, so both are read here, and
/// [`shape`](Self::shape) says which arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublicKey {
    /// The key bytes.
    pub bytes: Vec<u8>,
    /// The key as it was written — what to show a customer comparing it with the meter's
    /// label. `None` for [`PublicKeyShape::Opaque`], where there was no text.
    pub printed: Option<String>,
    /// The envelope's encoding token (`base16`, `base32`, `base64`), when there was an
    /// envelope.
    pub encoding: Option<String>,
    /// The envelope's content type — the key's own format, such as a curve name — when there
    /// was an envelope.
    pub content_type: Option<String>,
    /// Which shape the field arrived in.
    pub shape: PublicKeyShape,
}

/// A field the target version requires that a version-neutral value does not carry.
///
/// The versions disagree about which parts of a signed meter value are mandatory — 2.0.1
/// requires all four, 2.1 requires the record and its encoding — so a value read from one
/// version does not always fit another. Saying which field is missing is more use than
/// refusing without a reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissingField {
    /// The member the target version requires.
    pub field: &'static str,
    /// The version that requires it.
    pub version: crate::version::Version,
}

impl fmt::Display for MissingField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OCPP {} requires signedMeterValue.{}",
            self.version, self.field
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MissingField {}

/// How an OCMF record announces itself, in the text form (OCMF v1.0 §5).
const OCMF_PREFIX: &str = "OCMF|";

/// Whether every byte could appear in Base64 — the test that tells an encoded record from one
/// sent in plain.
fn is_base64_alphabet(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

/// Reads a `publicKey` field in either of the two shapes it is sent in.
///
/// # Errors
///
/// [`PublicKeyError`] when the field is not Base64, or is an envelope this cannot read.
pub fn decode_public_key(field: &str) -> Result<PublicKey, PublicKeyError> {
    let raw = BASE64
        .decode(field.trim())
        .map_err(|_| PublicKeyError::NotBase64)?;

    // Everything below only applies if the Base64 covered *text*. Anything else is the key.
    let Ok(text) = core::str::from_utf8(&raw) else {
        return Ok(PublicKey {
            bytes: raw,
            printed: None,
            encoding: None,
            content_type: None,
            shape: PublicKeyShape::Opaque,
        });
    };
    let text = text.trim();

    if let Some(rest) = text.strip_prefix("oca:") {
        // `oca:<encoding>:<content-type>:<printed key>`. The printed key may itself contain
        // colons, so only the first two are separators.
        let mut parts = rest.splitn(3, ':');
        let (Some(encoding), Some(content_type), Some(printed)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(PublicKeyError::MalformedEnvelope);
        };
        let bytes = decode_printed(printed, encoding)?;
        return Ok(PublicKey {
            bytes,
            printed: Some(printed.to_owned()),
            encoding: Some(encoding.to_owned()),
            content_type: Some(content_type.to_owned()),
            shape: PublicKeyShape::Envelope,
        });
    }

    // No envelope. The example message's shape is hexadecimal text; anything else is bytes.
    match decode_printed_hex(text) {
        Some(bytes) => Ok(PublicKey {
            bytes,
            printed: Some(text.to_owned()),
            encoding: None,
            content_type: None,
            shape: PublicKeyShape::PrintedHex,
        }),
        None => Ok(PublicKey {
            bytes: raw,
            printed: None,
            encoding: None,
            content_type: None,
            shape: PublicKeyShape::Opaque,
        }),
    }
}

/// Decodes a printed key in the encoding its envelope names.
fn decode_printed(printed: &str, encoding: &str) -> Result<Vec<u8>, PublicKeyError> {
    // The tokens are lower-case in the note; stations are not.
    if encoding.eq_ignore_ascii_case("base16") || encoding.eq_ignore_ascii_case("hex") {
        decode_base16(printed).ok_or(PublicKeyError::MalformedKey)
    } else if encoding.eq_ignore_ascii_case("base32") {
        decode_base32(printed).ok_or(PublicKeyError::MalformedKey)
    } else if encoding.eq_ignore_ascii_case("base64") {
        BASE64
            .decode(strip_ignored(printed, |byte| {
                byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/' || byte == b'='
            }))
            .map_err(|_| PublicKeyError::MalformedKey)
    } else {
        Err(PublicKeyError::UnsupportedEncoding(encoding.to_owned()))
    }
}

/// Keeps only the bytes the encoding's alphabet contains.
///
/// §3.2.2 requires it for `base16` — a key printed on a meter has spaces in it so a human can
/// read it back — and it is harmless for the others, whose alphabets do not contain the
/// separators either.
fn strip_ignored(text: &str, keep: impl Fn(u8) -> bool) -> Vec<u8> {
    text.bytes().filter(|byte| keep(*byte)).collect()
}

/// Hexadecimal, ignoring *every* character that is not a hex digit, and a leading `0x`.
///
/// That is what §3.2.2 requires of an envelope's `base16`: a key printed on a meter is broken
/// up with spaces so a human can read it back, and the note says those "SHALL be ignored".
fn decode_base16(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let digits = strip_ignored(text, |byte| byte.is_ascii_hexdigit());
    if digits.is_empty() || digits.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(bytes)
}

/// The same, but only when the text is *nothing but* hex digits and separators.
///
/// Used to recognise the un-enveloped shape, where there is no `oca:` prefix to say that the
/// text is a printed key. Ignoring stray characters there — which is right inside an envelope
/// that has already declared itself — would let any text at all be read as a key.
fn decode_printed_hex(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if body
        .bytes()
        .any(|byte| !byte.is_ascii_hexdigit() && !is_separator(byte))
    {
        return None;
    }
    decode_base16(trimmed)
}

/// What a printed key may be broken up with.
const fn is_separator(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'-' | b':' | b'.')
}

/// RFC 4648 base32, ignoring separators and padding.
fn decode_base32(text: &str) -> Option<Vec<u8>> {
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    let mut bytes = Vec::new();
    for byte in text.bytes() {
        if byte == b'=' || is_separator(byte) {
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => return None,
        };
        accumulator = (accumulator << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    // Whatever is left is the encoding's tail padding, and must be zero.
    if accumulator & ((1 << bits) - 1) != 0 || bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

// ---------------------------------------------------------------------------
// Per-version bridges
// ---------------------------------------------------------------------------

/// Generates the conversion from a version's generated `SignedMeterValueType`.
///
/// 2.0.1 makes `signingMethod` and `publicKey` mandatory and 2.1 makes both optional;
/// `Option::from` absorbs exactly that difference and nothing else.
#[cfg(any(feature = "v2_0_1", feature = "v2_1"))]
macro_rules! from_generated {
    ($module:ident) => {
        impl From<&crate::$module::SignedMeterValue> for SignedMeterValue {
            fn from(value: &crate::$module::SignedMeterValue) -> Self {
                Self {
                    signed_meter_data: value.signed_meter_data.clone(),
                    encoding_method: Option::from(value.encoding_method.clone()),
                    signing_method: Option::from(value.signing_method.clone()),
                    public_key: Option::from(value.public_key.clone()),
                }
            }
        }
    };
}

#[cfg(feature = "v2_0_1")]
from_generated!(v2_0_1);
#[cfg(feature = "v2_1")]
from_generated!(v2_1);

#[cfg(feature = "v2_0_1")]
impl TryFrom<&SignedMeterValue> for crate::v2_0_1::SignedMeterValue {
    type Error = MissingField;

    /// 2.0.1 makes every member of `SignedMeterValueType` mandatory, so a value that came
    /// from 2.1 or from a 1.6 `SignedData` document may not fit — those two let a station
    /// omit the signing method and the public key.
    fn try_from(value: &SignedMeterValue) -> Result<Self, MissingField> {
        let missing = |field| MissingField {
            field,
            version: crate::version::Version::V2_0_1,
        };
        Ok(Self::new(
            value.signed_meter_data.clone(),
            value
                .signing_method
                .clone()
                .ok_or_else(|| missing("signingMethod"))?,
            value
                .encoding_method
                .clone()
                .ok_or_else(|| missing("encodingMethod"))?,
            value
                .public_key
                .clone()
                .ok_or_else(|| missing("publicKey"))?,
        ))
    }
}

#[cfg(feature = "v2_1")]
impl TryFrom<&SignedMeterValue> for crate::v2_1::SignedMeterValue {
    type Error = MissingField;

    /// 2.1 requires only the record and the method it is encoded in; the signing method and
    /// the public key became optional, because both can already be inside the record and the
    /// key is better obtained out of band.
    fn try_from(value: &SignedMeterValue) -> Result<Self, MissingField> {
        let mut out = Self::new(
            value.signed_meter_data.clone(),
            value.encoding_method.clone().ok_or(MissingField {
                field: "encodingMethod",
                version: crate::version::Version::V2_1,
            })?,
        );
        out.signing_method.clone_from(&value.signing_method);
        out.public_key.clone_from(&value.public_key);
        Ok(out)
    }
}

#[cfg(feature = "v1_6")]
impl crate::v1_6::SampledValue {
    /// A sampled value carrying a signed meter value, in the shape 1.6 defines for it.
    ///
    /// Sets both halves of that shape: the record serialised into `value`, and the `format`
    /// of `SignedData` without which a CSMS reads the document as a measurement. A station
    /// writing this by hand gets the second half wrong, and nothing tells it so — the message
    /// is schema-valid either way.
    ///
    /// ```
    /// use ocpp_kit::metering::SignedMeterValue;
    /// use ocpp_kit::v1_6::{Measurand, ReadingContext, SampledValue};
    ///
    /// let record = SignedMeterValue::new("T0NNRnx7fQ==").with_encoding_method("OCMF");
    /// let sample = SampledValue::signed(&record)
    ///     .with_measurand(Measurand::EnergyActiveImportRegister)
    ///     .with_context(ReadingContext::TransactionEnd);
    ///
    /// // …and it reads back as what went in.
    /// assert_eq!(sample.signed_meter_value().unwrap().unwrap(), record);
    /// ```
    #[must_use]
    pub fn signed(value: &SignedMeterValue) -> Self {
        Self::new(value.to_signed_data()).with_format(crate::v1_6::ValueFormat::SignedData)
    }
    /// The signed meter value this sampled value carries, if it carries one.
    ///
    /// 1.6 has no `signedMeterValue` field. The OCA application note (§3.2.1) reuses 2.x's
    /// `SignedMeterValueType` by serializing the whole object into this sampled value's
    /// `value` **string**, with `format` set to `SignedData`:
    ///
    /// ```json
    /// {"format": "SignedData",
    ///  "value": "{\"signedMeterData\":\"T0NNRnx7…\",\"encodingMethod\":\"OCMF\"}",
    ///  "context": "Transaction.End", "measurand": "Energy.Active.Import.Register"}
    /// ```
    ///
    /// So `value` is a string holding JSON holding Base64 holding the record, and a CSMS
    /// reading it as a measurement — which is what the field is for everywhere else — finds a
    /// JSON document where it expected kilowatt-hours. Every calibration-law-compliant German
    /// 1.6 station sends its billable value this way.
    ///
    /// Returns `None` when `format` is not `SignedData`, and `Some(Err(_))` when it is but
    /// the string is not the document the note describes — which is worth surfacing rather
    /// than dropping, since the station is then unbillable and does not know it.
    #[must_use]
    pub fn signed_meter_value(&self) -> Option<Result<SignedMeterValue, SignedDataError>> {
        if self.format.as_ref()? != &crate::v1_6::ValueFormat::SignedData {
            return None;
        }
        Some(SignedMeterValue::from_signed_data(&self.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// Base64 over uppercase hexadecimal, no envelope — the shape the OCA's own example
    /// message (§5.2) uses, and the one a reader of §3.2.2 alone would reject.
    const EXAMPLE_KEY: &str = "MzA1OTMwMTMwNjA3MkE4NjQ4Q0UzRDAyMDEwNjA4MkE4NjQ4Q0UzRDAzMDEwNw==";

    /// Base64 over `oca:base16:secp256r1:<printed key>` — what §3.2.2 specifies, spaces and
    /// all, because the key is meant to be compared with the label on the cabinet.
    const ENVELOPE_KEY: &str =
        "b2NhOmJhc2UxNjpzZWNwMjU2cjE6MzAgNTkgMzAgMTMgMDYgMDcgMkEgODYgNDggQ0UgM0QgMDIgMDE=";

    #[test]
    fn both_shapes_of_the_public_key_field_are_read() {
        let bare = decode_public_key(EXAMPLE_KEY).expect("the example message's shape");
        assert_eq!(bare.shape, PublicKeyShape::PrintedHex);
        assert_eq!(&bare.bytes[..4], &[0x30, 0x59, 0x30, 0x13]);
        assert_eq!(bare.printed.as_deref().map(|key| &key[..4]), Some("3059"));
        assert_eq!(bare.encoding, None);

        let enveloped = decode_public_key(ENVELOPE_KEY).expect("the specified shape");
        assert_eq!(enveloped.shape, PublicKeyShape::Envelope);
        // The separators a human needs are ignored, exactly as §3.2.2 requires.
        assert_eq!(&enveloped.bytes[..4], &[0x30, 0x59, 0x30, 0x13]);
        assert_eq!(enveloped.encoding.as_deref(), Some("base16"));
        assert_eq!(enveloped.content_type.as_deref(), Some("secp256r1"));
        assert_eq!(
            enveloped.printed.as_deref(),
            Some("30 59 30 13 06 07 2A 86 48 CE 3D 02 01"),
            "the printed form is what a customer compares with the meter's label"
        );

        // Both decode to the same key, which is the whole point of reading both.
        assert_eq!(bare.bytes[..13], enveloped.bytes[..]);
    }

    #[test]
    fn base64_over_key_bytes_is_taken_as_the_key() {
        // Not text, so there is no printed form to show and no transformation to undo.
        let key = decode_public_key("MFkTAQ==").expect("bytes");
        assert_eq!(key.shape, PublicKeyShape::Opaque);
        assert_eq!(key.bytes, alloc::vec![0x30, 0x59, 0x13, 0x01]);
        assert_eq!(key.printed, None);
    }

    #[test]
    fn a_leading_0x_and_an_odd_digit_count_are_handled_as_the_note_says() {
        // "a hexadecimal prefix 0x SHALL be ignored"
        assert_eq!(decode_base16("0x3059").unwrap(), alloc::vec![0x30, 0x59]);
        assert_eq!(decode_base16("30-59:30.13").unwrap().len(), 4);
        // Half a byte is not a key.
        assert_eq!(decode_base16("305"), None);
        // Text that merely contains hex digits is not a printed key.
        assert_eq!(decode_printed_hex("hello dead beef"), None);
    }

    #[test]
    fn an_unreadable_field_says_which_way_it_is_unreadable() {
        assert_eq!(decode_public_key("!!!!"), Err(PublicKeyError::NotBase64));
        // base64("oca:base16") — an envelope with two parts instead of four.
        assert_eq!(
            decode_public_key("b2NhOmJhc2UxNg=="),
            Err(PublicKeyError::MalformedEnvelope)
        );
        // base64("oca:base99:x:3059")
        assert_eq!(
            decode_public_key("b2NhOmJhc2U5OTp4OjMwNTk="),
            Err(PublicKeyError::UnsupportedEncoding("base99".to_string()))
        );
    }

    #[test]
    fn base32_and_base64_envelopes_decode_too() {
        // base64("oca:base32:x:GBMTC===") — GBMTC base32-decodes to 0x30 0x59 0x31.
        let field = BASE64.encode("oca:base32:x:GBMTC===");
        assert_eq!(
            decode_public_key(&field).unwrap().bytes,
            alloc::vec![0x30, 0x59, 0x31]
        );
        let field = BASE64.encode("oca:base64:x:MFkTAQ==");
        assert_eq!(
            decode_public_key(&field).unwrap().bytes,
            alloc::vec![0x30, 0x59, 0x13, 0x01]
        );
    }

    /// The record has to come out of the transport intact, whichever way the station put it
    /// in. Refusing the plain form would be a *quiet* failure: the station keeps sending, and
    /// its sessions stop being billable for a reason nobody looks for.
    #[test]
    fn the_record_comes_out_whether_it_was_base64_or_plain() {
        const RECORD: &str = "OCMF|{\"FV\":\"1.0\",\"RD\":[{\"RV\":0.636}]}|{\"SD\":\"304402\"}";

        // Base64, as the specification says.
        let encoded = SignedMeterValue::new(BASE64.encode(RECORD)).with_encoding_method("OCMF");
        assert_eq!(encoded.decoded_str().unwrap(), RECORD);

        // Plain, as a great many stations actually send it. `|` is not in the Base64
        // alphabet, so the two shapes cannot be confused for one another.
        let plain = SignedMeterValue::new(RECORD).with_encoding_method("OCMF");
        assert_eq!(plain.decoded_str().unwrap(), RECORD);
        assert_eq!(plain.decoded().unwrap(), RECORD.as_bytes());

        // An EDL record is XML, which is not Base64 either.
        let xml = SignedMeterValue::new("<?xml version=\"1.0\"?><edl/>");
        assert_eq!(xml.decoded_str().unwrap(), "<?xml version=\"1.0\"?><edl/>");

        // Binary after decoding: bytes are available, text is not, and the error says so.
        let binary = SignedMeterValue::new(BASE64.encode([0xff, 0xfe, 0x00]));
        assert_eq!(binary.decoded().unwrap(), alloc::vec![0xff, 0xfe, 0x00]);
        assert!(binary.decoded_str().is_err());

        // Line-wrapped Base64 is still Base64; the decoder does not skip whitespace itself.
        let wrapped = SignedMeterValue::new(
            "T0NNRnx7IkZWIjoiMS4w\nIiwiUkQiOlt7IlJWIjowLjYzNn1dfXx7IlNEIjoiMzA0NDAyIn0=",
        );
        assert_eq!(wrapped.decoded_str().unwrap(), RECORD);

        // An empty record is not a record.
        assert!(SignedMeterValue::new("").decoded().is_err());
        assert!(SignedMeterValue::new("   ").decoded().is_err());
    }

    /// The bridge has to work in both directions. A Local Controller relaying between
    /// versions, a test harness, and a station emitting its own records all need the write
    /// side — and a station writing the 1.6 shape by hand gets it wrong silently, because the
    /// message is schema-valid whether or not `format` says `SignedData`.
    #[test]
    fn a_signed_meter_value_round_trips_through_every_version() {
        let record = SignedMeterValue::new("T0NNRnx7fQ==")
            .with_encoding_method("OCMF")
            .with_signing_method("ECDSA-secp256r1-SHA256")
            .with_public_key("MzA1OQ==");

        // 1.6: a JSON document inside the `value` string, with the format that says so.
        let sample = crate::v1_6::SampledValue::signed(&record);
        assert_eq!(sample.format, Some(crate::v1_6::ValueFormat::SignedData));
        assert_eq!(sample.signed_meter_value().unwrap().unwrap(), record);

        // 2.1: the record and its encoding are required, the rest optional.
        let modern = crate::v2_1::SignedMeterValue::try_from(&record).unwrap();
        assert_eq!(SignedMeterValue::from(&modern), record);

        // 2.0.1: every member is mandatory, so the same value fits — and one that came from
        // 2.1 without a signing method does not, which is worth saying out loud.
        let legacy = crate::v2_0_1::SignedMeterValue::try_from(&record).unwrap();
        assert_eq!(SignedMeterValue::from(&legacy), record);

        let partial = SignedMeterValue::new("T0NNRnx7fQ==").with_encoding_method("OCMF");
        let error = crate::v2_0_1::SignedMeterValue::try_from(&partial).unwrap_err();
        assert_eq!(error.field, "signingMethod");
        assert_eq!(
            error.to_string(),
            "OCPP 2.0.1 requires signedMeterValue.signingMethod"
        );
        // 2.1 takes it as it stands.
        assert!(crate::v2_1::SignedMeterValue::try_from(&partial).is_ok());

        // A record with no encoding named fits nowhere typed, and says which member is missing.
        let bare = SignedMeterValue::new("T0NNRnx7fQ==");
        assert_eq!(
            crate::v2_1::SignedMeterValue::try_from(&bare)
                .unwrap_err()
                .field,
            "encodingMethod"
        );
    }

    /// The 1.6 shape: a `value` string holding JSON holding Base64 holding the record.
    #[test]
    fn a_16_signed_data_value_is_a_json_document_in_a_string() {
        let document = r#"{"signedMeterData":"T0NNRnx7InBhZ2luYXRpb24iOiJUMSJ9",
             "encodingMethod":"OCMF","publicKey":"MzA1OQ=="}"#;
        let value = SignedMeterValue::from_signed_data(document).expect("the note's document");
        assert_eq!(value.signed_meter_data, "T0NNRnx7InBhZ2luYXRpb24iOiJUMSJ9");
        assert_eq!(value.encoding_method.as_deref(), Some("OCMF"));
        // signingMethod is absent from the OCA's own example, so it cannot be required.
        assert_eq!(value.signing_method, None);
        assert_eq!(
            value.public_key_bytes().unwrap().unwrap(),
            alloc::vec![0x30, 0x59]
        );

        // A measurement where a document belongs is reported, not silently dropped: the
        // station is unbillable and does not know it.
        assert!(SignedMeterValue::from_signed_data("12345.6").is_err());
        assert!(SignedMeterValue::from_signed_data(r#"{"encodingMethod":"OCMF"}"#).is_err());
    }
}
