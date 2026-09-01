+++
title = "Typed messages"
description = "All 194 OCPP actions as Rust types, generated from the official OCA JSON schemas: open enumerations, separate validation, and direction-aware dispatch."
weight = 20
+++

`v1_6`, `v2_0_1` and `v2_1` hold one Rust type per request and response payload — 39, 64 and 91
actions — generated from the official OCA JSON schemas. The generated source is committed, so
building the crate needs neither the schemas nor a network connection, and a schema change
shows up as a readable diff.

## Payloads

```rust
use ocpp_kit::v2_1;

let request = v2_1::BootNotificationRequest::new(
    v2_1::ChargingStation::new("Model-1", "ACME").with_serial_number("SN-42"),
    v2_1::BootReason::PowerUp,
);
```

Required fields are constructor arguments; optional ones are `with_…` setters. Anything that
converts into a `String` is accepted, and the fields are public, so `..Default::default()`
works where every field is optional.

## Numbers are exact decimals

Every OCPP `number` — a meter register, a charging limit, a tariff price — is a
[`types::Decimal`](https://docs.rs/ocpp-kit/latest/ocpp_kit/types/struct.Decimal.html): a
signed mantissa and a decimal scale, never an `f64`.

```rust
use ocpp_kit::decimal;
use ocpp_kit::types::Decimal;

// The scale the meter wrote is part of what it said, and it survives the round trip.
let register: Decimal = "2935.600".parse().unwrap();
assert_eq!(register.scale(), 3);
assert_eq!(register.to_string(), "2935.600");

// A session's energy is a difference of two registers. In f64 this is 10.000000000000002.
assert_eq!(decimal!(20.2) - decimal!(10.1), decimal!(10.1));
```

The reasons are the same three every time:

* **Resolution is a claim.** A meter reporting `2935.600` kWh is stating three decimals of
  accuracy. As an `f64`, `2935.600` and `2935.6` are the same value and the claim is gone.
* **Energy is a subtraction.** OCPP *defines* a session's energy as the difference of two
  register readings, and binary floating point does not subtract decimals exactly.
* **The 2.1 Tariff and Cost block is money.**

Literals are written with `decimal!(32.5)`, which parses the source text at compile time;
integers convert with `From`. There is no `From<f64>`, deliberately — a number that has been
through a float has already lost what this type exists to keep. The conversions are there when
they are needed, named `to_f64_lossy` and `from_f64_lossy` so the signature says what it
costs. `cargo xtask no-floats` fails the build if an `f32` or `f64` reaches any public
signature in the crate, and CI runs it.

## Enumerations are open

Field devices ship values that are not in the schema, and OCA adds values in errata. A
generated enumeration therefore always parses:

```rust
use ocpp_kit::v2_1::BootReason;

assert_eq!(BootReason::from_wire("PowerUp"), BootReason::PowerUp);
assert_eq!(
    BootReason::from_wire("Levitation"),
    BootReason::UnknownValue("Levitation".into()),
);
```

The parser never fails on an unknown value; the *policy* decides whether it is fatal. By
default it is reported as a `PropertyConstraintViolation`;
[`DecodeOptions::lenient()`](@/docs/interop.md) keeps it. The catch-all is `UnknownValue`
rather than `Unknown` because `Unknown` is itself a defined value of several OCPP
enumerations.

## Validation is separate from serde

Deserialization checks shape. Everything beyond shape — `maxLength`, `minItems`, ranges, closed
enumerations — is a second, explicit pass:

```rust
use ocpp_kit::v2_1;
use ocpp_kit::validate::{Validate, ViolationKind};

let request = v2_1::BootNotificationRequest::new(
    v2_1::ChargingStation::new("a-model-name-that-is-far-too-long", "ACME"),
    v2_1::BootReason::PowerUp,
);

let violations = request.validate().unwrap_err();
let first = violations.first().unwrap();
assert_eq!(first.path, "/chargingStation/model");
assert_eq!(first.kind, ViolationKind::Property);
```

Keeping the two apart is what makes precise error codes possible, keeps the happy path fast,
and lets a local controller relay a message it never fully parsed.

## Dispatch

Each version exposes four direction-aware unions, named after who *originates* the request:

| Union | Direction |
|---|---|
| `CsRequest` | Charging Station → CSMS |
| `CsResponse` | the CSMS's answer to a `CsRequest` |
| `CsmsRequest` | CSMS → Charging Station |
| `CsmsResponse` | the station's answer to a `CsmsRequest` |

```rust
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::{RawValue, v2_1};

let payload = RawValue::from_string(
    r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}"#.into(),
).unwrap();

let request = v2_1::CsRequest::decode(
    v2_1::Action::BootNotification,
    &payload,
    &DecodeOptions::strict(),
).unwrap();
assert_eq!(request.action(), v2_1::Action::BootNotification);
```

Asking `CsRequest` to decode a CSMS-originated action fails with `UnsupportedAction`, which
maps to `NotSupported`. The direction check is free and happens before any payload parsing.

## Naming rules

* `FooEnumType` → `Foo`; `FooType` → `Foo`.
* Unless that name is already taken by an object or by an action, in which case the enum keeps
  its `Enum` — the schema's own `javaType`. Hence `IdTokenEnum` (because `IdTokenType` is an
  object) and `ResetEnum` / `TransactionEventEnum` (because `Reset` and `TransactionEvent` are
  actions).
* Enumeration values keep internal capitalisation: `Energy.Active.Import.Register` becomes
  `EnergyActiveImportRegister`, `L1-N` becomes `L1N`, `SHA256` stays `SHA256`.
* OCPP 1.6's schemas are anonymous — most enumerations and nested objects are inline — so the
  generator carries a table giving each one the name the 1.6 specification uses (`IdTagInfo`,
  `ChargePointErrorCode`, `UnitOfMeasure`, …). It fails on an inline type the table does not
  name, so the table cannot drift.
