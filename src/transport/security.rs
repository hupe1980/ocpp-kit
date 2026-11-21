//! Security profiles 0–3 and the credential rules that go with them.
//!
//! OCPP defines three profiles (2.x Part 2 §A 1.3 Table 12; 1.6 Security Whitepaper ed. 2),
//! and plain OCPP-J 1.6 — without the Security Whitepaper — has a fourth, with no
//! authentication at all:
//!
//! | Profile | Transport | Charging Station authenticates with | CSMS authenticates with |
//! |---|---|---|---|
//! | 0 | plain WebSocket | – (OCPP 1.6 only) | – |
//! | 1 | plain WebSocket | HTTP Basic | – |
//! | 2 | TLS | HTTP Basic | server certificate |
//! | 3 | TLS | client certificate | server certificate |
//!
//! Profile 1 is only acceptable on a network that is already trusted end to end; the
//! specification says so, and so does [`SecurityProfile::is_transport_encrypted`].

use core::fmt;

use zeroize::Zeroizing;

use crate::credentials::{self, PASSWORD_STRING_CHARSET};
pub use crate::credentials::{
    AUTHORIZATION_KEY_MAX_BYTES, AUTHORIZATION_KEY_MIN_BYTES, BASIC_AUTH_INTEROPERABLE_MAX_LEN,
    BASIC_AUTH_MAX_LEN, BASIC_AUTH_MIN_LEN, CredentialError,
};
use crate::types::Identity;
use crate::version::Version;

/// Which security profile a connection uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SecurityProfile {
    /// 0 — unsecured transport, **no authentication**: plain OCPP-J 1.6 without the Security
    /// Whitepaper. Any peer that knows a station's identity can be that station.
    ///
    /// OCPP 2.x has no such profile — "the Charging Station SHALL authenticate itself to the
    /// CSMS using HTTP Basic authentication" (A00.FR.202) is the floor — so a [`Station`]
    /// offering any 2.x version refuses to build with it. It is never a default: a station
    /// reaches it only by naming it.
    ///
    /// [`Station`]: super::Station
    Unsecured,
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
            SecurityProfile::Unsecured => 0,
            SecurityProfile::BasicAuth => 1,
            SecurityProfile::TlsBasicAuth => 2,
            SecurityProfile::TlsClientCertificate => 3,
        }
    }

    /// Maps a `SecurityCtrlr.SecurityProfile` value.
    #[must_use]
    pub const fn from_number(number: u8) -> Option<Self> {
        Some(match number {
            0 => SecurityProfile::Unsecured,
            1 => SecurityProfile::BasicAuth,
            2 => SecurityProfile::TlsBasicAuth,
            3 => SecurityProfile::TlsClientCertificate,
            _ => return None,
        })
    }

    /// Whether the transport is TLS.
    #[must_use]
    pub const fn is_transport_encrypted(self) -> bool {
        !matches!(
            self,
            SecurityProfile::Unsecured | SecurityProfile::BasicAuth
        )
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

impl BasicAuthPassword {
    /// A 2.x `BasicAuthPassword`: a `passwordString` of 16 to [`BASIC_AUTH_MAX_LEN`]
    /// characters, sent as UTF-8 (A00.FR.205).
    ///
    /// Refuses a character outside [`PASSWORD_STRING_CHARSET`](crate::credentials::PASSWORD_STRING_CHARSET)
    /// and names it:
    ///
    /// ```
    /// use ocpp_kit::transport::{BasicAuthPassword, CredentialError};
    /// assert!(BasicAuthPassword::utf8("a-sixteen-plus-character-secret").is_ok());
    /// assert_eq!(
    ///     BasicAuthPassword::utf8("not a password string").unwrap_err(),
    ///     CredentialError::Character(' ')
    /// );
    /// ```
    pub fn utf8(password: impl Into<String>) -> Result<Self, CredentialError> {
        let password = password.into();
        credentials::check_password(&password, BASIC_AUTH_MAX_LEN)?;
        Ok(Self {
            secret: Zeroizing::new(password.into_bytes()),
        })
    }

    /// A 1.6 `AuthorizationKey`, given as the hexadecimal string the specification uses.
    ///
    /// The decoded octets are what goes on the wire, not the hex text. The 1.6 Security
    /// Whitepaper (edition 4, A00.FR.205) requires "minimal 16-bytes long" and "20 bytes
    /// maximum" — 32 to 40 hexadecimal digits.
    pub fn hex(key: &str) -> Result<Self, CredentialError> {
        Ok(Self {
            secret: Zeroizing::new(credentials::authorization_key_octets(key)?),
        })
    }

    /// Generates a fresh password for `version` from the operating system's entropy source.
    ///
    /// Both versions require a *randomly chosen* secret: 1.6 gets 20 random octets, the
    /// longest `AuthorizationKey` ("strongly advised to be randomly generated binary",
    /// Whitepaper A00.FR.205); 2.x gets 40 random `passwordString` characters, the longest
    /// password every CSMS's `maxLimit` admits (A00.FR.205). Characters are drawn by rejection
    /// sampling, so each of the 71 is equally likely.
    ///
    /// Send [`configuration_value`](Self::configuration_value) to the station
    /// (`ChangeConfiguration(AuthorizationKey)` or `SetVariables(SecurityCtrlr.BasicAuthPassword)`),
    /// keep the password to [`verify`](Self::verify) its next connection.
    ///
    /// ```
    /// use ocpp_kit::{transport::BasicAuthPassword, version::Version};
    /// let password = BasicAuthPassword::generate(Version::V2_0_1)?;
    /// let value = password.configuration_value(Version::V2_0_1)?;
    /// assert_eq!(value.len(), 40);
    /// assert!(password.verify(value.as_bytes()));
    /// # Ok::<(), ocpp_kit::transport::CredentialError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`CredentialError::Entropy`] when the entropy source fails; there is no weaker fallback.
    pub fn generate(version: Version) -> Result<Self, CredentialError> {
        // 213 = 3 × 71: a byte below it maps onto the charset without bias; the rest are
        // drawn again.
        const ACCEPT_BELOW: u8 = 213;
        if version == Version::V1_6 {
            let mut octets = Zeroizing::new(vec![0u8; AUTHORIZATION_KEY_MAX_BYTES]);
            getrandom::fill(&mut octets).map_err(|_| CredentialError::Entropy)?;
            return Ok(Self { secret: octets });
        }
        let mut secret = Zeroizing::new(Vec::with_capacity(BASIC_AUTH_INTEROPERABLE_MAX_LEN));
        let mut pool = Zeroizing::new([0u8; 64]);
        while secret.len() < BASIC_AUTH_INTEROPERABLE_MAX_LEN {
            getrandom::fill(&mut *pool).map_err(|_| CredentialError::Entropy)?;
            for &byte in pool.iter().filter(|b| **b < ACCEPT_BELOW) {
                if secret.len() == BASIC_AUTH_INTEROPERABLE_MAX_LEN {
                    break;
                }
                secret.push(PASSWORD_STRING_CHARSET[usize::from(byte % 71)]);
            }
        }
        Ok(Self { secret })
    }

    /// The text to configure on the station for `version` — the inverse of
    /// [`for_version`](Self::for_version).
    ///
    /// 1.6: the `AuthorizationKey`, lowercase hexadecimal of the octets. That is also how a key
    /// is derived from a passphrase: `BasicAuthPassword::raw(passphrase).configuration_value(V1_6)`.
    /// 2.x: the `BasicAuthPassword` text itself.
    ///
    /// ```
    /// use ocpp_kit::{transport::BasicAuthPassword, version::Version};
    /// let key = BasicAuthPassword::raw("sixteen-byte-key").configuration_value(Version::V1_6)?;
    /// assert_eq!(&*key, "7369787465656e2d627974652d6b6579");
    /// # Ok::<(), ocpp_kit::transport::CredentialError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// The rule of `version` the password breaks: 1.6 needs 16–20 octets
    /// ([`CredentialError::KeyLength`]); 2.x needs a valid `passwordString` of 16–64
    /// characters.
    pub fn configuration_value(
        &self,
        version: Version,
    ) -> Result<ConfigurationValue, CredentialError> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        if version == Version::V1_6 {
            let len = self.secret.len();
            if !(AUTHORIZATION_KEY_MIN_BYTES..=AUTHORIZATION_KEY_MAX_BYTES).contains(&len) {
                return Err(CredentialError::KeyLength(len));
            }
            let mut text = Zeroizing::new(String::with_capacity(len * 2));
            for byte in self.secret.iter() {
                text.push(char::from(DIGITS[usize::from(byte >> 4)]));
                text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
            }
            return Ok(ConfigurationValue(text));
        }
        // Not UTF-8 means not a passwordString; name the first byte that is not.
        let text = core::str::from_utf8(&self.secret).map_err(|error| {
            CredentialError::Character(char::from(self.secret[error.valid_up_to()]))
        })?;
        credentials::check_password(text, BASIC_AUTH_MAX_LEN)?;
        Ok(ConfigurationValue(Zeroizing::new(text.to_owned())))
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

/// The text a station is configured with — a 1.6 `AuthorizationKey` or a 2.x
/// `BasicAuthPassword` — as [`BasicAuthPassword::configuration_value`] renders it.
///
/// A secret: zeroized on drop, and `Debug` prints `<redacted>`. Read it as a `&str` where it is
/// sent (`&*value`, [`as_str`](Self::as_str)).
pub struct ConfigurationValue(Zeroizing<String>);

impl ConfigurationValue {
    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::ops::Deref for ConfigurationValue {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ConfigurationValue {
    /// Never prints the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationValue(<redacted>)")
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
        let legacy = BasicAuthPassword::hex("000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(legacy.as_bytes(), &(0..16).collect::<Vec<u8>>()[..]);
        assert_eq!(
            BasicAuthPassword::hex("xyz").unwrap_err(),
            CredentialError::NotHex
        );
        // Whitepaper A00.FR.205: 16 to 20 octets.
        assert_eq!(
            BasicAuthPassword::hex("0001020304").unwrap_err(),
            CredentialError::KeyLength(5)
        );
        assert_eq!(
            BasicAuthPassword::hex(&"ab".repeat(21)).unwrap_err(),
            CredentialError::KeyLength(21)
        );
        assert!(BasicAuthPassword::hex(&"ab".repeat(20)).is_ok());

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
    fn a_generated_password_round_trips_through_its_configuration_value() {
        for version in [Version::V1_6, Version::V2_0_1, Version::V2_1] {
            for _ in 0..1_000 {
                let password = BasicAuthPassword::generate(version).unwrap();
                let value = password.configuration_value(version).unwrap();
                let back = BasicAuthPassword::for_version(version, &value).unwrap();
                assert!(
                    back.verify(password.as_bytes()),
                    "{version}: {}",
                    value.as_str()
                );
            }
        }
        let legacy = BasicAuthPassword::generate(Version::V1_6).unwrap();
        assert_eq!(legacy.as_bytes().len(), AUTHORIZATION_KEY_MAX_BYTES);
        let key = legacy.configuration_value(Version::V1_6).unwrap();
        assert_eq!(key.len(), 40);
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let modern = BasicAuthPassword::generate(Version::V2_1).unwrap();
        assert_eq!(modern.as_bytes().len(), BASIC_AUTH_INTEROPERABLE_MAX_LEN);
    }

    #[test]
    fn generated_characters_are_uniform_over_the_charset() {
        // 10 000 passwords × 40 characters; each of 71 characters expected 5 633.8 times.
        let mut counts = [0u32; 256];
        for _ in 0..10_000 {
            for &byte in BasicAuthPassword::generate(Version::V2_0_1)
                .unwrap()
                .as_bytes()
            {
                counts[usize::from(byte)] += 1;
            }
        }
        let expected = 400_000.0 / 71.0;
        let mut chi_square = 0.0;
        for &c in PASSWORD_STRING_CHARSET {
            let observed = f64::from(counts[usize::from(c)]);
            assert!(observed > 0.0, "{} never generated", char::from(c));
            chi_square += (observed - expected).powi(2) / expected;
        }
        let outside: u32 = (0u8..=255)
            .filter(|b| !PASSWORD_STRING_CHARSET.contains(b))
            .map(|b| counts[usize::from(b)])
            .sum();
        assert_eq!(outside, 0);
        // 70 degrees of freedom; p = 0.001 is at ~112. A modulo bias over 256 would sit near 4 000.
        assert!(chi_square < 120.0, "chi-square {chi_square}");
    }

    #[test]
    fn a_configuration_value_follows_the_version_rules() {
        let passphrase = BasicAuthPassword::raw("sixteen-byte-key");
        assert_eq!(
            &*passphrase.configuration_value(Version::V1_6).unwrap(),
            "7369787465656e2d627974652d6b6579"
        );
        assert_eq!(
            BasicAuthPassword::raw("fifteen-byte-ky")
                .configuration_value(Version::V1_6)
                .unwrap_err(),
            CredentialError::KeyLength(15)
        );
        assert_eq!(
            BasicAuthPassword::raw([0u8; 21].to_vec())
                .configuration_value(Version::V1_6)
                .unwrap_err(),
            CredentialError::KeyLength(21)
        );
        // 2.x: the text, and only if it is a valid passwordString.
        let modern = BasicAuthPassword::utf8("0123456789abcdef").unwrap();
        assert_eq!(
            &*modern.configuration_value(Version::V2_0_1).unwrap(),
            "0123456789abcdef"
        );
        assert_eq!(
            BasicAuthPassword::raw([0xff; 16].to_vec())
                .configuration_value(Version::V2_0_1)
                .unwrap_err(),
            CredentialError::Character('\u{ff}')
        );
        // A secret is never printed — neither the password nor what is configured from it.
        assert_eq!(format!("{modern:?}"), "BasicAuthPassword(<redacted>)");
        let value = BasicAuthPassword::generate(Version::V2_0_1)
            .unwrap()
            .configuration_value(Version::V2_0_1)
            .unwrap();
        assert_eq!(format!("{value:?}"), "ConfigurationValue(<redacted>)");
        assert!(!format!("{value:?}").contains(value.as_str()));
    }

    #[test]
    fn a_2x_password_outside_the_charset_is_refused_by_name() {
        assert_eq!(
            BasicAuthPassword::utf8("0123456789abcde!").unwrap_err(),
            CredentialError::Character('!')
        );
        assert_eq!(
            BasicAuthPassword::for_version(Version::V2_1, "0123456789 abcdef").unwrap_err(),
            CredentialError::Character(' ')
        );
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
