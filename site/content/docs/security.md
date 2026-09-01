+++
title = "Security"
description = "Security profiles 1-3, version-correct Basic authentication, TLS 1.2+ with rustls, 401-versus-404 handshake outcomes, and JWS signed messages per OCPP Part 4 chapter 7."
weight = 110
+++

## The three profiles

OCPP defines three (2.x Part 2 §A 1.3 Table 12; 1.6 Security Whitepaper ed. 2):

| Profile | Transport | Station authenticates with | CSMS authenticates with |
|---|---|---|---|
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

## Passwords

The versions encode the Basic-auth password differently, and it is a routine source of "the
credentials are right but it will not connect":

* **1.6** `AuthorizationKey` is a *hexadecimal string*. The octets it decodes to are the
  password.
* **2.x** `BasicAuthPassword` is sent as UTF-8 — never hex- or base64-encoded — and is at
  least 16 characters (A00.FR.205). The specification names no single maximum: the ceiling is
  the `maxLimit` of the `BasicAuthPassword` variable, "which must be at least 40 characters
  and at most 64". `BASIC_AUTH_MAX_LEN` is therefore 64, and
  `BASIC_AUTH_INTEROPERABLE_MAX_LEN` is the 40 a station should stay under if it has to work
  with any CSMS.

```rust
use ocpp_kit::transport::BasicAuthPassword;

let legacy = BasicAuthPassword::hex("0001020304").unwrap();
assert_eq!(legacy.as_bytes(), &[0, 1, 2, 3, 4]);

let modern = BasicAuthPassword::utf8("0123456789abcdef").unwrap();
assert_eq!(modern.as_bytes(), b"0123456789abcdef");
```

`BasicAuthPassword::for_version` picks the right one. The value is zeroized on drop, its
`Debug` prints `<redacted>`, and `verify` compares in constant time.

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

## Unsafe options are never the default

Where an option is unsafe *and* leaves no evidence of itself, reaching it takes a sentence:

| | Unsafe option | How you reach it |
|---|---|---|
| Signature verification | accept an unsigned frame | `SignaturePolicy::Optional`, named at the call site |
| CSMS authentication | accept every station | `authenticate(AcceptEveryStation)`, or `build()` fails |

A CSMS that authenticates nobody looks, in its own logs, exactly like one that authenticates
everybody successfully — so `Csms::builder().bind(addr).build()` is an error, not a permissive
server.

## Handshake outcomes

Part 4 §3.1.1 distinguishes them, and so does the CSMS here:

| Outcome | HTTP | Meaning |
|---|---|---|
| `AuthOutcome::Accept(_)` | 101 | in you go — the payload is what the authenticator resolved; see [`SessionContext`](@/docs/transport.md) |
| `AuthOutcome::Reject` | **401** with `WWW-Authenticate` | the credentials are wrong |
| `AuthOutcome::Unknown` | **404** | there is no such Charging Station |
| no common subprotocol | 101 **without** the header, then an immediate close | we speak nothing in common |

The 401/404 distinction is what lets an operator tell a typo from a wrong password without
reading server logs.

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
