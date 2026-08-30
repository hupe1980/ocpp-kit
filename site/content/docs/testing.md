+++
title = "Testing and conformance"
description = "9288 schema-generated round trips, simulated-time protocol rule tests, randomised properties, four fuzz targets, and WebSocket interop against an independent implementation."
weight = 120
+++

Correctness claims are worth what their evidence is worth. Here is the evidence.

## Schema conformance — the types cannot drift

`tests/schema_conformance.rs` takes every action of every version and, for both its request and
its response:

1. generates pseudo-random **schema-valid** payloads directly from the official OCA schema;
2. feeds them through the Rust types and serializes the result back;
3. checks that nothing was **dropped or invented** — every member and value survives;
4. validates the output **against the schema again**.

```console
$ cargo test --test schema_conformance -- --nocapture
OCPP 1.6: 1872 payload round trips checked
OCPP 2.0.1: 3072 payload round trips checked
OCPP 2.1: 4344 payload round trips checked
```

This is what catches the mistakes hand-written OCPP libraries actually make: a member typed as
optional that is required, a missing enumeration value, a dropped field, a constraint that was
widened. The generator is deterministic, so a failure always reproduces from its seed. The
round trip runs in `pedantic` mode, so a member the Rust types do not model shows up as an
unknown field rather than being silently ignored.

## Error classification — pinned, not assumed

`tests/decode_classification.rs` asserts the mapping from every kind of bad payload to the OCPP
error code the specification names, and to the JSON pointer of the offending member. `serde`'s
messages are the only structured signal a derived `Deserialize` gives us, so the mapping
depends on their wording — which is exactly why it is pinned by tests. A `serde` change that
altered them fails CI rather than silently downgrading every error to `FormatViolation`.

## Protocol rules — simulated time

`tests/engine_rules.rs` drives the sans-I/O engine with a synthetic clock. Each test is named
after the rule it checks, and several cite the requirement id:

```text
only_one_call_is_outstanding_at_a_time_and_the_rest_queue
a_send_bypasses_the_outstanding_call_slot
a_send_received_as_a_call_is_a_protocol_error_n15_fr_01
a_send_is_never_answered_even_when_it_is_unusable
before_acceptance_only_boot_notification_leaves_the_station_b02_fr_02
pending_schedules_a_boot_retry_and_keeps_the_connection
interval_zero_falls_back_to_a_local_backoff_b02_fr_07
a_csms_answers_an_unsolicited_call_from_a_pending_station_with_security_error_b02_fr_09
transaction_messages_are_retried_on_the_linear_schedule_and_then_skipped
transaction_messages_survive_a_disconnection_and_replay_in_order
a_durable_store_replays_what_a_power_cut_interrupted
an_unknown_message_type_is_ignored_on_21_and_answered_on_201
```

A 90-second boot back-off and a three-attempt retry schedule spanning five minutes both run in
microseconds, because time is an input.

## Properties — hundreds of random scenarios

`tests/properties.rs` checks the claims a hand-picked example cannot establish, over hundreds
of pseudo-random scenarios from a deterministic generator:

* a transaction message is never **lost or reordered**, however the link flaps — the property
  that found a real bug, where a message waiting out its retry interval could be overtaken by a
  later one, in violation of 1.6 §3.7;
* every call reaches **exactly one** terminal outcome, or is still legitimately queued;
* the CSMS ledger reaches the **same state** whatever order events arrive in, and always
  recognises a repeat;
* a gap is reported **exactly** when a sequence number is missing;
* a composite schedule agrees with the stacking rules **evaluated directly**, and never
  contains two consecutive steps with the same limit.

## The WebSocket layer — against a reference implementation

`tests/websocket_interop.rs` puts every frame past `tokio-tungstenite`, an independent and
widely used implementation, in both directions: it serves our client, we serve its client, and
a message it fragments we reassemble. Compression is checked on the wire, not merely
configured — a raw socket verifies the `Sec-WebSocket-Extensions` response, sends a frame with
`RSV1` set, and checks the answer comes back compressed. `tokio-tungstenite` is a
**dev-dependency**: the reference, never the runtime.

## End to end — real sockets

`tests/transport_e2e.rs` runs a Charging Station, a CSMS and a Local Controller against each
other over loopback TCP: the boot handshake, a CSMS-initiated call, the 401/404 distinction, a
negotiation failure when there is no common version, and a Local Controller intercepting
`SetChargingProfile` while everything else passes through.

`tests/rpc_framing.rs` covers framing in both directions: parse, re-serialize, the
version-specific error spellings, the `"-1"` rule, `SEND` and `CALLRESULTERROR` being 2.1-only,
and the 2.0.1-versus-2.1 difference in what an unknown message type deserves.

## Fuzzing

Four `cargo fuzz` targets, over the parsers that face hostile input:

| Target | Property |
|---|---|
| `frame` | the OCPP-J parser never panics, and anything it parses re-serializes and re-parses to the same frame |
| `payload` | payload decoding never panics under any policy, and the types are closed under their own serialization |
| `engine` | the engine survives arbitrary peer input and only ever emits frames it can parse itself |
| `websocket` | the WebSocket codec always terminates, never panics, and round-trips whatever it decodes |

## Certification profiles

`cargo xtask coverage --profile core` answers "how much of the Core certification profile do
the scenario tests actually drive?", from the OCPP 2.0.1 Part 5 table:

```console
$ cargo xtask coverage --profile core
Core (OCPP 2.0.1 Part 5)

  [x] BootNotification
  [x] Heartbeat
  [ ] GetBaseReport
  …
  11/34 action(s) named in a scenario test

  mandatory controller components (Part 5 §5):
    [x] OCPPCommCtrlr
    [x] TxCtrlr
    …
```

It is a coverage *signal*, not a certification — certification is a test-lab activity against
the OCA test tool (OCTT), which needs the tool, a licence and a running system. What this crate
provides is the traceability that makes it tractable: every action carries its functional
block, `cargo xtask coverage` reports which requirement ids the source and tests cite, and the
engine's rules are individually testable.

## Testing *your* code — the `testkit` feature

Everything above is how this crate proves itself. `ocpp-kit` with `features = ["testkit"]` in
your `[dev-dependencies]` gives you the tools it built to do that.

**A simulated driver.** `Sim` is the engine plus the clock a real driver would own, so a test
reads as a transcript and every timing rule runs in microseconds.

```rust
use std::time::Duration;
use ocpp_kit::Version;
use ocpp_kit::engine::{EngineConfig, Role, Timer};
use ocpp_kit::testkit::Sim;

let mut sim = Sim::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
sim.connect(Version::V2_1);
sim.call("BootNotification", r#"{"reason":"PowerUp"}"#).unwrap();
assert!(sim.only_frame().contains("BootNotification"));

// The id was minted inside the engine, so this is the only way to answer the call.
let id = sim.only_frame_id();
assert!(sim.armed_at(Timer::CallTimeout).is_some());

// Thirty seconds of message timeout, in microseconds of wall clock.
sim.advance(Duration::from_secs(31));
assert_eq!(sim.failures().len(), 1);
```

`advance_to_next_timer()` jumps to whatever is due next, so a test never guesses an interval.

**A recorder.** `Sim` derefs to `Recorder`, the half that answers questions: `frames()`,
`requests()`, `outcomes()`, `failures()`, `violations()`, `timers()`. Use it directly when
something other than `Sim` drives the engine.

The protocol-rule tests in this repository are written against these types, so they cannot
drift into a parallel API.

**Mock peers.** A working CSMS or Charging Station on a loopback port, in one line, that
records what it was asked for.

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use std::time::Duration;
use ocpp_kit::Version;
use ocpp_kit::testkit::{MockCsms, MockStation};

let csms = MockCsms::start().await?;
let station = MockStation::connect(csms.url(), "CS-0001")?;

station.boot(Version::V2_1).await?;
assert!(csms.wait_for("BootNotification", Duration::from_secs(5)).await);
# Ok(()) }
```

`MockCsms::builder().answer(…)` replaces the canned responses, which is how the failure paths
get exercised: return `Err(CallError::…)` and the mock answers a `CALLERROR`.

### What is deliberately not in it

There is **no JSON Schema generator or validator**, though this crate has both and uses them
on every action of every version. Once that check passes, a payload built out of the generated
types is schema-conformant by construction — that is the whole point of having generated
types. Shipping a schema engine, and two megabytes of embedded schemas with it, so downstream
code could re-derive a guarantee the type system already gives it would be paying a real cost
to answer a question nobody has.

There is no scenario file format either. With `Sim`, a transcript is a handful of statements
in a `#[test]` — which every tool already understands, and no
one has to learn.

## Platforms and generated code

CI checks `cargo check --no-default-features --target thumbv7em-none-eabihf`, so the `no_std`
claim is a build failure away from being noticed rather than a README assertion. And
`cargo xtask ci` runs what CI runs — the commands are read out of
`.github/workflows/ci.yml` rather than restated, so a local run cannot drift from the real
one. `--all` includes the steps needing `cargo-hack`, `zola`, `cargo-fuzz` and the cross
targets.

`cargo xtask codegen --check` regenerates from the schemas and fails if the committed output
differs, so a schema update is always a visible, reviewable diff.
