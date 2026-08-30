//! Security profiles 1–3 and the credential rules that go with them.
//!
//! OCPP defines three profiles (2.x Part 2 §A 1.3 Table 12; 1.6 Security Whitepaper ed. 2):
//!
//! | Profile | Transport | Charging Station authenticates with | CSMS authenticates with |
//! |---|---|---|---|
//! | 1 | plain WebSocket | HTTP Basic | – |
//! | 2 | TLS | HTTP Basic | server certificate |
//! | 3 | TLS | client certificate | server certificate |
//!
//! Profile 1 is only acceptable on a network that is already trusted end to end; the
//! specification says so, and so does [`SecurityProfile::is_transport_encrypted`].

use core::fmt;

use zeroize::Zeroizing;

use crate::types::Identity;
use crate::version::Version;

/// Which security profile a connection uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SecurityProfile {
    /// 1 — unsecured transport with HTTP Basic authentication.
    BasicAuth,
    /// 2 — TLS with HTTP Basic authentication.
    TlsBasicAuth,
    /// 3 — TLS with a client certificate.
    TlsClientCertificate,
}

impl SecurityProfile {
    /// The profile number as it appears in `SecurityCtrlr.SecurityProfile`.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            SecurityProfile::BasicAuth => 1,
            SecurityProfile::TlsBasicAuth => 2,
            SecurityProfile::TlsClientCertificate => 3,
        }
    }

    /// Maps a `SecurityCtrlr.SecurityProfile` value.
    #[must_use]
    pub const fn from_number(number: u8) -> Option<Self> {
        Some(match number {
            1 => SecurityProfile::BasicAuth,
            2 => SecurityProfile::TlsBasicAuth,
            3 => SecurityProfile::TlsClientCertificate,
            _ => return None,
        })
    }

    /// Whether the transport is TLS.
    #[must_use]
    pub const fn is_transport_encrypted(self) -> bool {
        !matches!(self, SecurityProfile::BasicAuth)
    }

    /// Whether HTTP Basic credentials are sent.
    #[must_use]
    pub const fn uses_basic_auth(self) -> bool {
        matches!(
            self,
            SecurityProfile::BasicAuth | SecurityProfile::TlsBasicAuth
        )
    }
}

impl fmt::Display for SecurityProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "security profile {}", self.number())
    }
}

/// The HTTP Basic password a Charging Station authenticates with.
///
/// The two versions encode it differently and the builder enforces the right one:
///
/// * **1.6** `AuthorizationKey` is a *hexadecimal* string; the octets it decodes to are the
///   password (Security Whitepaper ed. 2).
/// * **2.x** `BasicAuthPassword` is sent as UTF-8, never hex- or base64-encoded, and is at
///   least 16 characters long (A00.FR.205).
///
/// The value is zeroized when dropped.
#[derive(Clone)]
pub struct BasicAuthPassword {
    secret: Zeroizing<Vec<u8>>,
}

/// The shortest `BasicAuthPassword` OCPP 2.x allows (A00.FR.205).
pub const BASIC_AUTH_MIN_LEN: usize = 16;

/// The longest `BasicAuthPassword` OCPP 2.x can define.
///
/// A00.FR.205 does not name a single maximum: it says the ceiling is the `maxLimit` of the
/// `BasicAuthPassword` variable, "which must be at least 40 characters and at most 64". 40 is
/// therefore the shortest maximum a CSMS may impose — the *floor* of the ceiling — and 64 is
/// the longest password the specification can ever describe. This constant is the latter,
/// because refusing a 48-character password an operator legitimately configured would be a
/// bug in this crate, not a conformance win.
pub const BASIC_AUTH_MAX_LEN: usize = 64;

/// The smallest `maxLimit` a CSMS may configure for `BasicAuthPassword` (A00.FR.205).
///
/// A Charging Station that must interoperate with any CSMS should stay at or below this.
pub const BASIC_AUTH_INTEROPERABLE_MAX_LEN: usize = 40;

impl BasicAuthPassword {
    /// A 2.x `BasicAuthPassword`: at least 16 characters, at most
    /// [`BASIC_AUTH_MAX_LEN`], sent as UTF-8.
    pub fn utf8(password: impl Into<String>) -> Result<Self, CredentialError> {
        let password = password.into();
        let len = password.chars().count();
        if !(BASIC_AUTH_MIN_LEN..=BASIC_AUTH_MAX_LEN).contains(&len) {
            return Err(CredentialError::PasswordLength(len));
        }
        Ok(Self {
            secret: Zeroizing::new(password.into_bytes()),
        })
    }

    /// A 1.6 `AuthorizationKey`, given as the hexadecimal string the specification uses.
    ///
    /// The decoded octets are what goes on the wire, not the hex text.
    pub fn hex(key: &str) -> Result<Self, CredentialError> {
        let text = key.trim();
        if text.len() % 2 != 0 || text.is_empty() {
            return Err(CredentialError::NotHex);
        }
        let mut bytes = Vec::with_capacity(text.len() / 2);
        for pair in text.as_bytes().chunks(2) {
            let digits = core::str::from_utf8(pair).map_err(|_| CredentialError::NotHex)?;
            bytes.push(u8::from_str_radix(digits, 16).map_err(|_| CredentialError::NotHex)?);
        }
        Ok(Self {
            secret: Zeroizing::new(bytes),
        })
    }

    /// Uses the bytes verbatim, skipping every version rule.
    ///
    /// For talking to a CSMS whose credentials predate the rules.
    #[must_use]
    pub fn raw(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: Zeroizing::new(bytes.into()),
        }
    }

    /// Builds the password `version` prescribes, from the text an operator configured.
    pub fn for_version(version: Version, secret: &str) -> Result<Self, CredentialError> {
        match version {
            Version::V1_6 => Self::hex(secret),
            _ => Self::utf8(secret),
        }
    }

    /// The octets that go into the HTTP Basic credentials.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.secret
    }

    /// Compares two passwords without leaking the position of the first difference.
    #[must_use]
    pub fn verify(&self, candidate: &[u8]) -> bool {
        constant_time_eq(&self.secret, candidate)
    }
}

impl fmt::Debug for BasicAuthPassword {
    /// Never prints the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BasicAuthPassword(<redacted>)")
    }
}

/// Compares two byte strings in time that depends only on their lengths.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // The length itself is not secret; the contents are.
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Why a credential was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialError {
    /// The password is shorter than [`BASIC_AUTH_MIN_LEN`] or longer than
    /// [`BASIC_AUTH_MAX_LEN`] characters (A00.FR.205).
    PasswordLength(usize),
    /// The 1.6 `AuthorizationKey` is not a hexadecimal string.
    NotHex,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::PasswordLength(len) => write!(
                f,
                "BasicAuthPassword is {len} characters; OCPP 2.x allows {BASIC_AUTH_MIN_LEN}–{BASIC_AUTH_MAX_LEN} (A00.FR.205)"
            ),
            CredentialError::NotHex => {
                f.write_str("the 1.6 AuthorizationKey must be an even-length hexadecimal string")
            }
        }
    }
}

impl std::error::Error for CredentialError {}

/// Encodes HTTP Basic credentials for `identity`.
///
/// The identity is the user name (A00.FR.204 forbids `:` in it, which [`Identity`] already
/// enforces), and the password's *octets* — not their hexadecimal spelling — are the secret.
#[must_use]
pub fn basic_auth_header(identity: &Identity, password: &BasicAuthPassword) -> String {
    use base64::Engine as _;
    let mut credentials = Vec::new();
    credentials.extend_from_slice(identity.as_str().as_bytes());
    credentials.push(b':');
    credentials.extend_from_slice(password.as_bytes());
    let credentials = Zeroizing::new(credentials);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(&*credentials)
    )
}

/// The credentials a client presented, as seen by the CSMS.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Credentials {
    /// No `Authorization` header was sent.
    None,
    /// HTTP Basic credentials.
    Basic {
        /// The user name.
        ///
        /// A00.FR.204 requires it to equal the Charging Station identity from the URL, and
        /// A00.FR.207 makes checking that the CSMS's job — so [`Csms`](super::Csms) refuses a
        /// mismatch with 401 before an [`Authenticator`](super::Authenticator) sees it.
        user: String,
        /// The password octets. Zeroized when the credentials are dropped.
        password: Zeroizing<Vec<u8>>,
    },
    /// The peer authenticated with a TLS client certificate (profile 3).
    ClientCertificate {
        /// The DER-encoded end-entity certificate.
        der: Vec<u8>,
    },
}

impl Credentials {
    /// Parses an `Authorization` header value.
    #[must_use]
    pub fn from_authorization_header(value: &str) -> Self {
        use base64::Engine as _;
        let Some(encoded) = value
            .strip_prefix("Basic ")
            .or_else(|| value.strip_prefix("basic "))
        else {
            return Credentials::None;
        };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
            return Credentials::None;
        };
        // The user name cannot contain ':' (A00.FR.204), so the first colon splits it.
        match decoded.iter().position(|byte| *byte == b':') {
            Some(index) => {
                let (user, password) = decoded.split_at(index);
                match core::str::from_utf8(user) {
                    Ok(user) => Credentials::Basic {
                        user: user.to_owned(),
                        password: Zeroizing::new(password[1..].to_vec()),
                    },
                    Err(_) => Credentials::None,
                }
            }
            None => Credentials::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_16_authorization_key_is_hex_and_a_2x_password_is_utf8() {
        // The 1.6 whitepaper's AuthorizationKey is hex; the octets are the password.
        let legacy = BasicAuthPassword::hex("0001020304").unwrap();
        assert_eq!(legacy.as_bytes(), &[0, 1, 2, 3, 4]);
        assert_eq!(
            BasicAuthPassword::hex("xyz").unwrap_err(),
            CredentialError::NotHex
        );

        // 2.x sends the text itself, and constrains its length (A00.FR.205).
        let modern = BasicAuthPassword::utf8("0123456789abcdef").unwrap();
        assert_eq!(modern.as_bytes(), b"0123456789abcdef");
        assert_eq!(
            BasicAuthPassword::utf8("short").unwrap_err(),
            CredentialError::PasswordLength(5)
        );
        // 41 characters is legal: A00.FR.205's ceiling is the variable's maxLimit, which is
        // "at least 40 and at most 64".
        assert!(BasicAuthPassword::utf8("x".repeat(41)).is_ok());
        assert!(BasicAuthPassword::utf8("x".repeat(64)).is_ok());
        assert!(BasicAuthPassword::utf8("x".repeat(65)).is_err());
    }

    #[test]
    fn basic_auth_round_trips_through_the_header() {
        let identity = Identity::new("CS-0001").unwrap();
        let password = BasicAuthPassword::utf8("0123456789abcdef").unwrap();
        let header = basic_auth_header(&identity, &password);
        let parsed = Credentials::from_authorization_header(&header);
        let Credentials::Basic {
            user,
            password: bytes,
        } = parsed
        else {
            panic!("{parsed:?}")
        };
        assert_eq!(user, "CS-0001");
        assert!(password.verify(&bytes));
        assert!(!password.verify(b"0123456789abcdee"));
    }

    #[test]
    fn profiles_report_what_they_secure() {
        assert!(!SecurityProfile::BasicAuth.is_transport_encrypted());
        assert!(SecurityProfile::TlsBasicAuth.is_transport_encrypted());
        assert!(!SecurityProfile::TlsClientCertificate.uses_basic_auth());
        assert_eq!(
            SecurityProfile::from_number(3),
            Some(SecurityProfile::TlsClientCertificate)
        );
        assert_eq!(SecurityProfile::from_number(4), None);
    }
}
