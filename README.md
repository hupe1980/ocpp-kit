# ocpp-kit

[![crates.io](https://img.shields.io/crates/v/ocpp-kit.svg)](https://crates.io/crates/ocpp-kit)
[![docs.rs](https://docs.rs/ocpp-kit/badge.svg)](https://docs.rs/ocpp-kit)
[![CI](https://github.com/hupe1980/ocpp-kit/actions/workflows/ci.yml/badge.svg)](https://github.com/hupe1980/ocpp-kit/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

An [Open Charge Point Protocol](https://openchargealliance.org/) toolkit for Rust —
**OCPP 1.6J, 2.0.1 and 2.1** over JSON/WebSocket, for charging stations, CSMS backends and
local controllers.

📖 **[Documentation](https://hupe1980.github.io/ocpp-kit/docs/)** ·
[API reference](https://docs.rs/ocpp-kit)

```console
$ cargo add ocpp-kit --features tokio,rustls
```

## Five layers

| Layer | Module | What it gives you | `no_std` |
|---|---|---|---|
| **L0** | `v1_6` · `v2_0_1` · `v2_1` | Typed, validated payloads for all **39 / 64 / 91** actions, generated from the official OCA JSON schemas | ✅ `alloc` |
| **L1** | `rpc` | OCPP-J framing: `CALL`, `CALLRESULT`, `CALLERROR`, `CALLRESULTERROR`, `SEND`, with version-exact error codes, plus JWS signed messages | ✅ `alloc` |
| **L2** | `engine` | A **sans-I/O** protocol engine: correlation, the one-outstanding-`CALL` rule, timeouts, transaction retries, the offline queue, the boot state machine | ✅ `alloc` |
| **L3** | `transport` | Tokio + `rustls`, with its own RFC 6455 WebSocket and RFC 7692 compression: charging station, CSMS and local controller; security profiles 1–3; network failover | – |
| **L4** | `station` · `csms` | Opt-in blocks: device model, 1.6 configuration keys, transaction rules, local authorization, composite schedules, an idempotent CSMS ledger, version-agnostic events | partial |
| — | `standard` | The catalogues that live outside the schemas: security events with their criticality, 74 standardized components, 448 variables, reason codes | ✅ `alloc` |

With default features there is no async runtime, no TLS and no domain logic in your binary —
just the types, the framing and the engine.

## A charging station

```rust
use ocpp_kit::transport::{BasicAuthPassword, ClientTls, Handler, SecurityProfile, Station};
use ocpp_kit::{Version, v2_1};

async fn boot(handler: impl Handler) -> Result<(), Box<dyn std::error::Error>> {
    let handle = Station::builder()
        .identity("CS-0001")?
        .url("wss://csms.example.com/ocpp")
        // Part 4 §3.2 recommends that a 2.1 station also offer 2.0.1.
        .versions([Version::V2_1, Version::V2_0_1])
        .security_profile(SecurityProfile::TlsBasicAuth)
        .password(BasicAuthPassword::utf8("a-sixteen-plus-character-secret")?)
        .tls(ClientTls::with_webpki_roots()?)
        .handler(handler)
        .build()?
        .spawn()?;

    let boot = handle
        .call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        ))
        .await?;

    println!("{:?}, heartbeat every {}s", boot.status, boot.interval);
    Ok(())
}
```

The offline queue, the reconnect back-off, the heartbeat and the boot gate are all handled
underneath: anything you send before the CSMS accepts the station is queued and released the
moment it does, and a connection the network has silently dropped is detected by an unanswered
WebSocket ping instead of waiting out the operating system's TCP timeout.

Add `.store(FileStore::open("/var/lib/ocpp/queue.jsonl")?)` and the queue survives a power cut,
which E04/E08/E12 require of a station.

Run the pair for yourself:

```console
$ cargo run --features full --example minimal_csms
$ cargo run --features full --example minimal_station
```

## Why another OCPP crate

**Error codes are exact.** OCPP does not have one "bad payload" error; it has five, and
answering with the wrong one is a conformance failure that most implementations make. A
missing member is an `OccurrenceConstraintViolation`, a string where an integer belongs is a
`TypeConstraintViolation`, an over-long string is a `PropertyConstraintViolation`, a
non-object payload is a `FormatViolation` — spelled `FormationViolation` when you are talking
1.6 — and a frame that is not a valid RPC request at all is an `RpcFrameworkError`, which
1.6 does not have and therefore degrades to `GenericError`.

```console
$ echo '{"reason":"PowerUp","chargingStation":{"model":"a-model-name-that-is-far-too-long","vendorName":"ACME"}}' \
    | ocpp-cli validate --action BootNotification
error: /chargingStation/model: maxLength 20 exceeded (got 32 characters)
  OCPP error code: PropertyConstraintViolation
  path: /chargingStation/model
```

**The protocol logic has no I/O.** The engine is a pure state machine — feed it events with the
current instant, drain effects. Every timing rule in OCPP is therefore testable in microseconds
rather than minutes, and the same code runs on Tokio, in an `embassy` firmware loop and in
WebAssembly. The `thumbv7em-none-eabihf` build of L0–L4 is checked in CI.

```rust
# use std::time::Duration;
# use ocpp_kit::Version;
# use ocpp_kit::engine::{EngineConfig, Role};
# use ocpp_kit::testkit::Sim;
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut sim = Sim::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
sim.connect(Version::V2_1);
sim.call("BootNotification", r#"{"reason":"PowerUp"}"#)?;
sim.advance(Duration::from_secs(31));          // the message timeout, instantly
assert_eq!(sim.failures().len(), 1);
# Ok(()) }
```

Time is a parameter of *every* entry point, not just the timer one — a deadline armed against
a clock that has not moved since the last timer fired expires the moment it is armed.

**Both sides, and the middle.** Charging Station, CSMS and the Part 4 chapter 6 Local
Controller are built from the same engine. The local controller opens one upstream connection
per attached station under the station's own identity, propagates closes in both directions,
and relays the OCPP-J text unchanged so signed messages survive.

**The types are checked against the schemas.** Every action's request and response is
round-tripped through pseudo-random schema-valid payloads in CI, and the result is validated
back against the official schema — 9 000+ round trips per run. The types cannot quietly drop
a member, widen a constraint, or miss an enumeration value.

**Compression, because 2.1 requires it.** Part 4 §3.4 Table 2 makes RFC 7692
`permessage-deflate` **required** for a CSMS and a Local Controller. No general-purpose Rust
WebSocket crate implements it — and RFC 6455 obliges a crate that does not to *reject* the
`RSV1` bit a compressed message sets. So the frame layer is in this crate: a focused RFC 6455
implementation with compression, checked against `tokio-tungstenite` in both directions,
fuzzed, and verified compressed on the wire. Framing costs ~110 ns per message, and a repeated
`TransactionEvent` frame goes out at **5 % of its size** once the DEFLATE window has warmed up.
See [the WebSocket layer](https://hupe1980.github.io/ocpp-kit/docs/websocket/).

**Message-level signatures, and a policy you cannot forget.** With a Local Controller in the
path, TLS proves you are talking to the *controller*, not the CSMS. Part 4 chapter 7's JWS
signed messages — `<Action>-Signed`, the flattened JWS serialization, the `OCPPAction` /
`OCPPMessageTypedId` / `x5t#S256` headers — are implemented, with `Signer` and `Verifier` as
traits so a key in a secure element never has to reach RAM.

`verify_frame` takes a `SignaturePolicy` and has no default: "verify it if it is signed" is not
a check, because an intermediary can delete three JSON members and a name suffix and the
receiver cannot tell.

**Unsafe options are never the default.** A00.FR.207 requires the CSMS to validate that the
Basic username is the identity from the URL, so `Csms` answers 401 on a mismatch before your
`Authenticator` sees it — left to the application it is a trap, since HTTP Basic hands you a
username while the session is filed under the identity from the path. And a CSMS with no
authenticator does not build: accepting everyone is spelled `authenticate(AcceptEveryStation)`,
because a server that authenticates nobody looks exactly like one that authenticates everybody
successfully.

**The vocabulary, not just the messages.** The schemas define the messages; the appendix
defines what they talk about. `ocpp_kit::standard` carries the 21 security events *with their
criticality*, the 74 standardized components and their 448 variables *with their data types
and units*, and the standardized reason codes — extracted from the specification, asserted by
tests, and wired into the device model so a station gets them right without transcribing a PDF.

**Version differences are modelled, not averaged.** They are real, and they are in the code
with the citation next to them:

| | 1.6J | 2.0.1 | 2.1 |
|---|---|---|---|
| Message types | 2, 3, 4 | 2, 3, 4 | + 5 `CALLRESULTERROR`, 6 `SEND` |
| Unknown message type number | ignore the payload | answer `MessageTypeNotSupported` | ignore the payload |
| Unreadable `MessageId` | no rule | answer with id `"-1"` | answer with id `"-1"` |
| "Syntactically incorrect" | `FormationViolation` | `FormatViolation` | `FormatViolation` |
| Occurrence violation | `OccurenceConstraintViolation` | `OccurrenceConstraintViolation` | `OccurrenceConstraintViolation` |
| One outstanding `CALL` | `SHOULD NOT` | `SHALL NOT` | `SHALL NOT` |
| Basic-auth password | `AuthorizationKey`, hexadecimal | `BasicAuthPassword`, UTF-8, ≥ 16 chars, ceiling 40–64 | same |
| `permessage-deflate` | discouraged | – | CSMS and LC **shall** support — [implemented](https://hupe1980.github.io/ocpp-kit/docs/websocket/) |
| Transaction ordering | strictly chronological (§3.7) | same | same |

**You can test your own code with the tools this crate tests itself with.** `testkit` ships
`Sim` and `Recorder` — what the protocol-rule tests here are written against — and mock
peers that give you a working CSMS or Charging Station on a loopback port in one line:

```rust,no_run
# use std::time::Duration;
# use ocpp_kit::Version;
# use ocpp_kit::testkit::{MockCsms, MockStation};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let csms = MockCsms::start().await?;
let station = MockStation::connect(csms.url(), "CS-0001")?;
station.boot(Version::V2_1).await?;
assert!(csms.wait_for("BootNotification", Duration::from_secs(5)).await);
# Ok(()) }
```

**A `SEND` cannot be awaited.** OCPP 2.1's `SEND` is never answered (Part 4 §4.2.4), so
`Handle::call` does not accept one — the bound is `Confirmed`, which the code generator
implements for exactly the actions whose schemas define a response. Unconfirmed messages go
through `Handle::send`, which returns when the frame is written. What would otherwise be a
message timeout in production is a compile error.

**Real-world tolerance is a policy, not a guess.** The strict default is exactly what the
specification requires. `DecodeOptions::lenient()` additionally accepts the things field
devices actually send — timestamps without an offset, `"42"` where a number belongs, enum
values that are not in the schema — and it does so with a bounded *repair loop* that runs
only after the strict parse has already failed, so conforming traffic costs nothing.

## `ocpp-cli`

```console
$ cargo install ocpp-kit --features cli

$ ocpp-cli actions --version 2.1 --block R
ACTION                               BLOCK                    KIND    DIRECTION
ClearDERControl                      R                        CALL    CSMS -> CS
GetDERControl                        R                        CALL    CSMS -> CS
NotifyDERAlarm                       R                        CALL    CS  -> CSMS
…

$ ocpp-cli replay capture.ocppcap --version 2.1     # validate a whole capture
$ ocpp-cli csms --bind 127.0.0.1:9000               # a mock CSMS
$ ocpp-cli station --url ws://… --identity CS-0001  # a mock charging station
```

## Feature flags

| Feature | Default | What it turns on |
|---|---|---|
| `std` | ✅ | `std` types and `std::error::Error`; off gives `alloc`-only L0–L2 |
| `v1_6`, `v2_0_1`, `v2_1` | ✅ | The per-version types and dispatch tables |
| `tokio` | – | L3: the Tokio + WebSocket transport |
| `rustls` | – | TLS for security profiles 2 and 3 |
| `compression` | – | RFC 7692 `permessage-deflate`, which 2.1 requires of a CSMS |
| `signed-messages` | – | JWS signed messages (Part 4 ch. 7) |
| `getrandom` | – | Random `MessageId`s and `transactionId`s, which is what Part 4 §4.1.4 and E01.FR.08 need; implied by `tokio` |
| `testkit` | – | A recorder for the sans-I/O engine and mock peers, for your `[dev-dependencies]` |
| `jws-es256` | – | A ready-made software ES256 signer and verifier |
| `station`, `csms` | – | The L4 building blocks |
| `tracing` | – | `tracing` spans and events in the transport |
| `cli` | – | The `ocpp-cli` binary |
| `full` | – | Everything above |

`ocpp_kit::standard` — the security events, standardized components and reason codes — is
always available; it is data, not machinery.

## Documentation

* [The guide](https://hupe1980.github.io/ocpp-kit/docs/) — layer by layer, with the
  specification citations
* [API reference](https://docs.rs/ocpp-kit)
* [`SPEC_EDITIONS.md`](SPEC_EDITIONS.md) — exactly which editions and errata the generated
  code targets
* [`CHANGELOG.md`](CHANGELOG.md)

## Development

```console
$ cargo xtask codegen           # regenerate src/v1_6, src/v2_0_1, src/v2_1 from schemas/
$ cargo xtask codegen --check   # what CI runs; fails if the committed code is stale
$ cargo xtask schema-report     # action, enum and type counts per version
$ cargo xtask coverage          # which specification requirement IDs the tests cite
$ cargo xtask coverage --profile core   # how much of a certification profile the tests drive
$ cargo xtask appendix          # regenerate src/standard from Part 2 — Appendices
$ cargo test --features full
```

Generated code is committed, so building the crate needs neither the schemas nor a network
connection, and a schema change shows up as a readable diff.

## JSON schemas

The OCPP JSON schemas in [`schemas/`](schemas/) are published by the Open Charge Alliance and
are redistributed unmodified; see [`schemas/NOTICE`](schemas/NOTICE). They are **not** covered
by this crate's license. The specification PDFs are not redistributed.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
