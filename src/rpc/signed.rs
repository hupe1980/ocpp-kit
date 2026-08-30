//! Signed messages — OCPP 2.x Part 4 chapter 7 (feature `signed-messages`).
//!
//! Proof that a message came from the other *end*, not merely from whatever terminated the
//! TLS connection. §7.4 is explicit that a Local Controller's key mismatch is expected and
//! must not invalidate a signature.
//!
//! # The format
//!
//! For every message there is a signed equivalent. The `MessageTypeId` and `MessageId` stay
//! the same; the action gains a `-Signed` suffix; and the payload is replaced by the
//! [Flattened JWS JSON Serialization](https://www.rfc-editor.org/rfc/rfc7515#section-7.2.2)
//! of the original payload.
//!
//! ```text
//! [2, "19223201", "BootNotification",        {"reason": "PowerUp", …}]
//! [2, "19223201", "BootNotification-Signed", {"protected": "…", "payload": "…", "signature": "…"}]
//! ```
//!
//! The protected header adds `OCPPAction` and `OCPPMessageTypedId` (the specification's own
//! spelling), and should carry `x5t#S256` — the SHA-256 hash of the DER signing certificate —
//! so the verifier knows which key to use.
//!
//! # Bring your own key
//!
//! This module implements the *format*, not the cryptography. [`Signer`] and [`Verifier`] are
//! traits because §7.4 anticipates keys this crate could not reach — "a certificate stored in
//! the calibrated measuring chip". `jws-es256` adds a software ES256 pair for the common case.
//!
//! # Require, do not merely verify
//!
//! [`verify_frame`] takes a [`SignaturePolicy`] and has no default. A verifier can only check
//! a signature that is present, so "verify it if it is signed" accepts a frame whose signature
//! an intermediary deleted — the downgrade this chapter exists to prevent.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::frame::{Frame, MessageTypeId};

/// The suffix a signed action's name carries.
pub const SIGNED_SUFFIX: &str = "-Signed";

/// base64url without padding, which is what JWS uses everywhere.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The signature algorithms OCPP permits (Part 4 §7.3).
///
/// The set is deliberately the same as the TLS connection's, so a Charging Station does not
/// have to implement anything extra.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Algorithm {
    /// ECDSA using P-256 and SHA-256. Recommended by RFC 7518.
    Es256,
    /// RSASSA-PKCS1-v1_5 using SHA-256. Recommended by RFC 7518.
    Rs256,
    /// RSASSA-PKCS1-v1_5 using SHA-384.
    Rs384,
}

impl Algorithm {
    /// The `alg` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Algorithm::Es256 => "ES256",
            Algorithm::Rs256 => "RS256",
            Algorithm::Rs384 => "RS384",
        }
    }

    /// Parses an `alg` value, rejecting anything OCPP does not allow — including `none`,
    /// which would otherwise turn a signature check into a no-op.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "ES256" => Algorithm::Es256,
            "RS256" => Algorithm::Rs256,
            "RS384" => Algorithm::Rs384,
            _ => return None,
        })
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The JWS protected header, with OCPP's two extra fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtectedHeader {
    /// The signature algorithm.
    pub alg: String,
    /// The OCPP action the payload belongs to — without the `-Signed` suffix.
    #[serde(rename = "OCPPAction")]
    pub ocpp_action: String,
    /// The message type number (2–6).
    ///
    /// The specification spells the field `OCPPMessageTypedId`; the spelling is kept
    /// verbatim, typo and all, because that is what goes on the wire.
    #[serde(rename = "OCPPMessageTypedId")]
    pub ocpp_message_typed_id: u8,
    /// The SHA-256 hash of the DER encoding of the signing certificate (§7.4).
    #[serde(rename = "x5t#S256", default, skip_serializing_if = "Option::is_none")]
    pub x5t_s256: Option<String>,
    /// Anything else the header carried, preserved so a relayed signature still verifies.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ProtectedHeader {
    /// Builds a header for one message.
    #[must_use]
    pub fn new(algorithm: Algorithm, action: &str, message_type: MessageTypeId) -> Self {
        Self {
            alg: algorithm.as_str().to_owned(),
            ocpp_action: action.to_owned(),
            ocpp_message_typed_id: message_type.number(),
            x5t_s256: None,
            extra: serde_json::Map::new(),
        }
    }

    /// The algorithm, if it is one OCPP allows.
    #[must_use]
    pub fn algorithm(&self) -> Option<Algorithm> {
        Algorithm::from_wire(&self.alg)
    }

    /// Sets the certificate thumbprint.
    #[must_use]
    pub fn with_thumbprint(mut self, x5t_s256: impl Into<String>) -> Self {
        self.x5t_s256 = Some(x5t_s256.into());
        self
    }

    /// Rejects a `crit` extension this implementation does not understand (RFC 7515 §4.1.11).
    ///
    /// `crit` means the signature is void unless the named header fields are enforced too.
    /// OCPP defines no critical headers, so the only conforming value is absent or empty.
    fn check_critical(&self) -> Result<(), SignatureError> {
        let Some(crit) = self.extra.get("crit") else {
            return Ok(());
        };
        let Some(names) = crit.as_array() else {
            // RFC 7515 §4.1.11: `crit` must be an array of strings. Anything else is a
            // malformed header, not an empty one.
            return Err(SignatureError::UnsupportedCriticalHeader(crit.to_string()));
        };
        match names.first() {
            // Part 4 chapter 7 defines no critical header parameters, so any name at all is
            // one this implementation does not understand.
            Some(name) => Err(SignatureError::UnsupportedCriticalHeader(
                name.as_str().unwrap_or("?").to_owned(),
            )),
            None => Ok(()),
        }
    }
}

/// A signature in Flattened JWS JSON Serialization.
///
/// The three members are kept base64url-encoded, because the signing input is defined over
/// the *encoded* forms — re-encoding a decoded header would change the bytes that were
/// signed and break verification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Jws {
    /// base64url of the protected header's JSON.
    pub protected: String,
    /// base64url of the payload.
    pub payload: String,
    /// base64url of the signature.
    pub signature: String,
    /// The unprotected header. OCPP does not use it; it is carried so a relayed message
    /// round-trips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Jws {
    /// The bytes a signature is computed over: `BASE64URL(protected) || '.' || BASE64URL(payload)`.
    #[must_use]
    pub fn signing_input(&self) -> String {
        format!("{}.{}", self.protected, self.payload)
    }

    /// Decodes the protected header.
    pub fn header(&self) -> Result<ProtectedHeader, SignatureError> {
        let bytes = B64
            .decode(&self.protected)
            .map_err(|_| SignatureError::Malformed)?;
        serde_json::from_slice(&bytes).map_err(|_| SignatureError::Malformed)
    }

    /// Decodes the payload, which is the original OCPP payload object.
    pub fn payload_json(&self) -> Result<Box<RawValue>, SignatureError> {
        let bytes = B64
            .decode(&self.payload)
            .map_err(|_| SignatureError::Malformed)?;
        let text = String::from_utf8(bytes).map_err(|_| SignatureError::Malformed)?;
        RawValue::from_string(text).map_err(|_| SignatureError::Malformed)
    }

    /// Decodes the signature.
    pub fn signature_bytes(&self) -> Result<Vec<u8>, SignatureError> {
        B64.decode(&self.signature)
            .map_err(|_| SignatureError::Malformed)
    }

    /// Serializes to the flattened JSON form that goes in the payload slot.
    pub fn to_json(&self) -> Result<Box<RawValue>, SignatureError> {
        serde_json::value::to_raw_value(self).map_err(|_| SignatureError::Malformed)
    }

    /// Parses the flattened JSON form.
    pub fn parse(json: &RawValue) -> Result<Self, SignatureError> {
        serde_json::from_str(json.get()).map_err(|_| SignatureError::Malformed)
    }
}

/// Produces signatures.
pub trait Signer {
    /// Which algorithm this key uses.
    fn algorithm(&self) -> Algorithm;

    /// The `x5t#S256` value identifying the signing certificate, if one is known.
    fn thumbprint(&self) -> Option<String> {
        None
    }

    /// Signs the JWS signing input.
    ///
    /// For `ES256` the result must be the 64-byte `R || S` concatenation RFC 7518 requires,
    /// **not** a DER-encoded ECDSA signature — a mismatch there is the single most common
    /// JWS interoperability failure.
    fn sign(&self, signing_input: &[u8]) -> Result<Vec<u8>, SignatureError>;
}

/// Checks signatures.
pub trait Verifier {
    /// Verifies one signature.
    ///
    /// `header` is given in full so an implementation can pick the key by `x5t#S256`, and
    /// can refuse an algorithm it does not accept.
    fn verify(
        &self,
        header: &ProtectedHeader,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<(), SignatureError>;
}

/// Whether a frame is *required* to carry a signature.
///
/// There is no default: a verifier can only check a signature that is present, so accepting an
/// unsigned frame is the signature-stripping downgrade, not a lenient version of a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Every frame must carry a valid signature. An unsigned one is
    /// [`SignatureError::Unsigned`].
    Required,
    /// Verify a signature when one is present; accept the frame when it is not.
    ///
    /// For a fleet mid-migration, and only where something else already provides
    /// authenticity: an attacker who can modify the stream can also delete the signature.
    Optional,
}

/// A verifier that accepts everything.
///
/// Only for extracting a payload from a signed message when the signature is somebody else's
/// business — a Local Controller relaying end-to-end signed traffic, for instance. It
/// verifies nothing, and its name says so.
pub struct AcceptAnySignature;

impl Verifier for AcceptAnySignature {
    fn verify(&self, _: &ProtectedHeader, _: &[u8], _: &[u8]) -> Result<(), SignatureError> {
        Ok(())
    }
}

/// Why a signed message could not be produced or accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignatureError {
    /// The JWS structure, its base64url, or the header JSON is not well formed.
    Malformed,
    /// The `alg` is not one OCPP permits (Part 4 §7.3).
    UnsupportedAlgorithm(String),
    /// The signature did not verify.
    Invalid,
    /// The signing key is unavailable, or the signing operation failed.
    KeyUnavailable(String),
    /// The header's `OCPPAction` or `OCPPMessageTypedId` disagrees with the frame that
    /// carried it — a signature lifted from one message and pasted into another.
    HeaderMismatch {
        /// What the frame says.
        expected: String,
        /// What the protected header says.
        found: String,
    },
    /// This frame type cannot be signed.
    ///
    /// `CALLERROR` and `CALLRESULTERROR` have no payload slot the specification defines a
    /// signed form for.
    NotSignable(MessageTypeId),
    /// The frame carried no signature, and [`SignaturePolicy::Required`] was in force.
    ///
    /// Distinct from [`Invalid`](Self::Invalid): a signature that fails to verify is a broken
    /// or hostile signer; an absent one is often an intermediary that removed it.
    Unsigned,
    /// The protected header marks a `crit` extension this implementation does not understand
    /// (RFC 7515 §4.1.11).
    UnsupportedCriticalHeader(String),
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignatureError::Malformed => f.write_str("the signed payload is not a valid JWS"),
            SignatureError::UnsupportedAlgorithm(alg) => {
                write!(f, "{alg:?} is not an algorithm OCPP permits (Part 4 §7.3)")
            }
            SignatureError::Invalid => f.write_str("the signature did not verify"),
            SignatureError::KeyUnavailable(why) => write!(f, "signing key unavailable: {why}"),
            SignatureError::HeaderMismatch { expected, found } => {
                write!(
                    f,
                    "the protected header says {found}, but the frame says {expected}"
                )
            }
            SignatureError::NotSignable(message_type) => {
                write!(f, "a {message_type} has no defined signed form")
            }
            SignatureError::Unsigned => {
                f.write_str("the frame carries no signature, and one is required")
            }
            SignatureError::UnsupportedCriticalHeader(name) => write!(
                f,
                "the protected header marks {name:?} critical, and it is not understood (RFC 7515 §4.1.11)"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SignatureError {}

/// Builds the JWS for one payload.
pub fn sign_payload(
    payload: &RawValue,
    action: &str,
    message_type: MessageTypeId,
    signer: &dyn Signer,
) -> Result<Jws, SignatureError> {
    let mut header = ProtectedHeader::new(signer.algorithm(), action, message_type);
    header.x5t_s256 = signer.thumbprint();
    let header_json = serde_json::to_vec(&header).map_err(|_| SignatureError::Malformed)?;

    let jws = Jws {
        protected: B64.encode(&header_json),
        payload: B64.encode(payload.get().as_bytes()),
        signature: String::new(),
        header: None,
    };
    let signature = signer.sign(jws.signing_input().as_bytes())?;
    Ok(Jws {
        signature: B64.encode(&signature),
        ..jws
    })
}

/// Verifies a JWS and returns the payload it protects.
///
/// `action` and `message_type` come from the frame that carried the JWS; they are checked
/// against the protected header, so a signature cannot be lifted from one message onto
/// another.
pub fn verify_payload(
    jws: &Jws,
    action: &str,
    message_type: MessageTypeId,
    verifier: &dyn Verifier,
) -> Result<Box<RawValue>, SignatureError> {
    let header = jws.header()?;
    if header.algorithm().is_none() {
        return Err(SignatureError::UnsupportedAlgorithm(header.alg.clone()));
    }
    header.check_critical()?;
    if header.ocpp_action != action {
        return Err(SignatureError::HeaderMismatch {
            expected: action.to_owned(),
            found: header.ocpp_action.clone(),
        });
    }
    if header.ocpp_message_typed_id != message_type.number() {
        return Err(SignatureError::HeaderMismatch {
            expected: message_type.number().to_string(),
            found: header.ocpp_message_typed_id.to_string(),
        });
    }
    verifier.verify(
        &header,
        jws.signing_input().as_bytes(),
        &jws.signature_bytes()?,
    )?;
    jws.payload_json()
}

/// The action a `-Signed` name wraps, or `None` when the name is not a signed one.
#[must_use]
pub fn unsigned_action(action: &str) -> Option<&str> {
    action.strip_suffix(SIGNED_SUFFIX)
}

/// Whether an action name is a signed one.
#[must_use]
pub fn is_signed_action(action: &str) -> bool {
    action.ends_with(SIGNED_SUFFIX)
}

/// Wraps a frame in its signed equivalent.
///
/// A `CALL` and a `SEND` carry the action themselves; a `CALLRESULT` does not, so
/// `request_action` supplies the action of the `CALL` it answers — that is what goes into the
/// protected header's `OCPPAction`.
pub fn sign_frame(
    frame: &Frame<'_>,
    signer: &dyn Signer,
    request_action: Option<&str>,
) -> Result<Frame<'static>, SignatureError> {
    let message_type = frame.message_type();
    match frame {
        Frame::Call {
            id,
            action,
            payload,
        }
        | Frame::Send {
            id,
            action,
            payload,
        } => {
            let jws = sign_payload(payload, action, message_type, signer)?;
            let signed_action = format!("{action}{SIGNED_SUFFIX}");
            let payload = alloc::borrow::Cow::Owned(jws.to_json()?);
            Ok(if message_type == MessageTypeId::Call {
                Frame::Call {
                    id: id.clone(),
                    action: signed_action.into(),
                    payload,
                }
            } else {
                Frame::Send {
                    id: id.clone(),
                    action: signed_action.into(),
                    payload,
                }
            })
        }
        Frame::CallResult { id, payload } => {
            let action = request_action.ok_or_else(|| {
                SignatureError::KeyUnavailable(
                    "signing a CALLRESULT needs the action of the CALL it answers".to_owned(),
                )
            })?;
            let jws = sign_payload(payload, action, message_type, signer)?;
            Ok(Frame::CallResult {
                id: id.clone(),
                payload: alloc::borrow::Cow::Owned(jws.to_json()?),
            })
        }
        Frame::CallError { .. } | Frame::CallResultError { .. } => {
            Err(SignatureError::NotSignable(message_type))
        }
    }
}

/// Unwraps a signed frame back into the plain one, verifying the signature on the way.
///
/// Under [`SignaturePolicy::Required`] an unsigned frame is [`SignatureError::Unsigned`];
/// under [`Optional`](SignaturePolicy::Optional) it is returned unchanged.
///
/// `CALLERROR` and `CALLRESULTERROR` have no signed form (§7.2), so they are returned
/// unchanged whatever the policy.
pub fn verify_frame(
    frame: &Frame<'_>,
    verifier: &dyn Verifier,
    request_action: Option<&str>,
    policy: SignaturePolicy,
) -> Result<Frame<'static>, SignatureError> {
    let message_type = frame.message_type();
    match frame {
        Frame::Call {
            id,
            action,
            payload,
        }
        | Frame::Send {
            id,
            action,
            payload,
        } => {
            let Some(action) = unsigned_action(action) else {
                return unsigned(frame, policy);
            };
            let jws = Jws::parse(payload)?;
            let payload = verify_payload(&jws, action, message_type, verifier)?;
            let payload = alloc::borrow::Cow::Owned(payload);
            Ok(if message_type == MessageTypeId::Call {
                Frame::Call {
                    id: id.clone(),
                    action: action.to_owned().into(),
                    payload,
                }
            } else {
                Frame::Send {
                    id: id.clone(),
                    action: action.to_owned().into(),
                    payload,
                }
            })
        }
        Frame::CallResult { id, payload } => {
            // A CALLRESULT carries no action, so nothing in the frame says whether it is
            // signed. `request_action` is the caller telling us what it asked for, which is
            // also the only thing that can go in the protected header.
            let Some(action) = request_action else {
                return unsigned(frame, policy);
            };
            let Ok(jws) = Jws::parse(payload) else {
                return unsigned(frame, policy);
            };
            let payload = verify_payload(&jws, action, message_type, verifier)?;
            Ok(Frame::CallResult {
                id: id.clone(),
                payload: alloc::borrow::Cow::Owned(payload),
            })
        }
        // §7.2 defines no signed form for an error frame, so there is nothing to require.
        Frame::CallError { .. } | Frame::CallResultError { .. } => Ok(frame.clone().into_owned()),
    }
}

/// What an unsigned frame means, under `policy`.
fn unsigned(frame: &Frame<'_>, policy: SignaturePolicy) -> Result<Frame<'static>, SignatureError> {
    match policy {
        SignaturePolicy::Required => Err(SignatureError::Unsigned),
        SignaturePolicy::Optional => Ok(frame.clone().into_owned()),
    }
}

// ---------------------------------------------------------------------------
// A software ES256 implementation
// ---------------------------------------------------------------------------

#[cfg(feature = "jws-es256")]
mod es256 {
    use super::{Algorithm, B64, ProtectedHeader, SignatureError, Signer, Verifier};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use base64::Engine as _;
    use p256::ecdsa::signature::{Signer as _, Verifier as _};
    use p256::ecdsa::{Signature, SigningKey, VerifyingKey};

    /// The SHA-256 thumbprint of a DER-encoded certificate, base64url-encoded — the
    /// `x5t#S256` value Part 4 §7.4 asks for.
    #[must_use]
    pub fn thumbprint(certificate_der: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        B64.encode(Sha256::digest(certificate_der))
    }

    /// An ES256 signer holding a P-256 private key in memory.
    ///
    /// Fine for a CSMS. A Charging Station with a secure element should implement
    /// [`Signer`](super::Signer) against it instead, and never let the key reach RAM.
    pub struct Es256Signer {
        key: SigningKey,
        thumbprint: Option<String>,
    }

    impl Es256Signer {
        /// Wraps a P-256 signing key.
        #[must_use]
        pub fn new(key: SigningKey) -> Self {
            Self {
                key,
                thumbprint: None,
            }
        }

        /// Reads a key from its 32-byte big-endian scalar.
        pub fn from_bytes(bytes: &[u8]) -> Result<Self, SignatureError> {
            let key = SigningKey::from_slice(bytes)
                .map_err(|error| SignatureError::KeyUnavailable(error.to_string()))?;
            Ok(Self::new(key))
        }

        /// Declares the certificate this key belongs to, so `x5t#S256` identifies it.
        #[must_use]
        pub fn with_certificate(mut self, certificate_der: &[u8]) -> Self {
            self.thumbprint = Some(thumbprint(certificate_der));
            self
        }

        /// The matching verifier.
        #[must_use]
        pub fn verifier(&self) -> Es256Verifier {
            Es256Verifier::new(*self.key.verifying_key())
        }
    }

    impl Signer for Es256Signer {
        fn algorithm(&self) -> Algorithm {
            Algorithm::Es256
        }

        fn thumbprint(&self) -> Option<String> {
            self.thumbprint.clone()
        }

        fn sign(&self, signing_input: &[u8]) -> Result<Vec<u8>, SignatureError> {
            let signature: Signature = self.key.sign(signing_input);
            // RFC 7518 §3.4: the JWS signature is the fixed-width `R || S` concatenation,
            // never the DER encoding an ECDSA library hands you by default.
            Ok(signature.to_bytes().to_vec())
        }
    }

    /// An ES256 verifier for one public key.
    pub struct Es256Verifier {
        key: VerifyingKey,
    }

    impl Es256Verifier {
        /// Wraps a P-256 verifying key.
        #[must_use]
        pub fn new(key: VerifyingKey) -> Self {
            Self { key }
        }

        /// Reads a key from its SEC1 encoding.
        pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self, SignatureError> {
            let key = VerifyingKey::from_sec1_bytes(bytes)
                .map_err(|error| SignatureError::KeyUnavailable(error.to_string()))?;
            Ok(Self::new(key))
        }
    }

    impl Verifier for Es256Verifier {
        fn verify(
            &self,
            header: &ProtectedHeader,
            signing_input: &[u8],
            signature: &[u8],
        ) -> Result<(), SignatureError> {
            if header.algorithm() != Some(Algorithm::Es256) {
                return Err(SignatureError::UnsupportedAlgorithm(header.alg.clone()));
            }
            let signature =
                Signature::from_slice(signature).map_err(|_| SignatureError::Invalid)?;
            self.key
                .verify(signing_input, &signature)
                .map_err(|_| SignatureError::Invalid)
        }
    }
}

#[cfg(feature = "jws-es256")]
pub use es256::{Es256Signer, Es256Verifier, thumbprint};

#[cfg(all(test, feature = "jws-es256"))]
mod tests {
    use super::*;
    use crate::types::MessageId;
    use crate::version::Version;

    /// A fixed key, so the tests are deterministic.
    fn key() -> Es256Signer {
        Es256Signer::from_bytes(&[7u8; 32])
            .unwrap()
            .with_certificate(b"a-der-certificate")
    }

    fn call() -> Frame<'static> {
        Frame::Call {
            id: MessageId::new("19223201").unwrap(),
            action: "BootNotification".into(),
            payload: alloc::borrow::Cow::Owned(
                RawValue::from_string(r#"{"reason":"PowerUp"}"#.to_owned()).unwrap(),
            ),
        }
    }

    #[test]
    fn a_signed_call_carries_the_suffix_and_round_trips() {
        let es256 = key();
        let signed = sign_frame(&call(), &es256, None).unwrap();

        assert_eq!(signed.action(), Some("BootNotification-Signed"));
        assert!(is_signed_action(signed.action().unwrap()));
        assert_eq!(
            unsigned_action(signed.action().unwrap()),
            Some("BootNotification")
        );
        // The signed frame is a perfectly ordinary OCPP-J frame.
        let text = signed.to_json(Version::V2_1).unwrap();
        assert!(text.starts_with(r#"[2,"19223201","BootNotification-Signed","#));
        assert_eq!(Frame::parse(&text, Version::V2_1).unwrap(), signed);

        let verified =
            verify_frame(&signed, &es256.verifier(), None, SignaturePolicy::Required).unwrap();
        assert_eq!(verified, call());
    }

    #[test]
    fn the_protected_header_carries_the_ocpp_fields() {
        let es256 = key();
        let signed = sign_frame(&call(), &es256, None).unwrap();
        let jws = Jws::parse(signed.payload().unwrap()).unwrap();
        let header = jws.header().unwrap();

        assert_eq!(header.algorithm(), Some(Algorithm::Es256));
        assert_eq!(header.ocpp_action, "BootNotification");
        assert_eq!(header.ocpp_message_typed_id, 2);
        assert!(
            header.x5t_s256.is_some(),
            "x5t#S256 identifies the signing certificate"
        );
        // The spelling of both OCPP fields is the specification's, typo included.
        let raw: serde_json::Value =
            serde_json::from_slice(&B64.decode(&jws.protected).unwrap()).unwrap();
        assert!(raw.get("OCPPAction").is_some());
        assert!(raw.get("OCPPMessageTypedId").is_some());
        assert!(raw.get("x5t#S256").is_some());
    }

    #[test]
    fn a_tampered_payload_does_not_verify() {
        let es256 = key();
        let signed = sign_frame(&call(), &es256, None).unwrap();
        let mut jws = Jws::parse(signed.payload().unwrap()).unwrap();
        jws.payload = B64.encode(br#"{"reason":"FirmwareUpdate"}"#);

        let tampered = Frame::Call {
            id: MessageId::new("19223201").unwrap(),
            action: "BootNotification-Signed".into(),
            payload: alloc::borrow::Cow::Owned(jws.to_json().unwrap()),
        };
        assert_eq!(
            verify_frame(
                &tampered,
                &es256.verifier(),
                None,
                SignaturePolicy::Required
            )
            .unwrap_err(),
            SignatureError::Invalid
        );
    }

    #[test]
    fn a_signature_cannot_be_moved_to_another_action() {
        let es256 = key();
        let signed = sign_frame(&call(), &es256, None).unwrap();
        // The same JWS, presented as a different action.
        let moved = Frame::Call {
            id: MessageId::new("19223201").unwrap(),
            action: "Heartbeat-Signed".into(),
            payload: alloc::borrow::Cow::Owned(signed.payload().unwrap().to_owned()),
        };
        assert!(matches!(
            verify_frame(&moved, &es256.verifier(), None, SignaturePolicy::Required).unwrap_err(),
            SignatureError::HeaderMismatch { .. }
        ));
    }

    /// The attack message-level signing exists to stop: an intermediary deletes the `-Signed`
    /// suffix and the JWS wrapper, and the receiver accepts the payload as if it had been
    /// verified. Under `Required` it is refused; under `Optional` it is not, and the
    /// documentation says so rather than pretending otherwise.
    #[test]
    fn a_stripped_signature_is_refused_when_signatures_are_required() {
        let es256 = key();
        let plain = call();

        assert_eq!(
            verify_frame(&plain, &es256.verifier(), None, SignaturePolicy::Required).unwrap_err(),
            SignatureError::Unsigned
        );
        assert_eq!(
            verify_frame(&plain, &es256.verifier(), None, SignaturePolicy::Optional).unwrap(),
            plain
        );

        // The same for a CALLRESULT, where nothing in the frame says whether it was signed.
        let result = Frame::CallResult {
            id: MessageId::new("19223201").unwrap(),
            payload: alloc::borrow::Cow::Owned(
                RawValue::from_string(r#"{"status":"Accepted"}"#.to_owned()).unwrap(),
            ),
        };
        assert_eq!(
            verify_frame(
                &result,
                &es256.verifier(),
                Some("BootNotification"),
                SignaturePolicy::Required
            )
            .unwrap_err(),
            SignatureError::Unsigned
        );

        // An error frame has no signed form at all (§7.2), so requiring one would make every
        // CALLERROR unusable.
        let error = Frame::CallError {
            id: MessageId::new("19223201").unwrap(),
            error: crate::rpc::CallError::new(crate::rpc::ErrorCode::GenericError, "").into(),
        };
        assert!(
            verify_frame(&error, &es256.verifier(), None, SignaturePolicy::Required).is_ok(),
            "an error frame has no signed form to require"
        );
    }

    /// RFC 7515 §4.1.11: `crit` means "reject this signature unless you enforce these header
    /// fields too". OCPP defines none, so any `crit` entry is one we cannot honour — and
    /// honouring a signature whose own constraints we ignored is worse than refusing it.
    #[test]
    fn a_critical_header_we_do_not_understand_is_refused() {
        let es256 = key();
        let signed = sign_frame(&call(), &es256, None).unwrap();
        let jws = Jws::parse(signed.payload().unwrap()).unwrap();

        let mut header = jws.header().unwrap();
        header
            .extra
            .insert("crit".into(), serde_json::json!(["exp"]));
        header.extra.insert("exp".into(), serde_json::json!(0));
        let header_json = serde_json::to_vec(&header).unwrap();
        // Re-sign, so the failure is the `crit` and not a broken signature.
        let unsigned_jws = Jws {
            protected: B64.encode(&header_json),
            payload: jws.payload.clone(),
            signature: String::new(),
            header: None,
        };
        let signature = es256.sign(unsigned_jws.signing_input().as_bytes()).unwrap();
        let critical = Jws {
            signature: B64.encode(&signature),
            ..unsigned_jws
        };

        assert_eq!(
            verify_payload(
                &critical,
                "BootNotification",
                MessageTypeId::Call,
                &es256.verifier()
            )
            .unwrap_err(),
            SignatureError::UnsupportedCriticalHeader("exp".to_owned())
        );

        // An empty `crit` array constrains nothing, so it is not a reason to refuse.
        let mut header = jws.header().unwrap();
        header.extra.insert("crit".into(), serde_json::json!([]));
        assert!(header.check_critical().is_ok());
    }

    #[test]
    fn a_call_result_is_signed_under_the_requests_action() {
        let es256 = key();
        let result = Frame::CallResult {
            id: MessageId::new("19223201").unwrap(),
            payload: alloc::borrow::Cow::Owned(
                RawValue::from_string(r#"{"status":"Accepted"}"#.to_owned()).unwrap(),
            ),
        };
        // A CALLRESULT has no action element, so the caller supplies the CALL's.
        assert!(matches!(
            sign_frame(&result, &es256, None).unwrap_err(),
            SignatureError::KeyUnavailable(_)
        ));
        let signed = sign_frame(&result, &es256, Some("BootNotification")).unwrap();
        let verified = verify_frame(
            &signed,
            &es256.verifier(),
            Some("BootNotification"),
            SignaturePolicy::Required,
        )
        .unwrap();
        assert_eq!(verified, result);
    }

    #[test]
    fn an_error_frame_has_no_signed_form() {
        let es256 = key();
        let error = Frame::CallError {
            id: MessageId::new("1").unwrap(),
            error: crate::rpc::CallError::internal("boom").into(),
        };
        assert_eq!(
            sign_frame(&error, &es256, None).unwrap_err(),
            SignatureError::NotSignable(MessageTypeId::CallError)
        );
    }

    #[test]
    fn the_none_algorithm_is_refused() {
        let mut header = ProtectedHeader::new(Algorithm::Es256, "Heartbeat", MessageTypeId::Call);
        header.alg = "none".to_owned();
        assert_eq!(header.algorithm(), None);

        let jws = Jws {
            protected: B64.encode(serde_json::to_vec(&header).unwrap()),
            payload: B64.encode(b"{}"),
            signature: String::new(),
            header: None,
        };
        assert_eq!(
            verify_payload(&jws, "Heartbeat", MessageTypeId::Call, &AcceptAnySignature)
                .unwrap_err(),
            SignatureError::UnsupportedAlgorithm("none".to_owned())
        );
    }
}
