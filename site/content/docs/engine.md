+++
title = "The sans-I/O engine"
description = "A pure OCPP state machine: the one-outstanding-CALL rule, message timeouts, transaction retries, the durable offline queue and the boot state machine — with time as an input."
weight = 40
+++

`engine::Engine` owns no socket, reads no clock and spawns no task. The driver feeds it
`Input`s — each with the current `Instant` — and drains `Output`s.

```rust
use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Output, Role};
use ocpp_kit::{RawValue, Version};

let now = Instant::ZERO;
let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
engine.handle(now, Input::Connected { version: Version::V2_1 });

let payload = RawValue::from_string(
    r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}"#.into(),
).unwrap();
engine.call(now, "BootNotification", payload).unwrap();

let frame = engine.drain().into_iter().find_map(|output| match output {
    Output::Transmit(text) => Some(text),
    _ => None,
}).unwrap();
assert!(frame.starts_with("[2,"));
```

Why the indirection is worth it:

* **Time is an input, on every entry point.** `Engine::handle`, `call`, `respond` and the rest
  all take the driver's `Instant`, so a 30-second message timeout, a 60-second retry interval
  and a 90-second boot back-off all run in microseconds in the test suite — deterministically.
  Everywhere rather than only on `Input::Timeout`, because a deadline armed against a stale
  clock expires the moment it is armed.
* **It runs anywhere.** The same engine drives the Tokio transport here and compiles for
  `thumbv7em-none-eabihf` with `no_std` + `alloc`.
* **The rules live in one place.** Charging station, CSMS and local controller share one
  implementation.

## Rules it enforces

**One outstanding `CALL` per direction.** 2.x Part 4 §4.1.1 says `SHALL NOT`; 1.6J §4.1.1 says
`SHOULD NOT`. Your own extra calls are queued, never rejected. A peer that breaks the rule is
served by default — dropping its calls is usually worse — and `InboundConcurrency::Reject`
answers `ProtocolError` instead.

**`SEND` is exempt, and is never answered.** A `SEND` goes out even while a `CALL` is
outstanding (Part 4 §4.2.4), and nothing is ever sent back for one (FR.07). An action defined
as a `SEND` that arrives as a `CALL` is a protocol error (N15.FR.01).

**Message timeout.** 30 seconds by default; 2.x sources it from
`NetworkConnectionProfile.messageTimeout`, falling back to
`OCPPCommCtrlr.MessageTimeout[Default]`. On expiry the slot is freed and the next queued call
goes out.

**Transaction retries, and only transaction retries.** Both 1.6 (§3.7.1) and 2.x
(`MessageAttempts[TransactionEvent]`) prescribe the same **linear** schedule — wait
`interval × preceding transmissions` — not an exponential one. Once the attempts are exhausted
the message is *skipped*, exactly as §3.7.1 permits. Nothing else is ever retried.

**Chronological order.** 1.6 §3.7: "the delivery of new transaction-related messages SHALL wait
until the queue has been emptied". A transaction message waiting out its retry interval blocks
every later one. A message that is *not* transaction-related is explicitly allowed to overtake
the queue, "so that customers are not kept waiting".

**A durable offline queue.** Transaction-related messages are written to a `MessageStore` before
being sent and acknowledged only once answered, so a station that loses power replays them in
order (E04.FR.01–03, E08.FR.05–07, E12.FR.01–02). `Engine::queued()` is what
`GetTransactionStatus.messagesInQueue` reports.

Two implementations ship: `MemStore`, which is all a CSMS needs, and `FileStore` for a station,
since E04/E08/E12 are not optional. `FileStore` is an append-only journal, one line of JSON per
change, `sync_data`-flushed before a write is reported as successful, compacted when the dead
records outnumber the live ones.

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use ocpp_kit::Version;
use ocpp_kit::engine::{Engine, EngineConfig, FileStore, Role};

// Whatever the outage interrupted is back in the queue by the time this returns.
let store = FileStore::open("/var/lib/ocpp/queue.jsonl")?;
let engine = Engine::with_store(EngineConfig::new(Role::ChargingStation, Version::V2_1), store)?;
# let _ = engine;
# Ok(()) }
```

A crash mid-append leaves a partial final line, discarded on the next open: it described a
message that was never reported as queued. Not concurrent, and no defence against a corrupt
filesystem — for a station whose state already lives in SQLite, implement the trait against
that instead. It is four synchronous methods.

**A `MessageId` is answered once.** Part 4 §4.2.3 names "an existing message with the same
unique identifier is being handled already" as a `CALLERROR` condition, and it is right to: two
answers under one id are indistinguishable to the sender. A reused id is refused with
`RpcFrameworkError` rather than dispatched a second time. Serving a peer that breaks the
one-outstanding rule is a kindness; `EngineConfig::max_peer_requests` is where that kindness
stops, so no peer can make the receiver hold requests without limit.

**The boot state machine, on both sides.** On the station: until the CSMS answers `Accepted`,
nothing but `BootNotification` leaves unless the CSMS asked for it (B02.FR.02). `Pending` keeps
the connection open (B02.FR.06) and schedules a retry after the interval the CSMS gave, or a
local back-off when it gave `0` (B02.FR.04 / FR.07 / FR.08).

On the CSMS, the rule is narrower than it first looks. B01.FR.10 and B02.FR.09 both begin
*"the Charging Station **has received** a `BootNotificationResponse`"* with a status other than
`Accepted` — so `SecurityError` is the answer to a station this CSMS has told to wait, not to
one it has simply not heard from. That distinction matters, because Part 4 §5.4 tells a
reconnecting station **not** to repeat its `BootNotification`: gating the un-answered state
would refuse every message from every reconnect. Anything the CSMS itself asked for —
`TriggerMessage`, `GetBaseReport`, `CustomerInformation` — is let through in any state
(B02.FR.01).

On the station, the other half of that is yours to declare: mark the answer to a
`TriggerMessage` with `CallOptions::triggered()`, or the boot gate holds it back until the
CSMS accepts the station and the trigger goes unanswered.

```rust,no_run
# async fn example(handle: ocpp_kit::transport::Handle) -> Result<(), Box<dyn std::error::Error>> {
use ocpp_kit::engine::CallOptions;
use ocpp_kit::types::DateTime;
use ocpp_kit::v2_1;

handle
    .call_with(
        v2_1::StatusNotificationRequest::new(
            DateTime::now(),
            v2_1::ConnectorStatus::Available,
            1,
            1,
        ),
        CallOptions::triggered(),
    )
    .await?;
# Ok(()) }
```

**Heartbeats are an idle timer, not a metronome.** `OCPPCommCtrlr.HeartbeatInterval` is defined
as the *"interval of inactivity (no OCPP exchanges) with CSMS after which the Charging Station
should send `HeartbeatRequest`"*, so every frame in either direction postpones the next one —
a busy station sends none at all. Reading it as a fixed period anchored on the previous
response has a failure mode worth naming: one `Heartbeat` that times out ends the sequence for
good, and the station goes quiet exactly when the CSMS most needs to hear from it.

The interval comes from `BootNotificationResponse`, the payload is empty in every version, and
the `currentTime` in each answer is surfaced as a `ClockSample` — which is what a station whose
`ClockCtrlr.TimeSource` is `Heartbeat` needs.

**Graceful drain.** `shutdown(now, deadline)` refuses new calls, finishes the outstanding one,
flushes the queue and only then asks for the connection to close.

**A malformed frame is answered the way §4.2.3 says, or not at all.** A broken `CALL` gets a
`CALLERROR`; a broken `CALLRESULT` gets a `CALLRESULTERROR` on 2.1 and nothing before it; a
broken `SEND` or `CALLERROR` gets nothing, since answering an error frame with another is how
two peers start trading them. Incoming frames are forgiving about the one thing the field gets
wrong constantly: a `CALLERROR` that omits its `errorDescription` or `errorDetails` still
completes the call it answers, rather than stalling it until the message timeout.

## Inputs and outputs

```text
Engine::handle(now, Input)     Output::Transmit(String)
  Input::Connected { version } Output::Request(IncomingRequest)
  Input::Received(&str)        Output::Outcome(CallOutcome)
  Input::Disconnected          Output::SetTimer / ClearTimer
  Input::Timeout               Output::BootState / ClockSample
                               Output::ResultRejected
                               Output::Violation(ProtocolViolation)
                               Output::Close(CloseReason)
```

A `CallOutcome` succeeds as an `Answer`, which is `Answer::Result(payload)` for a `CALL` and
`Answer::Sent` for a `SEND`. They are different kinds of success and the type says so: §4.2.4
forbids the receiver from ever answering a `SEND`, so handing back an empty payload would let
a caller wait for a message that cannot arrive.

`Output::Violation` is the observability channel: an unexpected response, a concurrent call, a
reused or non-conforming message id, an unparseable frame. None is fatal on its own.

## Message ids

Part 4 §4.1.4 is stricter than it reads at first: an id must differ from every id the same
sender has used for a `CALL` or a `SEND` **on any connection under the same Charging Station
identity** — not merely within one connection. A counter that restarts at zero after a power
cut therefore violates it, and the collision lands precisely on the messages a station replays
from its offline queue after that power cut.

`RandomIds` (a version 4 UUID per id) is the default wherever an entropy source is available,
which is every build with the `getrandom` feature — implied by `tokio`. `CounterIds` is there
for targets without one, and satisfies §4.1.4 only if you give it a prefix that changes on
every boot. Retransmissions are the one exception the specification allows: reusing the
original id for those is explicitly permitted.

## Driving it

```rust,no_run
# use ocpp_kit::engine::{Engine, Input, Output, Instant, MemStore};
# fn example(engine: &mut Engine<MemStore>, text: &str, now: Instant) {
engine.handle(now, Input::Received(text));
engine.handle(now, Input::Timeout);
while let Some(output) = engine.poll_output() {
    match output {
        Output::Transmit(frame) => { /* write it to the socket */ }
        Output::Request(request) => { /* dispatch, then `engine.respond(…)` */ }
        Output::SetTimer { timer, at } => { /* arm it; a spurious tick is harmless */ }
        _ => {}
    }
}
# }
```

[`transport`](@/docs/transport.md) is that loop, written once against Tokio.
