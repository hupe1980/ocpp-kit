# ocpp-kit

An [Open Charge Point Protocol](https://openchargealliance.org/) toolkit for Rust —
**OCPP 1.6J, 2.0.1 and 2.1** over JSON/WebSocket, for charging stations, CSMS backends and
local controllers.

The crate is five independent layers. Each one is useful on its own, and you pay only for
the ones you turn on: with default features there is no async runtime, no TLS and no domain
logic in your binary.

| Layer | Module | What it gives you | `no_std` |
|---|---|---|---|
| **L0** | [`v1_6`], [`v2_0_1`], [`v2_1`] | Typed, validated payloads for all 39 / 64 / 91 actions, generated from the official OCA JSON schemas | ✅ `alloc` |
| **L1** | [`rpc`] | OCPP-J framing: `CALL`, `CALLRESULT`, `CALLERROR`, `CALLRESULTERROR`, `SEND`, with version-exact error codes, plus JWS signed messages | ✅ `alloc` |
| **L2** | [`engine`] | A sans-I/O protocol engine: correlation, the one-outstanding-`CALL` rule, timeouts, transaction retries, the offline queue, the boot state machine | ✅ `alloc` |
| **L3** | [`transport`] | Tokio + `rustls`, with its own RFC 6455 WebSocket and RFC 7692 compression: [`Station`](transport::Station), [`Csms`](transport::Csms), [`LocalController`](transport::LocalController), security profiles 1–3, network failover | – |
| **L4** | [`station`], [`csms`] | Opt-in building blocks: device model, 1.6 configuration keys, transaction rules, local authorization, composite schedules, an idempotent CSMS ledger, version-agnostic events | partial |
| — | [`standard`] | The catalogues that live outside the schemas: security events with their criticality, 74 standardized components, 448 variables, reason codes | ✅ `alloc` |
| — | [`metering`] | Signed meter values: the record a customer may be billed for, in every version's hiding place | ✅ `alloc` |

## Getting started

A Charging Station that boots and then talks to its CSMS:

```no_run
# #[cfg(all(feature = "tokio", feature = "v2_1"))]
# mod example {
use ocpp_kit::transport::{BasicAuthPassword, ClientTls, SecurityProfile, Station};
use ocpp_kit::{Version, v2_1};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let handle = Station::builder()
    .identity("CS-0001")?
    .url("wss://csms.example.com/ocpp")
    // Part 4 §3.2 recommends that a 2.1 station also offer 2.0.1.
    .versions([Version::V2_1, Version::V2_0_1])
    .security_profile(SecurityProfile::TlsBasicAuth)
    .password(BasicAuthPassword::utf8("a-sixteen-plus-character-secret")?)
    .tls(ClientTls::with_webpki_roots()?)
    .build()?
    .spawn()?;

let boot = handle
    .call(v2_1::BootNotificationRequest::new(
        v2_1::ChargingStation::new("Model-1", "ACME"),
        v2_1::BootReason::PowerUp,
    ))
    .await?;
println!("{:?}, heartbeat every {}s", boot.status, boot.interval);
# Ok(()) }
# }
```

The CSMS side is the mirror image — see [`transport::Csms`]. The guide at
<https://hupe1980.github.io/ocpp-kit/docs/> walks through each layer with the specification
citations.

## What is different about this crate

**Error codes are exact.** OCPP does not have one "bad payload" error; it has five, and
answering with the wrong one is a conformance failure. A missing member is an
`OccurrenceConstraintViolation`, a string where an integer belongs is a
`TypeConstraintViolation`, an over-long string is a `PropertyConstraintViolation`, and a
payload that is not an object at all is a `FormatViolation` — spelled `FormationViolation`
when you are talking 1.6. See [`decode`] and [`rpc::ErrorCode`].

**The value that pays for the electricity.** An OCPP meter value is telemetry; under German
calibration law a customer may only be billed for the record the *meter* signed, which is not
the protocol's own number and often not even the same quantity. [`metering`] is the protocol
knowledge around it: 1.6 hides the whole 2.x `SignedMeterValueType` inside the `value` string
of a `SignedData` sample, and the `publicKey` field is an envelope whose specification and
whose own example message disagree. Both shapes are read, and every signed record reaches the
[`csms::events`] funnel untouched.

**Nothing that decides money fails silently.** A station that claims to send signed meter data
and sends something unparseable is otherwise indistinguishable from one sending none, so every
such drop raises an [`Observed::warnings`](csms::events::Observed) entry naming what arrived.
The funnel also carries the `charging_state` and the EVSE a session is at — the facts a meter
reading cannot supply and a CDR needs.

**Numbers are exact, not `f64`.** Every OCPP `number` — a meter register, a charging limit, a
2.1 tariff price — is a [`types::Decimal`](types::Decimal): a mantissa and a decimal scale. A
meter reporting `2935.600` kWh is claiming three decimals of resolution, and it goes back out
as `2935.600`; a session's energy, which OCPP *defines* as a difference of two registers, is
that difference exactly rather than `10.000000000000002`. Literals are written
[`decimal!(32.5)`](decimal), and the `f64` conversions are named `to_f64_lossy` /
`from_f64_lossy` so a signature says what it costs. `cargo xtask no-floats` keeps them out of
the public API, and CI runs it.

**The protocol logic has no I/O.** [`engine::Engine`] is a pure state machine: you feed it
[`Input`](engine::Input)s — each with the driver's current [`Instant`](engine::Instant) — and
drain [`Output`](engine::Output)s. Every timing rule in OCPP is therefore testable in
microseconds rather than minutes, and the same code runs on Tokio, in an `embassy` firmware
loop and in WebAssembly. [`testkit::Sim`](testkit::Sim) owns the clock so a test reads as a
transcript.

**A `SEND` cannot be awaited.** OCPP 2.1's `SEND` is never answered (Part 4 §4.2.4), so
[`Handle::call`](transport::Handle::call) is bounded by [`Confirmed`](message::Confirmed),
which the code generator implements for exactly the actions whose schemas define a response.
Unconfirmed messages go through [`Handle::send`](transport::Handle::send).

**Both sides, and the middle.** Charging Station, CSMS and the Part 4 chapter 6 Local
Controller are all built from the same engine, down to Part 4 §5.3's ping/pong check, which a
Local Controller runs on each of its two legs separately.

**The offline queue survives a power cut.** E04/E08/E12 require a station to replay the
transaction messages an outage interrupted, so [`FileStore`](engine::FileStore) ships alongside
[`MemStore`](engine::MemStore): an append-only journal, flushed to the device before a write is
reported as successful.

**Compression, because 2.1 requires it.** Part 4 §3.4 makes RFC 7692 `permessage-deflate`
required for a CSMS and a Local Controller, and no general-purpose Rust WebSocket crate
implements it — RFC 6455 obliges one that does not to *reject* the `RSV1` bit a compressed
message sets. So [`transport`] contains a focused RFC 6455 implementation with compression,
checked against an independent one in both directions and fuzzed.

**The types are checked against the schemas.** Every action's request and response is
round-tripped through pseudo-random schema-valid payloads in CI, and the output is validated
back against the official schema — so the types cannot quietly drop a member, widen a
constraint, or miss an enumeration value.

**Version differences are modelled, not averaged.** 1.6 and 2.x really do differ:
`FormationViolation` vs `FormatViolation`, `OccurenceConstraintViolation` (one `r`, as the
1.6 specification prints it) vs `OccurrenceConstraintViolation`, an unknown message type
being ignored on 1.6J and 2.1 but answered on 2.0.1, `SEND` and `CALLRESULTERROR` existing
only in 2.1. All of it is in the code, with the citation next to it.

## Feature flags

| Feature | Default | What it turns on |
|---|---|---|
| `std` | ✅ | `std` types and `std::error::Error`; off gives `alloc`-only L0–L2 |
| `v1_6`, `v2_0_1`, `v2_1` | ✅ | The per-version types and dispatch tables |
| `tokio` | – | L3: the Tokio + WebSocket transport |
| `rustls` | – | TLS for security profiles 2 and 3 |
| `compression` | – | RFC 7692 `permessage-deflate`, required of a CSMS by 2.1 |
| `getrandom` | – | An entropy source, so `MessageId`s are UUIDs. Implied by `tokio` |
| `signed-messages` | – | JWS signed messages (Part 4 ch. 7) |
| `jws-es256` | – | A ready-made software ES256 signer and verifier |
| `station` | – | L4 Charging Station blocks |
| `csms` | – | L4 CSMS blocks |
| `tracing` | – | `tracing` spans and events in the transport |
| `cli` | – | The `ocpp-cli` binary |
| `testkit` | – | [`Sim`](testkit::Sim), [`Recorder`](testkit::Recorder) and mock peers, for your `[dev-dependencies]` |
| `full` | – | Everything above |

## The JSON schemas

The OCPP JSON schemas are published by the Open Charge Alliance and are redistributed
unmodified in the repository's `schemas/` directory; see `schemas/NOTICE`. They are **not**
covered by this crate's license. Generated code is committed, so building `ocpp-kit` needs
neither the schemas nor a network connection.
