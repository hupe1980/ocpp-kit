# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — unreleased

A hard cut, driven by what a CSMS billing under German calibration law could not get out of
`0.1.0`: an exact number type, and the signed meter record itself.

### Changed

**Every OCPP `number` is an exact decimal.** `types::Decimal` is a signed mantissa and a
decimal scale, not an `f64`. The scale the station sent survives end to end — a meter's
`2935.600` decodes, compares, prints and re-encodes as `2935.600` — the subtraction OCPP
*defines* a session's energy as is exact, and unit conversion (`kWh`, and the 2.x
`unitOfMeasure.multiplier`) moves the decimal point rather than multiplying by `1000.0`.
Literals are `decimal!(32.5)`, parsed at compile time. There is no `From<f64>`; the conversions
are named `Decimal::to_f64_lossy` and `Decimal::from_f64_lossy`.

The same change reaches the composite-schedule calculation (limits, `Supply::voltage`, the
ampere/watt conversion), the device model's numeric limits, and the ledger's meter registers.
`engine::Backoff::delay` takes an integer `engine::Jitter`. `cargo xtask no-floats` fails the
build if an `f32` or `f64` reaches any public signature, and CI runs it.

**The version-agnostic funnel carries what a CDR needs.** The transaction events gained
`signed`, `charging_state`, `evse_id` and `connector_id`; `Record` gained `signed`, `evse_id`
and `connector_id`, and `signed_with_context` to tell a begin record from an end one. 1.6 has
connectors and no EVSEs and says so everywhere, rather than reporting a connector number as an
EVSE id in `MeterValues`.

**`AuthOutcome::Accept` carries a `SessionContext`** — whatever an `Authenticator` resolved
about a station while deciding to admit it rides the session, and is read back by
`Ctx::session` and `Handle::session`. A CSMS keeps no second map keyed on `Identity`, and does
not have to decide what to do when that map misses for a station it definitely admitted.
`AuthOutcome::accept()` is the short spelling for storing nothing.

**`base64` is no longer optional.** Reading a public-key envelope is protocol knowledge, not an
opt-in extra.

### Added

**`metering` — signed meter values**, the record a customer may actually be billed for. Under
`MessEG` §33 a billable value is one the customer can check, which is the data set the meter
signed — not the protocol's own number, and often not the same quantity: in the OCA's example
message a 1.6 `meterStop` is the meter's *lifetime* register while the signed record beside it
reports the session.

* 1.6 has no `signedMeterValue` field. The OCA application note (§3.2.1) reuses 2.x's
  `SignedMeterValueType` by serializing the whole object into the `value` **string** of a
  `SampledValue` whose `format` is `SignedData` — a string holding JSON holding Base64 holding
  the record. `v1_6::SampledValue::signed_meter_value` reads it, and `SampledValue::signed`
  writes it, setting the `format` a station writing the shape by hand forgets.
* `SignedMeterValueType.publicKey` is not key bytes. §3.2.2 specifies Base64 over an
  `oca:<encoding>:<content-type>:<printed key>` envelope, where the last part is the key as
  printed on the meter so a customer can compare it with the label; the same document's example
  message sends Base64 over plain hexadecimal with no envelope. `metering::decode_public_key`
  reads both and reports which arrived, keeping the printed form. It is a claim, not a binding:
  OCMF wants the key out of band.
* `SignedMeterValue::decoded` / `decoded_str` return the record whether the station sent it
  Base64 — as 2.0.1 Part 2 §2.46 specifies — or put the `OCMF|` text in plain, which many do.
  The two cannot collide, because `|` is not in the Base64 alphabet.
* Every record reaches the funnel untouched, and converts back with `TryFrom` for the 2.x
  types, failing with the member the target version requires: 2.0.1 makes all four mandatory,
  2.1 only the record and its encoding.

**`Observed::warnings`** reports what a message said that the version-neutral view could not
carry, where the drop would otherwise be silent and about a value that decides money: an
unreadable 1.6 `SignedData` document, a reading that is not a number, an energy register in a
unit that is not an energy unit, one out of range. A station that *claims* to sign and does not
is otherwise indistinguishable from one that does not claim, and the difference surfaces when a
month of sessions turns out to be unbillable. No schema catches these — 1.6 types a sampled
value as a plain string, and 2.x puts no bound on the multiplier.

**`csms::events::to_ledger_event_with_id`** completes a 1.6 start event with the transaction id
the CSMS assigned in `StartTransaction.conf`. Without it the start register is lost and the
first periodic `MeterValues` silently takes its place.

**`ocpp-cli signed`** reads a `SignedMeterValueType`, or the 1.6 `SignedData` string that holds
one, and reports the record and the key as the station meant them.

**`cargo xtask no-floats`** fails the build on an `f32` or `f64` in any public signature.

**`COVERED_ACTIONS` is checked, not trusted.** A schema-driven test generates a valid request
for every action of every version — 77 station-originated ones — and asserts that each maps to
a modelled event exactly when the list says it does.

### Fixed

* A 1.6 sampled value that is not a number became an `f64::NAN` that poisoned every total it
  reached. It is skipped, and reported as a warning.
* The 2.x `unitOfMeasure.multiplier` was ignored — a factor-10ⁿ error in an invoice.
* An `Ended` transaction event took the first matching sample rather than the closing one, so
  a message carrying both a periodic and a `Transaction.End` reading billed the wrong end.
* A 1.6 `StopTransaction.transactionData` was ignored entirely, which is where every
  calibration-law-compliant 1.6 station puts its billable records.
* A 1.6 `MeterValues` naming a `transactionId` never reached the ledger, so the claim that
  `StartTransaction` / `MeterValues` / `StopTransaction` are one shape was not true.
* 1.6 deduplication did not cover a retried `MeterValues`, though the ledger documented that it
  did — two readings of the same kind bearing the same instant are one reading sent twice.
* The composite schedule compared limits with `f64::EPSILON`, which is the wrong tolerance at
  both ends of the range it has to cover. Exact decimals compare exactly.
* `cargo test` with default features did not compile: the integration tests now declare their
  `required-features`.
* `cargo xtask ci` skipped six checks from a hardcoded list — including the feature powerset,
  on a machine that had `cargo-hack`. It reads the workflow's `env:` block as well as its
  `- run:` commands, so `RUSTFLAGS: -D warnings` means the same thing locally, and decides what
  to skip by probing for the tool.

## [0.1.0] — 2026-08-30

The first functional release. `0.0.1` reserved the crate name and contained nothing.

### Added

**Typed messages (L0)** — all 39 OCPP 1.6J, 64 OCPP 2.0.1 and 91 OCPP 2.1 actions in modules
`v1_6`, `v2_0_1` and `v2_1`, generated from the official OCA JSON schemas by
`cargo xtask codegen` and committed. Constructors for required fields, `with_…` setters for
optional ones, generated `Validate` impls, open enumerations that never fail to parse, and
direction-aware dispatch unions (`CsRequest` / `CsResponse` / `CsmsRequest` / `CsmsResponse`).

The module and feature names are the protocol's own version strings with dots replaced by
underscores, so `2.0.1` is `v2_0_1` and never the ambiguous `v201`.

**OCPP-J framing (L1)** — `CALL`, `CALLRESULT`, `CALLERROR`, and the 2.1 `CALLRESULTERROR` and
`SEND`. Zero-copy two-stage parsing, version-exact error-code spellings
(`FormationViolation` and `OccurenceConstraintViolation` on 1.6), the `"-1"` message-id rule,
and the 2.0.1-versus-2.1 difference in what an unknown message type deserves. `FrameError::reply`
gives §4.2.3's answer for an unparseable frame — `CALLERROR` for a `CALL`, `CALLRESULTERROR`
for a `CALLRESULT`, nothing for a `SEND` or an error frame.

**Signed messages (feature `signed-messages`)** — Part 4 chapter 7: the `<Action>-Signed`
wrapper, the flattened JWS JSON serialization, and the `OCPPAction`, `OCPPMessageTypedId` and
`x5t#S256` protected-header fields. `Signer` and `Verifier` are traits, so a key in a secure
element never has to reach RAM; feature `jws-es256` adds a software ES256 implementation.
`verify_frame` takes a `SignaturePolicy` and has no default: a verifier can only check a
signature that is present, so accepting an unsigned frame is a downgrade, not leniency.

**Sans-I/O engine (L2)** — request/response correlation, the one-outstanding-`CALL` rule with
`SEND` exempt and a bound on how far a non-conforming peer may push it, refusal of a reused
`MessageId` (Part 4 §4.2.3), message timeouts, the linear transaction-retry schedule, a durable
offline queue behind a `MessageStore` trait — `MemStore` in memory, `FileStore` on disk — the
boot state machine on both the station and the CSMS side, heartbeats on the inactivity timer
`OCPPCommCtrlr.HeartbeatInterval` actually defines, clock samples, and graceful drain.

Time is a parameter of every entry point rather than state the engine keeps, as in
`quinn-proto` and `str0m`, so every timing rule is tested in microseconds and no deadline can
be armed against a stale clock. A `CALL` completes with `Answer::Result`, a `SEND` with
`Answer::Sent`, and `Handle::call`'s `Confirmed` bound keeps the two apart at compile time.

Outgoing `MessageId`s are version 4 UUIDs wherever an entropy source is available, because
Part 4 §4.1.4 requires uniqueness across *every* connection under one Charging Station
identity — not just within one. `CounterIds` remains for targets without entropy, and documents
that its prefix has to change on every boot.

**Transport (L3, feature `tokio`)** — `Station`, `Csms` and `LocalController`. `Handle::call`
is typed both ways; `call_with` carries `CallOptions`, which is how the answer to a
`TriggerMessage` passes the boot gate while a station is still `Pending` (B02.FR.02). Subprotocol
negotiation per Part 4 §3.1.2, reconnect back-off per §5.4, security profiles 1–3 with
`rustls` (feature `rustls`), a per-identity session router with single-active-connection
policy, and the 404 / 401 / "no common subprotocol" handshake outcomes. A00.FR.207's check
that the Basic username is the identity from the URL is enforced before the `Authenticator`
runs, and `authenticate(…)` is required — `AcceptEveryStation` exists for the case where
accepting everyone is the intent. The Local Controller implements Part 4 chapter 6: one
upstream connection per station under the station's own identity, close propagation both ways,
§5.3's ping/pong check on each of its two legs, and unchanged relaying of the OCPP-J text so
signed messages survive.

`Keepalive` carries both halves of Part 4 §5.3's liveness check — how often to ping and how
long to wait for the pong — so a connection the network dropped silently ends the session
instead of staying writable until the operating system's TCP timeout expires. The CSMS bounds
pending handshakes and per-session handler tasks as well as established sessions, so a peer
that opens sockets and says nothing cannot crowd out the fleet.

**Its own WebSocket layer, with RFC 7692 compression (feature `compression`)** — OCPP 2.1
Part 4 §3.4 Table 2 makes `permessage-deflate` *required* for a CSMS and a Local Controller,
and RFC 6455 obliges a WebSocket implementation that lacks the extension to reject the `RSV1`
bit a compressed message sets. No general-purpose Rust crate implements it, so the frame layer
is in this crate: full RFC 6455 framing and validation, both halves of the handshake, and
per-message DEFLATE with context takeover. Checked against `tokio-tungstenite` in both
directions (a dev-dependency: the reference, never the runtime), fuzzed, and verified
compressed on the wire.

**Network configuration slots** — the 2.x model of numbered slots,
`NetworkConfigurationPriority` and `NetworkProfileConnectionAttempts`, so failing over to a
second CSMS (use case B10) is a configuration change. Every slot is validated at build time,
and `NetworkConnectionProfile.messageTimeout` overrides the engine's default while its slot is
active.

**Domain building blocks (L4, features `station` and `csms`)** — the 2.x device model, the
1.6 configuration-key registry, transaction start/stop rules, local authorization list and
cache, a charging-profile store with composite-schedule calculation, an idempotent CSMS
transaction ledger with gap detection, and version-agnostic domain events.

Three of these encode rules the specification states as *lists of independent requirements*,
and each is implemented as such rather than collapsed into a compound condition:
`TxStartPoint` and `TxStopPoint` are disjunctions with transition semantics (E01.FR.01–06,
E06.FR.01–07); the composite schedule takes the leading profile *per purpose* — highest stack
level that is both valid and has a period at that instant — then the minimum across purposes,
with `LocalGeneration` added on top (§3.6); and local authorization is gated by all three of
`LocalAuthListEnabled`, `LocalAuthorizeOffline` and `LocalPreAuthorize`, with an online
station asking the CSMS even about a token its own list refuses (C10).

**`testkit` (feature `testkit`)** — the scaffolding this crate tests itself with, for
downstream `[dev-dependencies]`: `Sim`, an `Engine` plus the clock a driver would own, so a
test reads as a transcript; `Recorder`, the half that answers questions about what came out;
and `MockCsms` / `MockStation`, working peers on a loopback port. The protocol-rule tests here
are written against these, so they cannot drift into a parallel API.

**The standard catalogues (`ocpp_kit::standard`)** — the vocabulary that lives in OCPP 2.1
Part 2 — Appendices rather than in any schema: 21 security events with their criticality, 74
standardized components and 448 variables with their data types and units, and 56 standardized
reason codes. Extracted by `cargo xtask appendix` and wired into the device model through
`DeviceModel::declare_standard`.

**Decoding policy** — `DecodeOptions` with `strict()`, `pedantic()` and `lenient()` presets.
Leniency is a bounded repair loop that runs only after a strict parse has failed, so
conforming traffic costs nothing.

**`ocpp-cli` (feature `cli`)** — `actions`, `validate`, `frame`, `replay`, and mock `csms` and
`station` peers.

**Verification** — 250+ tests. 9 000+ schema round trips per run across all three versions;
the error-code mapping pinned by tests; protocol rules driven with simulated time; randomised
property tests over the engine, the ledger and the composite-schedule calculation; WebSocket
interop against an independent implementation; end-to-end tests over real sockets; four
`cargo fuzz` targets; and `thumbv7em-none-eabihf` (`no_std`) and `wasm32` builds in CI.

[0.2.0]: https://github.com/hupe1980/ocpp-kit/releases/tag/v0.2.0
[0.1.0]: https://github.com/hupe1980/ocpp-kit/releases/tag/v0.1.0
