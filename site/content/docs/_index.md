+++
title = "Documentation"
description = "How ocpp-kit is put together: typed OCPP messages, OCPP-J framing, a sans-I/O protocol engine, Tokio transports for charging stations, CSMS and local controllers."
sort_by = "weight"
template = "section.html"
page_template = "page.html"
+++

`ocpp-kit` implements the Open Charge Point Protocol over JSON/WebSocket — **OCPP 1.6J, 2.0.1
and 2.1** — for charging stations, CSMS backends and local controllers.

It is five layers. Each is useful on its own, and you compile only what you enable: with
default features there is no async runtime, no TLS and no domain logic in your binary.

| Layer | Module | What it gives you | `no_std` |
|---|---|---|---|
| **0** | `v1_6` · `v2_0_1` · `v2_1` | Typed, validated payloads for all 39 / 64 / 91 actions | ✅ `alloc` |
| **1** | `rpc` | OCPP-J framing with version-exact error codes, plus JWS signed messages | ✅ `alloc` |
| **2** | `engine` | A sans-I/O protocol state machine | ✅ `alloc` |
| **3** | `transport` | Tokio and `rustls`, with an RFC 6455 WebSocket and RFC 7692 compression | – |
| **4** | `station` · `csms` | Opt-in domain building blocks | partial |
| — | `standard` | The catalogues that live outside the schemas | ✅ `alloc` |
| — | `metering` | [Signed meter values](@/docs/metering.md): the record a customer may be billed for | ✅ `alloc` |

## Where to start

* **[Getting started](@/docs/getting-started.md)** — a station and a CSMS talking to each other.
* **[Version differences](@/docs/versions.md)** — what actually differs between 1.6, 2.0.1 and
  2.1, and where each difference lives in the code.
* **[Talking to real hardware](@/docs/interop.md)** — the leniency knobs, and why they cost
  nothing when unused.
* **[Signed meter values](@/docs/metering.md)** — the record a customer may actually be
  billed for, and the two shapes its public-key field is sent in.
* **[Testing and conformance](@/docs/testing.md)** — how the crate knows it is right.
* **[Design decisions](@/docs/design.md)** — the trade-offs behind the shape of it, and what
  it deliberately is not.

## Design commitments

**Spec first.** Where an idiom and the specification disagree, the specification wins.
`NotifyPeriodicEventStream` has no response type, so none is invented; 1.6J prints
`OccurenceConstraintViolation` with one `r`, so that is what goes on the wire there.

**Types make invalid states unrepresentable; validation catches the rest.** Every schema
enumeration is a Rust enum. Every constraint the type system cannot express — `maxLength`,
`minItems`, ranges — is checked by a generated `Validate` impl whose output maps onto the OCPP
error codes.

**Sans-I/O by construction.** The protocol state machine consumes events and produces effects.
It never touches a socket or a clock — the driver passes it the current instant, so no deadline
can be armed against a stale one.

**A rule the API can express wrongly will be expressed wrongly.** A `SEND` that cannot be
awaited is a trait bound, not a paragraph; an unconstrained stretch of a composite schedule is
an `Option`, not a convention; and where an option is unsafe *and* silent — accepting an
unsigned frame, authenticating nobody — it is never the default.

**Boring dependencies.** `serde`, `serde_json` and `jiff` by default; `tokio` and `rustls` when
you ask for a transport.
