+++
title = "Version differences"
description = "The two dozen places where OCPP 1.6J, 2.0.1 and 2.1 genuinely disagree — framing, error-code spelling, authentication, transaction retries, compression — and how each is modelled."
weight = 90
+++

Supporting three versions is mostly not about three sets of types. It is about the places where
the versions genuinely disagree, and where averaging them produces something conformant to none
of them. This page is the list, with where each one lives in the code.

## Framing

| | 1.6J | 2.0.1 | 2.1 | Where |
|---|---|---|---|---|
| Subprotocol | `ocpp1.6` | `ocpp2.0.1` | `ocpp2.1` | `Version::subprotocol` |
| Message types | 2, 3, 4 | 2, 3, 4 | + 5 `CALLRESULTERROR`, 6 `SEND` | `MessageTypeId::is_defined_in` |
| Unknown message type number | ignore the payload (§4.1.3) | answer `MessageTypeNotSupported` (§4.4) | ignore the payload (§4.4) | `FrameError::is_ignorable` |
| Unreadable `MessageId` | no rule | answer with id `"-1"` | answer with id `"-1"` | `MessageId::unreadable` |
| One outstanding `CALL` | `SHOULD NOT` | `SHALL NOT` | `SHALL NOT` | `InboundConcurrency` |
| `SEND` exempt from that rule | – | – | yes (§4.2.4) | `Engine::pump` |

The 2.0.1 → 2.1 change in §4.4 is easy to miss: 2.0.1 requires an *answer* to an unknown
message type, and 2.1 changed it back to silence.

## Error codes

| Meaning | 1.6J | 2.0.1 / 2.1 |
|---|---|---|
| Payload syntactically incorrect | `FormationViolation` | `FormatViolation` |
| Occurrence constraint violated | `OccurenceConstraintViolation` *(one `r`, as printed)* | `OccurrenceConstraintViolation` |
| Not a valid RPC request | *(not defined)* → `GenericError` | `RpcFrameworkError` |
| Unsupported message type number | *(not defined)* → `GenericError` | `MessageTypeNotSupported` |

`ErrorCode::as_wire(version)` produces the right spelling; `ErrorCode::parse` accepts every
spelling on every version, because peers mix them up.

## Security

| | 1.6 (Security Whitepaper ed. 2) | 2.x |
|---|---|---|
| Basic-auth password | `AuthorizationKey`, a **hexadecimal** string; the decoded octets are the password | `BasicAuthPassword`, sent as **UTF-8**, at least 16 characters, ceiling between 40 and 64 (A00.FR.205) |
| Identity must not contain `:` | yes | yes (A00.FR.204) |
| Profiles | 1, 2, 3 | 1, 2, 3 (Part 2 §A 1.3 Table 12) |

`BasicAuthPassword::for_version` builds the right one; getting this wrong is a silent
authentication failure that looks like a network problem.

## Transactions

| | 1.6 | 2.x |
|---|---|---|
| Messages | `StartTransaction`, `MeterValues`, `StopTransaction` | `TransactionEvent` |
| Retry attempts | `TransactionMessageAttempts` | `OCPPCommCtrlr.MessageAttempts[TransactionEvent]` |
| Retry interval | `TransactionMessageRetryInterval` | `MessageAttemptInterval[TransactionEvent]` |
| Schedule | linear: `interval × preceding transmissions` (§3.7.1) | the same |
| Ordering | strictly chronological; later transaction messages wait (§3.7) | the same |
| Sequence numbers | none | `seqNo`, strictly increasing |
| Transaction id | assigned by the **CSMS**, in `StartTransaction.conf` | assigned by the **station**, and unique across reboots (E01.FR.08) |
| Start / stop | implementation-defined ("cable in and authorized", by convention) | `TxStartPoint` / `TxStopPoint`, each a set that behaves as a disjunction (E01, E06) |

Both versions are **linear**, not exponential, and both apply only to transaction-related
messages. `actions::is_transaction_related` is the single place that knows which those are.

The ordering rule is easy to miss and easy to get wrong: 1.6 §3.7 says "the delivery of new
transaction-related messages **SHALL** wait until the queue has been emptied", so a transaction
message waiting out its retry interval blocks every later one — while a message that is *not*
transaction-related is explicitly allowed to overtake the queue, "so that customers are not
kept waiting". The [engine](@/docs/engine.md) implements both halves.

## Compression

2.1 Part 4 §3.4 Table 2 makes RFC 7692 `permessage-deflate` **required** for a CSMS and a Local
Controller, and optional (but recommended) for a Charging Station. 1.6J §5.1 recommends against
compression; 2.0.1 is silent. `Version::supports_compression` reports it, and
[the WebSocket layer](@/docs/websocket.md) implements it.

That one table is why this crate has its own WebSocket implementation: a library that does not
implement RFC 7692 is *required* to reject the `RSV1` bit a compressed message sets, and none
of the general-purpose Rust crates offer a way round it.

## Beyond the schemas

Two catalogues live in Part 2 — Appendices rather than in any schema, and both differ from
1.6's equivalents: the standardized **security events** with their criticality, and the
standardized **components and variables** of the device model, which 1.6 does not have at all
(it has flat configuration keys instead). Both are available as
[`ocpp_kit::standard`](@/docs/catalogues.md), and 1.6's keys as `station::configuration`.

## Schema-level

2.0.1 and 2.1 share 128 schema files by name, but 26 of them changed — across 23 actions,
including `Authorize`, `TransactionEvent`, `SetChargingProfile`, `ReserveNow`, `TriggerMessage`
and `NotifyEVChargingNeeds`. That is why the crate generates **separate type sets per version**
rather than sharing a "2.x" set: sharing would silently mis-validate 2.0.1.

2.1 adds 27 actions over 2.0.1 — `AFRRSignal`, `BatterySwap`, the DER Control block, the Tariff
and Cost block, periodic event streams — and three functional blocks: Q (Bidirectional Power
Transfer), R (DER Control) and S (Battery Swapping).

## 1.6 schema quirks the generator handles

* The 1.6 schemas name almost nothing: enumerations and nested objects are inline. The
  generator carries a table giving each one the name the 1.6 specification itself uses, and
  **fails** if a schema contains an inline type the table does not cover.
* `MeterValues` lists `Hertz` in its unit enumeration and `StopTransaction` does not — a defect
  in the published schema, not a protocol difference. The generated `UnitOfMeasure` is the
  union, and the merge is explicit in the generator rather than accidental.
* `MeterValue.sampledValue` is `minItems: 1` under `MeterValues` and unbounded under
  `StopTransaction`. The tighter bound wins, matching the specification's own cardinality table.
