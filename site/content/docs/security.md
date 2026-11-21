+++
title = "Security"
description = "Security profiles 0 (1.6) to 3, version-correct Basic authentication, TLS 1.2+ with rustls, 401-versus-404 handshake outcomes, and JWS signed messages per OCPP Part 4 chapter 7."
weight = 110
+++

## The profiles

OCPP defines three (2.x Part 2 §A 1.3 Table 12; 1.6 Security Whitepaper ed. 2), and plain
OCPP-J 1.6 — without the Security Whitepaper — has a fourth with no authentication at all:

| Profile | Transport | Station authenticates with | CSMS authenticates with |
|---|---|---|---|
| 0 | plain WebSocket | – (**1.6 only**) | – |
| 1 | plain WebSocket | HTTP Basic | – |
| 2 | TLS | HTTP Basic | server certificate |
| 3 | TLS | client certificate | server certificate |

```rust,no_run
use ocpp_kit::transport::{BasicAuthPassword, ClientTls, SecurityProfile, Station};

# fn build() -> Result<(), Box<dyn std::error::Error>> {
// Profile 2
let profile2 = Station::builder()
    .identity("CS-0001")?
    .url("wss://csms.example.com/ocpp")
    .security_profile(SecurityProfile::TlsBasicAuth)
    .password(BasicAuthPassword::utf8("a-sixteen-plus-character-secret")?)
    .tls(ClientTls::with_root_file("csms-root.pem")?)
    .build()?;

// Profile 3
let profile3 = Station::builder()
    .identity("CS-0001")?
    .url("wss://csms.example.com/ocpp")
    .security_profile(SecurityProfile::TlsClientCertificate)
    .tls(
        ClientTls::builder()
            .root_file("csms-root.pem")?
            .client_certificate("station-chain.pem", "station-key.pem")?
            .build()?,
    )
    .build()?;
# let _ = (profile2, profile3);
# Ok(()) }
```

The builder refuses contradictions: a TLS profile with a `ws://` URL, profile 1 or 2 without a
password, a TLS profile without a TLS configuration. Getting a security profile wrong should be
a build error, not a production incident.

`ClientTls` refuses a CSMS certificate whose name is a wildcard (`*.example.com`), after the
ordinary chain and host-name checks pass: "OCPP-compliant behavior is that a Charging Station
rejects a connection from a CSMS that presents a wildcard certificate" (2.x
`AllowCSMSTLSWildcards`, 1.6 `AllowCentralSystemTLSWildcards`). Where that variable is `true`,
`ClientTls::builder().allow_wildcard_server_certificates()` says so.

Profile 0 exists because a great many 1.6 fleets and back ends run without the Security
Whitepaper. Any peer that knows a station's identity can be that station, so it is never a
default: a station reaches it only as `SecurityProfile::Unsecured`, and the builder refuses it
when the station offers any 2.x version — "the Charging Station SHALL authenticate itself to
the CSMS using HTTP Basic authentication" (A00.FR.202) is the 2.x floor. A CSMS reports a plain
connection that sent no credentials as profile 0 in `Auth::profile`, so an authenticator can
refuse it in one comparison.

## Passwords

The versions encode the Basic-auth password differently, and it is a routine source of "the
credentials are right but it will not connect":

* **1.6** `AuthorizationKey` is a *hexadecimal string*. The octets it decodes to are the
  password.
* **2.x** `BasicAuthPassword` is sent as UTF-8 — never hex- or base64-encoded — and is a
  `passwordString` of at least 16 characters (A00.FR.205): only `a-z A-Z 0-9 * - _ = : + | @ .`
  (Part 2 §2.1.4). The specification names no single maximum: the ceiling is
  the `maxLimit` of the `BasicAuthPassword` variable, "which must be at least 40 characters
  and at most 64". `BASIC_AUTH_MAX_LEN` is therefore 64, and
  `BASIC_AUTH_INTEROPERABLE_MAX_LEN` is the 40 a station should stay under if it has to work
  with any CSMS.

```rust
use ocpp_kit::transport::BasicAuthPassword;

// 1.6: 16–20 octets, written as 32–40 hex digits.
let legacy = BasicAuthPassword::hex("000102030405060708090a0b0c0d0e0f").unwrap();
assert_eq!(legacy.as_bytes().len(), 16);

let modern = BasicAuthPassword::utf8("0123456789abcdef").unwrap();
assert_eq!(modern.as_bytes(), b"0123456789abcdef");

// Outside passwordString: refused, naming the character.
assert_eq!(
    BasicAuthPassword::utf8("0123456789abcde!").unwrap_err(),
    ocpp_kit::transport::CredentialError::Character('!'),
);
```

`BasicAuthPassword::for_version` picks the right one. The value is zeroized on drop, its
`Debug` prints `<redacted>`, and `verify` compares in constant time.

### Provisioning a station's password

Both versions require a *randomly chosen* secret. `generate` draws one from the operating
system — 20 random octets for 1.6 (the longest `AuthorizationKey`), 40 random `passwordString`
characters for 2.x (the longest every CSMS's `maxLimit` admits), each character equally likely —
and `configuration_value` is what you send the station, the inverse of `for_version`:

```rust
use ocpp_kit::transport::BasicAuthPassword;
use ocpp_kit::Version;

# fn provision() -> Result<(), ocpp_kit::transport::CredentialError> {
let version = Version::V1_6;
let password = BasicAuthPassword::generate(version)?;
// ChangeConfiguration(AuthorizationKey, …) on 1.6, SetVariables(SecurityCtrlr.BasicAuthPassword, …)
// on 2.x. Zeroized when dropped.
let value = password.configuration_value(version)?;
assert_eq!(value.len(), 40); // 20 octets as hex
// Store the password; verify the station's next connection against it.
assert!(password.verify(BasicAuthPassword::for_version(version, &value)?.as_bytes()));

// A 1.6 key configured from a passphrase is the hex of its octets.
let key = BasicAuthPassword::raw("sixteen-byte-key").configuration_value(Version::V1_6)?;
assert_eq!(&*key, "7369787465656e2d627974652d6b6579");
# Ok(()) }
```

On the station side, `ConfigurationKeys` rejects an `AuthorizationKey` that is not 16–20 octets of
hex and `DeviceModel` a `BasicAuthPassword` that is not a `passwordString` within its `maxLimit`
(40 by default). Both keep the old credential (1.6 Whitepaper A01), so a bad write cannot lock the
station out. The checks themselves are `ocpp_kit::credentials`, available without the transport.

The Charging Station identity may not contain `:` (A00.FR.204), because HTTP Basic could not
represent it unambiguously. `Identity::new` rejects it, so it cannot reach the wire.

### The username has to *be* the identity, and the CSMS checks

A00.FR.204 says the Basic username is the Charging Station identity from the URL, and
A00.FR.207 makes validating it the CSMS's job. A mismatch is refused with **401** before your
`Authenticator` is consulted.

Not left to the application: the natural way to write an authenticator is to look the password
up by the username HTTP Basic handed you, while the session is filed under the identity from
the *path*. A peer could then connect as `/ocpp/CS-VICTIM` with
`Authorization: Basic CS-ATTACKER:…`, authenticate as itself, and be filed as the victim.

## TLS

`rustls` negotiates TLS 1.2 and 1.3 only and offers no anonymous, null or export cipher suites
— what A00.FR.313 / A00.FR.416 and A00.FR.320 / A00.FR.423 require. Nothing in this crate can
configure it back below that line. The crypto provider is chosen explicitly rather than taken
from the process-wide default, so a host application that installs a different one cannot
silently change the cipher suites.

For profile 3 the CSMS receives the verified end-entity certificate as
`Credentials::ClientCertificate`, so it can bind the certificate to the identity in the URL —
without that check, any certificate your roots trust could impersonate any station. That one
*is* your `Authenticator`'s job: only you know how your certificates carry an identity.

### Profile 2 and 3 on one listener

| Constructor | Client certificate | `Auth::profile` |
|---|---|---|
| `ServerTls::new(chain, key)` | never asked for | 2 |
| `ServerTls::with_client_auth(chain, key, roots)` | required | 3 |
| `ServerTls::with_optional_client_auth(chain, key, roots)` | verified if presented | 3 with one, 2 without |

A presented certificate must still verify (A00.FR.407); only its absence is tolerated. A station
has one profile (A00.FR.001), so on a mixed listener a profile-3 station connecting without its
certificate is a downgrade the authenticator must refuse (A00.FR.002):

```rust,no_run
use ocpp_kit::transport::{Auth, AuthOutcome, Csms, SecurityProfile, ServerTls};

# fn build() -> Result<(), Box<dyn std::error::Error>> {
let csms = Csms::builder()
    .bind("0.0.0.0:443".parse()?)
    .tls(ServerTls::with_optional_client_auth("csms-chain.pem", "csms-key.pem", "station-roots.pem")?)
    .authenticate(|auth: Auth| async move {
        let Some(provisioned) = provisioned_profile(auth.identity.as_str()).await else {
            return AuthOutcome::Unknown;
        };
        if auth.profile != provisioned {
            return AuthOutcome::Reject; // A00.FR.002
        }
        check_credentials(&auth).await
    })
    .build()?;
# Ok(()) }
# async fn provisioned_profile(_: &str) -> Option<SecurityProfile> { None }
# async fn check_credentials(_: &Auth) -> AuthOutcome { AuthOutcome::accept() }
```

## Unsafe options are never the default

Where an option is unsafe *and* leaves no evidence of itself, reaching it takes a sentence:

| | Unsafe option | How you reach it |
|---|---|---|
| Signature verification | accept an unsigned frame | `SignaturePolicy::Optional`, named at the call site |
| CSMS authentication | accept every station | `authenticate(AcceptEveryStation)`, or `build()` fails |
| Client certificates | let a station without one through | `ServerTls::with_optional_client_auth`, which documents the A00.FR.002 check it hands you |

A CSMS that authenticates nobody looks, in its own logs, exactly like one that authenticates
everybody successfully — so `Csms::builder().bind(addr).build()` is an error, not a permissive
server.

## Handshake outcomes

Part 4 §3.2 (§3.3 in 2.1) and HTTP Basic distinguish them, and so does the CSMS here:

| Outcome | HTTP | Meaning |
|---|---|---|
| `AuthOutcome::Accept(admission)` | 101 | in you go — with what the authenticator resolved and, optionally, the version the station must use; see [`Admission`](@/docs/transport.md#what-the-authenticator-resolved-rides-the-session) |
| `AuthOutcome::Reject` | **401** with `WWW-Authenticate` | the credentials are wrong |
| `AuthOutcome::Unknown` | **404** | there is no such Charging Station |
| no common subprotocol, or the pinned one not offered | 101 **without** the header, then an immediate close | we agree on nothing |

The 401/404 distinction is what lets an operator tell a typo from a wrong password without
reading server logs. The operator learns more than the peer: every refusal is a
`SessionEvent::Refused` with a typed [`Refusal`](@/docs/transport.md#why-a-connection-was-refused)
— a username mismatch and a wrong password are both 401 on the wire, and two different reasons
in the event.

## The Local Controller's certificates

Part 4 §6.5 gives a [Local Controller](@/docs/transport.md#local-controller) two TLS roles at
once: it is a TLS *server* to the stations, presenting a "CSMS" certificate of its own, and a
TLS *client* to the CSMS, presenting a "Charging Station" certificate of its own. Both belong
to the controller, and it never stores the peers' certificates. `server_tls` and `client_tls`
on the builder are those two roles.

## Signed messages

TLS proves who you are talking to *right now*. With a Local Controller in the path that is the
controller, not the CSMS — the controller terminates the TLS connection. Part 4 chapter 7 adds
a message-level signature so each end can prove what the *other end* sent, and §7.4 is explicit
that the key mismatch a Local Controller introduces is expected and must not invalidate a
signature.

```rust
use ocpp_kit::RawValue;
use ocpp_kit::rpc::Frame;
use ocpp_kit::rpc::signed::{
    Es256Signer, SignaturePolicy, sign_frame, unsigned_action, verify_frame,
};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let private_key = [7u8; 32];
# let certificate_der = b"a-der-certificate";
let signer = Es256Signer::from_bytes(&private_key)?.with_certificate(certificate_der);

let frame = Frame::Call {
    id: "19223201".parse()?,
    action: "BootNotification".into(),
    payload: std::borrow::Cow::Owned(RawValue::from_string(r#"{"reason":"PowerUp"}"#.into())?),
};

// [2, "1", "BootNotification", {…}]  ->  [2, "1", "BootNotification-Signed", {jws}]
let signed = sign_frame(&frame, &signer, None)?;
assert_eq!(unsigned_action(signed.action().unwrap()), Some("BootNotification"));

// …and back, checking the signature on the way. `Required` is what makes the check mean
// something: see below.
let original = verify_frame(&signed, &signer.verifier(), None, SignaturePolicy::Required)?;
assert_eq!(original, frame);

// An intermediary that strips the signature is refused rather than believed.
assert!(verify_frame(&frame, &signer.verifier(), None, SignaturePolicy::Required).is_err());
# Ok(()) }
```

### Require, do not merely verify

`verify_frame` takes a `SignaturePolicy` and has no default. A verifier can only check a
signature that is *present*, so "verify it if it is signed" accepts a frame whose signature an
intermediary deleted — three JSON members and a `-Signed` suffix — which is the downgrade
chapter 7 exists to prevent. `Optional` is for a fleet mid-migration; `Required` is the one
that buys anything.

The protected header carries `OCPPAction`, `OCPPMessageTypedId` (spelled exactly as the
specification does) and `x5t#S256`, the SHA-256 hash of the DER signing certificate. The action
and message-type fields are checked against the frame that carried them, so a signature cannot
be lifted from one message and pasted onto another. `alg: none` is refused — it is not one of
the three algorithms §7.3 permits — and so is a header that marks an extension `crit` (RFC 7515
§4.1.11): OCPP defines no critical header parameters, so any entry is a constraint the signer
imposed that this implementation cannot honour.

[`Signer`] and [`Verifier`] are traits, because §7.4 anticipates keys this crate could not
reach: "a certificate stored in the calibrated measuring chip". A station with a secure element
implements them against it and never lets the private key into RAM. The `jws-es256` feature
adds a software implementation for the common case.

[`Signer`]: https://docs.rs/ocpp-kit/latest/ocpp_kit/rpc/signed/trait.Signer.html
[`Verifier`]: https://docs.rs/ocpp-kit/latest/ocpp_kit/rpc/signed/trait.Verifier.html

## What this crate does not do

* It does not implement ISO 15118 (EXI, SDP, the TLS session between the EV and the EVSE). It
  carries 15118's messages opaquely, as the specification does.
* It does not manage a certificate store. `InstallCertificate`, `GetInstalledCertificateIds`
  and `DeleteCertificate` are typed and dispatched; deciding what to trust is your policy.
