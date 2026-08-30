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
