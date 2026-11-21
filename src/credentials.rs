//! The rules a Charging Station's HTTP Basic password must follow, per version.
//!
//! Pure checks with no dependencies, so the station blocks can refuse a credential they could
//! not present without pulling in the transport. The password type itself, which zeroizes and
//! generates, is [`transport::BasicAuthPassword`](crate::transport) (feature `tokio`).
//!
//! * **1.6** `AuthorizationKey`: "Minimal 16-bytes long … Hexadecimal represented (20 bytes
//!   maximum, represented as a string of up to 40 hexadecimal digits)" (1.6 Security
//!   Whitepaper ed. 4, A00.FR.205). The decoded octets are the password.
//! * **2.x** `BasicAuthPassword`: "a randomly chosen passwordString … consisting of minimum 16
//!   and a maximum as defined by the maxLimit of configuration variable `BasicAuthPassword`,
//!   which must be at least 40 characters and at most 64" (A00.FR.205), where `passwordString`
//!   "can only contain characters from the following character set: a-z, A-Z, 0-9, '*', '-',
//!   '_', '=', ':', '+', '|', '@', '.'" (Part 2 §2.1.4, primitive datatypes).
//!
//! ```
//! use ocpp_kit::credentials::{self, CredentialError};
//!
//! assert!(credentials::check_password("a-sixteen-char-pw", 40).is_ok());
//! assert_eq!(
//!     credentials::check_password("sixteen-chars-ok!", 40),
//!     Err(CredentialError::Character('!'))
//! );
//! assert_eq!(credentials::authorization_key_octets("00112233445566778899AABBCCDDEEFF")?.len(), 16);
//! # Ok::<(), CredentialError>(())
//! ```

use alloc::vec::Vec;
use core::fmt;

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
/// A Charging Station that must interoperate with any CSMS should stay at or below this; it is
/// also the length of a generated 2.x password.
pub const BASIC_AUTH_INTEROPERABLE_MAX_LEN: usize = 40;

/// The shortest 1.6 `AuthorizationKey`, in octets (Security Whitepaper A00.FR.205).
pub const AUTHORIZATION_KEY_MIN_BYTES: usize = 16;

/// The longest 1.6 `AuthorizationKey`, in octets (Security Whitepaper A00.FR.205).
pub const AUTHORIZATION_KEY_MAX_BYTES: usize = 20;

/// Every character a 2.x `passwordString` may contain (Part 2 §2.1.4), 71 in all.
pub const PASSWORD_STRING_CHARSET: &[u8; 71] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789*-_=:+|@.";

/// Why a credential was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialError {
    /// The password is shorter than [`BASIC_AUTH_MIN_LEN`] or longer than the maximum in force
    /// (A00.FR.205).
    PasswordLength(usize),
    /// The password contains a character outside [`PASSWORD_STRING_CHARSET`] — the first one.
    Character(char),
    /// The 1.6 `AuthorizationKey` is not a hexadecimal string.
    NotHex,
    /// The 1.6 `AuthorizationKey` decodes to fewer than [`AUTHORIZATION_KEY_MIN_BYTES`] or
    /// more than [`AUTHORIZATION_KEY_MAX_BYTES`] octets (1.6 Security Whitepaper A00.FR.205).
    KeyLength(usize),
    /// The operating system's entropy source failed, so no password was generated.
    Entropy,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::PasswordLength(len) => write!(
                f,
                "BasicAuthPassword is {len} characters; OCPP 2.x allows {BASIC_AUTH_MIN_LEN} to the \
                 variable's maxLimit, at most {BASIC_AUTH_MAX_LEN} (A00.FR.205)"
            ),
            CredentialError::Character(c) => write!(
                f,
                "BasicAuthPassword contains {c:?}; a passwordString allows only a-z, A-Z, 0-9 and * - _ = : + | @ ."
            ),
            CredentialError::NotHex => {
                f.write_str("the 1.6 AuthorizationKey must be an even-length hexadecimal string")
            }
            CredentialError::KeyLength(len) => write!(
                f,
                "the 1.6 AuthorizationKey is {len} bytes; it must be \
                 {AUTHORIZATION_KEY_MIN_BYTES}–{AUTHORIZATION_KEY_MAX_BYTES} (A00.FR.205)"
            ),
            CredentialError::Entropy => f.write_str("the operating system's entropy source failed"),
        }
    }
}

impl core::error::Error for CredentialError {}

/// Checks a 2.x `BasicAuthPassword`: [`BASIC_AUTH_MIN_LEN`]`..=max_len` characters, every one a
/// `passwordString` character.
///
/// `max_len` is the `maxLimit` in force — the variable's, on a station; [`BASIC_AUTH_MAX_LEN`]
/// when the caller does not know it.
pub fn check_password(text: &str, max_len: usize) -> Result<(), CredentialError> {
    if let Some(c) = text
        .chars()
        .find(|c| !c.is_ascii() || !PASSWORD_STRING_CHARSET.contains(&(*c as u8)))
    {
        return Err(CredentialError::Character(c));
    }
    // ASCII only from here, so bytes are characters.
    if !(BASIC_AUTH_MIN_LEN..=max_len).contains(&text.len()) {
        return Err(CredentialError::PasswordLength(text.len()));
    }
    Ok(())
}

/// Decodes a 1.6 `AuthorizationKey` into the password octets: hexadecimal (either case,
/// surrounding whitespace ignored), [`AUTHORIZATION_KEY_MIN_BYTES`] to
/// [`AUTHORIZATION_KEY_MAX_BYTES`] octets.
pub fn authorization_key_octets(hex: &str) -> Result<Vec<u8>, CredentialError> {
    let text = hex.trim().as_bytes();
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return Err(CredentialError::NotHex);
    }
    let bytes = text
        .chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect::<Option<Vec<u8>>>()
        .ok_or(CredentialError::NotHex)?;
    if !(AUTHORIZATION_KEY_MIN_BYTES..=AUTHORIZATION_KEY_MAX_BYTES).contains(&bytes.len()) {
        return Err(CredentialError::KeyLength(bytes.len()));
    }
    Ok(bytes)
}

const fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_charset_is_the_one_part_2_lists() {
        assert_eq!(PASSWORD_STRING_CHARSET.len(), 71);
        let mut sorted = PASSWORD_STRING_CHARSET.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 71, "no duplicates");
        for c in b"*-_=:+|@." {
            assert!(PASSWORD_STRING_CHARSET.contains(c));
        }
    }

    #[test]
    fn a_password_is_a_password_string_of_16_to_the_limit() {
        assert!(check_password(&"a".repeat(16), 40).is_ok());
        assert!(check_password(&"Z9*-_=:+|@.".repeat(4)[..40], 40).is_ok());
        assert_eq!(
            check_password(&"a".repeat(15), 40),
            Err(CredentialError::PasswordLength(15))
        );
        assert_eq!(
            check_password(&"a".repeat(41), 40),
            Err(CredentialError::PasswordLength(41))
        );
        assert!(check_password(&"a".repeat(64), BASIC_AUTH_MAX_LEN).is_ok());
        // The first offender is named, even before the length is checked.
        assert_eq!(
            check_password("short!", 40),
            Err(CredentialError::Character('!'))
        );
        assert_eq!(
            check_password("sixteen chars ok", 40),
            Err(CredentialError::Character(' '))
        );
        assert_eq!(
            check_password("sixteen-chars-ü-ok", 40),
            Err(CredentialError::Character('ü'))
        );
    }

    #[test]
    fn an_authorization_key_is_16_to_20_octets_of_hex() {
        assert_eq!(
            authorization_key_octets("000102030405060708090a0b0c0d0E0F").unwrap(),
            (0..16).collect::<Vec<u8>>()
        );
        assert_eq!(
            authorization_key_octets(" 00112233445566778899aabbccddeeff\n")
                .unwrap()
                .len(),
            16
        );
        assert_eq!(authorization_key_octets("zz"), Err(CredentialError::NotHex));
        assert_eq!(
            authorization_key_octets("abc"),
            Err(CredentialError::NotHex)
        );
        assert_eq!(authorization_key_octets(""), Err(CredentialError::NotHex));
        assert_eq!(
            authorization_key_octets(&"ab".repeat(15)),
            Err(CredentialError::KeyLength(15))
        );
        assert_eq!(
            authorization_key_octets(&"ab".repeat(21)),
            Err(CredentialError::KeyLength(21))
        );
        assert!(authorization_key_octets(&"ab".repeat(20)).is_ok());
        // Not hex even though it is ASCII and of even length.
        assert_eq!(
            authorization_key_octets(&"0g".repeat(16)),
            Err(CredentialError::NotHex)
        );
    }
}
