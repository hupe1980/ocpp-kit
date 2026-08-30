+++
title = "Domain building blocks"
description = "Device model, 1.6 configuration keys, transaction rules, local authorization, composite charging schedules, and an idempotent CSMS transaction ledger."
weight = 70
+++

Components, not a framework. Each one owns a piece of OCPP that is fiddly enough to be worth
getting right once, and none of them assume anything about how the rest of your application is
organised.

## Station-side (`station`)

### Device model

The 2.x device model — components, variables, attributes — with the rules that `GetVariables`,
`SetVariables`, `GetBaseReport` and `GetReport` are defined in terms of, and the
specification's own component catalogue behind
[`declare_standard`](@/docs/catalogues.md).

```rust
use ocpp_kit::station::device_model::{Attribute, DeviceModel, SetStatus, VariableKey};

let mut model = DeviceModel::with_defaults();

assert_eq!(
    model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual, "600"),
    SetStatus::Accepted,
);
// The declared type is enforced …
assert_eq!(
    model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual, "soon"),
    SetStatus::Rejected,
);
// … and so are the declared limits.
assert_eq!(
    model.set("SecurityCtrlr", "SecurityProfile", Attribute::Actual, "9"),
    SetStatus::Rejected,
);

// Variable instances are how `MessageAttempts[TransactionEvent]` is modelled.
let attempts = VariableKey::new("MessageAttempts").instance("TransactionEvent");
assert_eq!(model.get("OCPPCommCtrlr", attempts, Attribute::Actual).unwrap(), "3");
```

A write-only variable — `BasicAuthPassword` — is never read back and never appears with a value
in a report (B07.FR.03). Reports paginate into `NotifyReport`-sized pages with the `seqNo` and
`tbc` that 2.x expects.

**Supported and set are two different facts.** B07.FR.11 requires every *supported* attribute
type to be reported "even if they have no value (are unset)", so the model tracks them apart.
`NotSupportedAttributeType` therefore means the attribute type is unknown for that variable
(B05.FR.06, B06.FR.08) — not that it happens to be empty. Reading a supported-but-unset
attribute gives an empty string, which is B06.FR.13 verbatim: *"this can happen, for example,
when the attributeType Target has not yet been set, even though it is supported."*

```rust
use ocpp_kit::station::device_model::{
    Attribute, DataType, DeviceModel, GetStatus, Mutability, SetStatus, VariableSpec,
};

let mut model = DeviceModel::new();
model.declare(
    "OCPPCommCtrlr",
    VariableSpec::new("HeartbeatInterval", DataType::Integer)
        .mutability(Mutability::ReadWrite)
        .value("300")
        .supports(Attribute::Target),   // supported, no value yet
);

// Supported but unset: an empty string, and writable.
assert_eq!(model.get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Target), Ok(String::new()));
assert_eq!(
    model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Target, "60"),
    SetStatus::Accepted,
);

// Never declared at all: that is what NotSupportedAttributeType is for.
assert_eq!(
    model.get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::MinSet),
    Err(GetStatus::NotSupportedAttributeType),
);
```

### OCPP 1.6 configuration keys

1.6 has no device model; it has a flat list of string-valued keys.

```rust
use ocpp_kit::station::configuration::{ConfigurationKeys, ConfigurationStatus};

let mut config = ConfigurationKeys::with_defaults();
assert_eq!(config.set("HeartbeatInterval", "600"), ConfigurationStatus::Accepted);
// Read-only keys are refused, not silently ignored …
assert_eq!(config.set("NumberOfConnectors", "4"), ConfigurationStatus::Rejected);
// … and a key this station does not implement says so.
assert_eq!(config.set("WhateverKey", "1"), ConfigurationStatus::NotSupported);

// `GetConfiguration` splits its answer into known keys and unknown names.
let (known, unknown) = config.report(&["HeartbeatInterval", "WhateverKey"]);
assert_eq!(known.len(), 1);
assert_eq!(unknown, vec!["WhateverKey".to_string()]);
```

Declare only what the station really implements: the `unknownKey` list is how a CSMS discovers
what a station supports.

### Transaction rules

What starts and stops a transaction in 2.x is not a message but a *condition*. `TxStartPoint`
and `TxStopPoint` each name a **set** of them, and the specification writes one independent
`SHALL` per member — six for starting (E01.FR.01–06), seven for stopping (E06.FR.01–07). The
set is therefore a **disjunction**: the first configured condition to arrive starts the
transaction, the first to disappear ends it.

That is what makes the specification's own recommendation work — *"if time of use is billed,
then the start points should be `EVConnected`, `Authorized` … such that upon authorization
first, the charger is already seen as 'in use'"* — and it is what the E02 sequence diagram
shows: `Started` on cable plug-in, `Updated` when authorization follows.

```rust
use ocpp_kit::station::transactions::{
    Conditions, RandomTransactionIds, TransactionMachine, TxEvent, TxPoint,
};
use ocpp_kit::types::DateTime;
use std::collections::BTreeSet;

// The specification's "time of use" configuration.
let mut machine = TransactionMachine::new(
    BTreeSet::from([TxPoint::EVConnected, TxPoint::Authorized]),
    BTreeSet::from([TxPoint::EVConnected]),
    Box::new(RandomTransactionIds::new()),
);
let now = DateTime::parse("2024-01-01T00:00:00Z").unwrap();

// The cable alone starts it — waiting for authorization too would be a conjunction.
let plugged_in = Conditions { ev_connected: true, ..Conditions::default() };
let events = machine.observe(plugged_in, now);
assert!(matches!(events[0], TxEvent::Started { seq_no: 0, .. }));

// Authorization then follows as an Updated, not a second Started.
let authorized = Conditions { authorized: true, ..plugged_in };
assert!(matches!(machine.observe(authorized, now)[0], TxEvent::Updated { seq_no: 1, .. }));
```

Two subtleties a level-triggered reading gets wrong, and both have consequences:

* **Stopping is a transition, not a level.** E06.FR.02 says a connection "**is lost**", not
  "no connection". The specification's own warning depends on it: with start point
  `ParkingBayOccupancy` and stop point `EVConnected`, "when the user never connects the EV,
  but simply drives away, then the transaction will **remain open**". A condition that never
  held cannot stop holding.
* **`PowerPathClosed` is derived, not reported.** E01.FR.05 defines it as *authorized* **and**
  *connected to the EV*; E06.FR.06 is its exact negation. So it is computed from the other
  two rather than being a seventh boolean you could set inconsistently with them.
  `with_defaults()` uses it, because Table 62 makes it the OCPP 1.6-compatible configuration.

`resume` restores a transaction a reboot interrupted — with the conditions that held when the
station went down, so the stop rule still has a transition to detect.

Transaction ids get the same treatment as message ids: E01.FR.08 wants one "unique for each
transaction started by that Charging Station, even when the Charging Station is rebooted,
repaired, firmware is updated", and §1.2 recommends UUIDs by name. `RandomTransactionIds` is
that; `CounterTransactionIds` is for targets with no entropy source and needs a prefix that
changes on every boot.

### Local authorization

The list, the cache, and the order between them — which is C13.FR.01's: the local list first,
because it is operator-managed and has "priority over Authorization Cache entries for the same
identifiers", then the cache, then the CSMS.

*When* a local answer may be used is a separate question, and it is the one the specification
gives three `Required` variables to answer:

| | offline | online |
|---|---|---|
| Local `Accepted` | used if `LocalAuthorizeOffline` | used if `LocalPreAuthorize`, otherwise the CSMS is asked |
| Local anything else | refuse | **ask the CSMS anyway** |
| Nothing local knows it | `OfflineTxForUnknownIdEnabled` decides | ask the CSMS |

The middle row is the one that surprises people, and it is C10 step 3 verbatim: *"If the
IdToken is not known, **or the IdToken is not Accepted**, the Charging Station sends an
AuthorizeRequest."* A stale `Blocked` in a local list is a reason to ask, not a reason to
refuse — the CSMS is the authority, and it may have changed its mind since the list was last
synchronised.

```rust
use ocpp_kit::station::authorization::{
    AuthorizationPolicy, AuthorizationStatus, Authorizer, Decision, IdTokenInfo, LocalSource,
    UpdateType,
};
use ocpp_kit::types::DateTime;

let mut authorizer =
    Authorizer::new().with_policy(AuthorizationPolicy::default().local_pre_authorize(true));
authorizer.list.update(
    UpdateType::Full,
    1,
    vec![("BLOCKED".into(), Some(IdTokenInfo::new(AuthorizationStatus::Blocked)))],
);
authorizer.remember("CACHED", IdTokenInfo::accepted());

let now = DateTime::parse("2024-06-01T12:00:00Z").unwrap();

// Online, a local refusal is a reason to ask the CSMS (C10).
assert_eq!(authorizer.decide("BLOCKED", true, now), Decision::AskCsms);

// Offline there is nobody to appeal to, so it stands.
assert!(matches!(
    authorizer.decide("BLOCKED", false, now),
    Decision::Local { source: LocalSource::LocalList, .. }
));

// With LocalPreAuthorize, the cache answers while online too (C06).
assert!(matches!(
    authorizer.decide("CACHED", true, now),
    Decision::Local { source: LocalSource::Cache, .. }
));

// And an unknown token offline carries the OfflineTxForUnknownIdEnabled answer with it.
assert_eq!(
    authorizer.decide("NEVER-SEEN", false, now),
    Decision::OfflineUnknown { start_anyway: false }
);
```

A differential `SendLocalList` whose version does not advance is refused
(`UpdateStatus::VersionMismatch`), which is what stops two out-of-order updates from silently
reordering the list.

### Smart charging

Smart charging is the one part of OCPP that hands you a *calculation*.

```rust
use ocpp_kit::station::smart_charging::{
    Period, ProfileKind, ProfileStore, Purpose, RateUnit, Schedule, Profile,
};
use ocpp_kit::types::DateTime;

let start = DateTime::parse("2024-01-01T00:00:00Z").unwrap();
let mut store = ProfileStore::new();

store.install(Profile::new(1, 0, Purpose::TxDefaultProfile, ProfileKind::Absolute,
    Schedule::new(1, RateUnit::A, vec![Period::new(0, 32.0)]).starting(start)));
store.install(Profile::new(2, 0, Purpose::ChargingStationMaxProfile, ProfileKind::Absolute,
    Schedule::new(1, RateUnit::A, vec![Period::new(0, 20.0), Period::new(1800, 40.0)]).starting(start)));

let composite = store.composite(1, start, 3600, RateUnit::A, None);
// First the station ceiling binds, then the session profile does.
assert_eq!(composite.periods[0].limit, Some(20.0));
assert_eq!(composite.periods[1].limit, Some(32.0));
```

A step's `limit` is an `Option`, and `None` marks a stretch that *no* installed profile
constrains — carrying the previous step's number through would report a limit nobody
configured, for exactly the stretch in which the EVSE may draw its rated maximum.
`GetCompositeSchedule` has to name a number, so `CompositeSchedule::fill_gaps(rated_maximum)`
supplies one at the boundary, where the caller knows what the hardware can deliver.

The rules are Part 2 §3.5 and §3.6, which say different things and are easy to conflate:

* **Within a purpose**, the *leading* schedule is the one that "has a schedule period defined
  for that time and … belongs to a charging profile with the highest stack level **that is
  valid at that time**". Both qualifications are part of the selection: a stack level 3
  holiday exception that is outside its validity window does not shadow the stack level 2
  weekly default it was layered on — it is simply not leading there.
* **Across purposes**, the composite is "the lowest charging limit … among the leading
  profiles of the different purposes". `ChargingStationMaxProfile` and
  `ChargingStationExternalConstraints` are two of those purposes, not post-hoc ceilings.
* A `TxProfile` **replaces** the `TxDefaultProfile`, and `PriorityCharging` overrules both.
* `LocalGeneration` is **added on top** of the result — it is capacity the site produces, and
  minimising with it would turn a solar array into a cap.

Recurring profiles repeat daily or weekly, validity windows bound a profile, and limits
convert between amperes and watts from the supply parameters.

## CSMS-side (`csms`)

### The idempotent ledger

A station may legitimately send the same `TransactionEvent` twice: it timed out, retried, and
the first copy arrived after all. A CSMS that treats the second copy as new double-bills.

```rust
use ocpp_kit::csms::ledger::{EventKind, Ingested, Ledger, TransactionEvent};
use ocpp_kit::types::{DateTime, Identity};

let station = Identity::new("CS-0001").unwrap();
let now = DateTime::parse("2024-01-01T00:00:00Z").unwrap();
let mut ledger = Ledger::new();

let started = TransactionEvent::new(station.clone(), "tx-1", 0, EventKind::Started, now);
let updated = TransactionEvent::new(station, "tx-1", 1, EventKind::Updated, now);

assert_eq!(ledger.ingest(&started), Ingested::Applied);
assert_eq!(ledger.ingest(&updated), Ingested::Applied);
assert_eq!(ledger.ingest(&updated), Ingested::Duplicate);   // the station retried
```

It also detects gaps — an offline period whose queue overflowed shows up as
`AppliedWithGap { missing }` — and folds 1.6's `StartTransaction` / `MeterValues` /
`StopTransaction` into the same model through `ingest_unsequenced`.

### Version-agnostic events

A CSMS that supports three versions does not want three copies of its business logic.

```rust,no_run
use ocpp_kit::csms::events::{DomainEvent, observe_v16, observe_v21};
use ocpp_kit::{v1_6, v2_1};

# fn example(legacy: &v1_6::CsRequest, modern: &v2_1::CsRequest) {
let a = observe_v16(legacy);
let b = observe_v21(modern);
// Both produce a `DomainEvent::Booted { vendor, model, .. }`.
# let _ = (a, b);
# }
```

The model is **deliberately lossy** — it keeps what almost every CSMS needs and drops the rest
— which is why every conversion also reports which version produced it, so you can reach for
the typed original whenever the detail matters. `to_ledger_event` bridges straight into the
ledger.
