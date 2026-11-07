+++
title = "Talking to real hardware"
description = "Strict by default, lenient where a fleet needs it: a bounded repair loop for malformed payloads, per-station quirk profiles, and hard bounds on everything that can grow."
weight = 100
+++

The strict default is exactly what the specification requires. That is the right default, and
it is the wrong thing to point at a fleet that has been in the field for eight years.

`DecodeOptions` is the dial.

```rust
use ocpp_kit::decode::DecodeOptions;

let strict = DecodeOptions::strict();     // the default: what the schemas say
let pedantic = DecodeOptions::pedantic(); // also rejects members the schema does not define
let lenient = DecodeOptions::lenient();   // what field devices actually send
```

## What leniency covers

| Knob | Strict | Lenient |
|---|---|---|
| `unknown_enum_values` | `Reject` → `PropertyConstraintViolation` | `Preserve` → the enum's `UnknownValue` variant |
| `unknown_fields` | `Ignore` | `Ignore` (`pedantic()` rejects) |
| `datetime` | offset required | a missing offset means UTC; a space may replace `T` |
| `numeric_strings` | `Reject` | `"42"` is coerced where a number belongs |
| `whole_floats` | `Reject` | `20000.0` is accepted where a **1.6 core** schema says `integer` |

`20000.0` for an `integer` is not leniency in 2.0.1 and 2.1: their schemas are JSON Schema
draft-06, where any number with a zero fractional part *is* an integer, so every mode accepts
it there. Only the OCPP 1.6 core schemas are draft-04, which says otherwise. A real fraction
(`20000.5`) is a `TypeConstraintViolation` in every mode.

Each knob has a `with_*` setter, so a policy can start from a preset and change one thing:

```rust
use ocpp_kit::decode::{DecodeOptions, UnknownEnumValues};

let options = DecodeOptions::strict().with_unknown_enum_values(UnknownEnumValues::Preserve);
assert_eq!(options.unknown_enum_values, UnknownEnumValues::Preserve);
```

```rust
use ocpp_kit::decode::{DecodeOptions, decode_payload};
use ocpp_kit::v2_1;
use ocpp_kit::RawValue;

let json = r#"{"timestamp":"2024-01-01 10:00:00","eventType":"Started","seqNo":"7",
               "triggerReason":"Authorized","transactionInfo":{"transactionId":"t1"}}"#;
let payload = RawValue::from_string(json.into()).unwrap();

assert!(decode_payload::<v2_1::TransactionEventRequest>(&payload, &DecodeOptions::strict()).is_err());

let request =
    decode_payload::<v2_1::TransactionEventRequest>(&payload, &DecodeOptions::lenient()).unwrap();
assert_eq!(request.timestamp.to_string(), "2024-01-01T10:00:00Z");
assert_eq!(request.seq_no, 7);
```

## Leniency costs nothing when it is not needed

Leniency is a **bounded repair loop**, not a second parser. The strict parse runs first; only
when it fails is the offending member — identified by its JSON pointer — rewritten and the
parse retried, up to `max_repairs` times. A conforming payload therefore costs exactly one
strict parse, and a payload that cannot be repaired fails with the same precise error it would
have failed with in strict mode.

## Leniency is reported

A lenient decode that had to forgive something says what, and where — so a fleet's
non-conformance is something an operator can count and take back to the vendor.

```rust
use ocpp_kit::decode::{DecodeOptions, RepairKind, decode_payload_reporting};
use ocpp_kit::{RawValue, v2_1};

let payload = RawValue::from_string(
    r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"},"extra":1}"#.into(),
).unwrap();
let decoded =
    decode_payload_reporting::<v2_1::BootNotificationRequest>(&payload, &DecodeOptions::lenient())
        .unwrap();
assert!(decoded.repairs.is_empty()); // conforming (an ignored extra member is not a repair)

let payload = RawValue::from_string(
    r#"{"reason":"SolarFlare","chargingStation":{"model":"M1","vendorName":"ACME"}}"#.into(),
).unwrap();
let decoded =
    decode_payload_reporting::<v2_1::BootNotificationRequest>(&payload, &DecodeOptions::lenient())
        .unwrap();
assert_eq!(decoded.repairs[0].pointer, "/reason");
assert_eq!(decoded.repairs[0].kind, RepairKind::UnknownEnumValue);
```

In a transport handler `ctx.decode` does this for you and publishes the repairs; a CSMS sees
them for its whole fleet as `SessionEvent::Repaired` ([Transport](@/docs/transport.md)).

## Per-station quirk profiles

A CSMS rarely wants one policy for a whole fleet.

```rust,no_run
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::transport::{AcceptEveryStation, Csms};

# fn build() -> Result<(), Box<dyn std::error::Error>> {
let csms = Csms::builder()
    .bind("0.0.0.0:9000".parse()?)
    .authenticate(AcceptEveryStation) // a real one goes here; see the security page
    .decode_options_for(|identity| {
        if identity.as_str().starts_with("LEGACY-") {
            DecodeOptions::lenient()
        } else {
            DecodeOptions::strict()
        }
    })
    .build()?;
# Ok(()) }
```

## Tolerances that are always on

* **An over-long `MessageId` is echoed verbatim.** Truncating it would break correlation. It is
  reported as `ProtocolViolation::NonConformingMessageId` instead.
* **A peer with two calls in flight is served.** 1.6J only says `SHOULD NOT`, and dropping a
  station's calls is usually worse than answering them. `InboundConcurrency::Reject` opts into
  strictness.
* **A binary WebSocket frame is ignored, not fatal.** OCPP-J is text-only, but an unexpected
  frame is not a reason to drop a charging session.
* **A trailing array element is tolerated.** Every version describes the frame as having *at
  least* the elements it lists.

## Bounds

Tolerance is not the same as trust. Everything that can grow is bounded:

* `DecodeOptions::max_payload_size` (1 MiB by default) is checked before parsing.
* `OfflinePolicy::max_queued` bounds the in-memory queue; a full queue fails the call with
  `CallFailure::QueueFull` rather than growing.
* `MemStore::bounded` bounds the durable queue.
* `Csms::max_connections` bounds the session router.
* The leniency repair loop is bounded by `max_repairs`.
