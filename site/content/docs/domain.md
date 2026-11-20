+++
title = "Domain building blocks"
description = "Device model, 1.6 configuration keys, transaction rules, local authorization, smart charging (composite schedules, plans, wire profiles), an idempotent CSMS transaction ledger and phase-aware telemetry."
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

A token is its value **and** its type (C01.FR.25): `IdToken::new("04A2", "ISO14443")` and
`IdToken::new("04A2", "Local")` are two tokens, and a 1.6 `idTag` is `IdToken::tag`. The cache
never stores a `Central` token (C05.FR.02), and with `with_lifetime(AuthCacheLifeTime)` an entry
without its own `cacheExpiryDateTime` expires that long after the CSMS last answered for it.

```rust
use ocpp_kit::station::authorization::{
    AuthorizationPolicy, AuthorizationStatus, Authorizer, Decision, IdToken, IdTokenInfo,
    LocalSource, UpdateType,
};
use ocpp_kit::types::DateTime;

let now = DateTime::parse("2024-06-01T12:00:00Z").unwrap();
let blocked = IdToken::new("BLOCKED", "ISO14443");
let cached = IdToken::new("CACHED", "ISO14443");

let mut authorizer =
    Authorizer::new().with_policy(AuthorizationPolicy::default().local_pre_authorize(true));
authorizer.list.update(
    UpdateType::Full,
    1,
    vec![(blocked.clone(), Some(IdTokenInfo::new(AuthorizationStatus::Blocked)))],
);
authorizer.remember(cached.clone(), IdTokenInfo::accepted(), now);

// Online, a local refusal is a reason to ask the CSMS (C10).
assert_eq!(authorizer.decide(&blocked, true, now), Decision::AskCsms);

// Offline there is nobody to appeal to, so it stands.
assert!(matches!(
    authorizer.decide(&blocked, false, now),
    Decision::Local { source: LocalSource::LocalList, .. }
));

// With LocalPreAuthorize, the cache answers while online too (C06).
assert!(matches!(
    authorizer.decide(&cached, true, now),
    Decision::Local { source: LocalSource::Cache, .. }
));

// The same value under another type is not the same token.
assert_eq!(
    authorizer.decide(&IdToken::new("CACHED", "Local"), false, now),
    Decision::OfflineUnknown { start_anyway: false }
);
```

A differential `SendLocalList` whose version does not advance is refused
(`UpdateStatus::VersionMismatch`), which is what stops two out-of-order updates from silently
reordering the list.

## Smart charging (both sides)

Smart charging is the one part of OCPP that hands you a *calculation*. `smart_charging` needs no
feature and no `std`: a station uses it to apply profiles, a CSMS or a home energy manager to
build them.

```rust
use ocpp_kit::decimal;
use ocpp_kit::smart_charging::{
    Period, ProfileKind, ProfileStore, Purpose, RateUnit, Schedule, Profile,
};
use ocpp_kit::types::DateTime;

let start = DateTime::parse("2024-01-01T00:00:00Z").unwrap();
let mut store = ProfileStore::new();

store.install(Profile::new(1, 0, Purpose::TxDefaultProfile, ProfileKind::Absolute,
    Schedule::new(1, RateUnit::A, vec![Period::new(0, decimal!(32))]).starting(start)));
store.install(Profile::new(2, 0, Purpose::ChargingStationMaxProfile, ProfileKind::Absolute,
    Schedule::new(1, RateUnit::A, vec![Period::new(0, decimal!(20)), Period::new(1800, decimal!(40))]).starting(start)));

let composite = store.composite(1, start, 3600, RateUnit::A, None);
// First the station ceiling binds, then the session profile does.
assert_eq!(composite.periods[0].limit, Some(decimal!(20)));
assert_eq!(composite.periods[1].limit, Some(decimal!(32)));
```

Limits are exact decimals, so two steps merge when they say the *same* number rather than
when they are within an epsilon of each other, and an ampere-to-watt conversion is a decimal
multiplication — 16 A at 230 V on three phases is 11040 W, not 11039.999999999998.

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

### From a plan to a profile

Per-slot ceilings in, one `TxProfile` the station accepts out:

```rust
use std::time::Duration;
use ocpp_kit::smart_charging::{Ceiling, ChargingPlan, RateUnits, StationLimits, Supply, Target};
use ocpp_kit::types::{DateTime, Decimal};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let start: DateTime = "2026-11-20T00:00:00Z".parse()?;
// Tomorrow's 96 quarter hours from the planner, in watts, on three phases.
let ceilings = (0..96).map(|slot| Ceiling::watts(Decimal::from(if slot < 28 { 11_000 } else { 4_200 })).phases(3));
let plan = ChargingPlan::new(start, Duration::from_secs(900), ceilings)?;

// What the station says about itself: PeriodsPerSchedule, ProfileStackLevel, RateUnit.
let limits = StationLimits::new(24, 3, RateUnits::parse_v2("A"));
let planned = plan.to_profile(1, 1, Target::transaction(1, "tx-4711"), &limits, Supply::default())?;

let request = planned.profile.set_request_v2_1()?; // or set_request_v1_6 / _v2_0_1
# let _ = request;
# Ok(()) }
```

* **A ceiling stays a ceiling.** W → A rounds down (7400 W on three phases at 230 V is 10.724 A,
  not 10.725). Too many slots for `PeriodsPerSchedule` merge into the lower limit and fewer phases,
  never truncate; `Planned::coarsened` says so.
* **Block K by construction**: `Absolute`, first period at 0, ascending, one schedule, no
  `phaseToUse`; stack level, rate unit and EVSE are checked (`PlanError`).
* `RateUnits::parse_v16("Current,Power")` / `parse_v2("A,W")` read the station's variable.

Send one profile per plan, not per slot: a 2.1 station may refuse frequent `SetChargingProfile`
(K01.FR.56). A plan holds ceilings only — a floor needs 2.1 setpoints (below).

### Across the wire

`Profile::to_v1_6` / `to_v2_0_1` / `to_v2_1` and `set_request_*` send a profile;
`Profile::from_v1_6(&wire, connector)` / `from_v2_0_1` / `from_v2_1` read one. What a side cannot
carry with its meaning is a `ConversionError`, never a default: a non-integer 1.6 transaction id, a
purpose the version lacks, a 2.1 `Dynamic` profile or a V2X period. Every request validates against
its version's schema.

### Bidirectional charging (2.1)

`smart_charging::v2x` builds 2.1 operation modes and setpoints (block Q) so that each of its traps is
a type or a typed error:

```rust
use std::time::Duration;
use ocpp_kit::smart_charging::v2x::{DynamicSetpoint, Operation, OperationModes, Setpoint, V2xPlan, V2xStation};
use ocpp_kit::smart_charging::{RateUnit, RateUnits, StationLimits, Target};
use ocpp_kit::types::{DateTime, Decimal};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// What the station reported: PeriodsPerSchedule, ProfileStackLevel, RateUnit, and
// V2XChargingCtrlr.SupportedOperationModes.
let station = V2xStation::new(
    StationLimits::new(24, 3, RateUnits::parse_v2("W")),
    OperationModes::parse("ChargingOnly,CentralSetpoint,Idle"),
);
let w = Decimal::from;

// Charge at night, idle in the morning, cover the evening peak from the battery.
let plan = V2xPlan::new("2026-11-20T00:00:00Z".parse::<DateTime>()?, Duration::from_secs(6 * 3600), [
    Operation::central_setpoint(Setpoint::charge(w(3700))?),
    Operation::idle().sleep_evse()?,
    Operation::central_setpoint(Setpoint::discharge(w(5000))?).discharge_at_most(w(6000))?,
    Operation::charging_only(w(11000))?,
])?;
let day = plan.to_request(1, 1, Target::transaction(1, "tx-1"), RateUnit::W, &station)?;

// The one-second arbiter: a Dynamic profile at a higher stack level that the station drops
// 30 s after the last update.
let mut follow = DynamicSetpoint::new(2, Target::transaction(1, "tx-1"), RateUnit::W,
    Operation::central_setpoint(Setpoint::ZERO).discharge_at_most(w(7000))?, Duration::from_secs(30))?
    .stack_level(2);
let install = follow.to_request(&station)?;
let every_second = follow.update_setpoint(Setpoint::discharge(w(2400))?)?;
# let _ = (day, install, every_second);
# Ok(()) }
```

| Trap in block Q | Here |
|---|---|
| Each mode allows different fields (Q §2.2): `ChargingOnly` needs `limit`, `CentralSetpoint` needs `setpoint`, `Idle` forbids all four (Q10.FR.01) | `Operation`'s constructors per mode; a field the mode does not take is `V2xError::NotInMode` |
| A setpoint outside `[dischargeLimit, limit]` is rejected (V2X.05) | checked when the operation is built and on every update |
| `setpoint` is signed, `dischargeLimit` negative, but the EV's `maxDischargePower` is positive | nothing takes a signed discharge: `Setpoint::discharge(w)` and `discharge_at_most(w)` take magnitudes and the wire gets the sign |
| A `Dynamic` profile keeps its last setpoint until its `duration` runs out (K28.FR.13) | `DynamicSetpoint::new` *requires* that fallback — a crashed controller cannot leave the EV discharging |
| A Dynamic profile is one schedule with one period at 0 (K28.FR.01/02); `dynUpdateInterval` only on Dynamic (K28.FR.04) | built that way; `pull_every` sets the interval, `pull_response` answers the station's pull |
| An update carries no `operationMode` | `update` refuses to change the mode: send a new profile |
| A mode the station does not list is rejected (Q09.FR.01) | checked against `SupportedOperationModes`; `OperationModes::MANDATORY` when it reports none |

A V2X plan merges only identical neighbours (setpoints are targets, not ceilings).

### What the EV needs

`EvNeeds` reads `NotifyEVChargingNeedsRequest` from 2.0.1 and 2.1 alike — the AC, DC and V2X
parameter sets, which use different units and phase conventions — into one view:

```rust
use ocpp_kit::decode;
use ocpp_kit::smart_charging::{EvNeeds, Supply};
use ocpp_kit::types::Decimal;
use ocpp_kit::v2_0_1::NotifyEVChargingNeedsRequest;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let request: NotifyEVChargingNeedsRequest = decode::from_str(r#"{"evseId": 1, "chargingNeeds": {
    "requestedEnergyTransfer": "AC_three_phase", "departureTime": "2026-11-21T07:00:00Z",
    "acChargingParameters": {"energyAmount": 20000, "evMinCurrent": 6, "evMaxCurrent": 16, "evMaxVoltage": 400}}}"#)?;
let needs = EvNeeds::from_v2_0_1(&request.charging_needs);
assert_eq!(needs.energy, Some(Decimal::from(20_000)));               // Wh
assert_eq!(needs.phases, Some(3));                                  // from the transfer mode
assert_eq!(needs.max_charge_power(Supply::default()), Some(Decimal::from(11_040)));
# Ok(()) }
```

AC currents are amperes **per phase**; V2X powers are magnitudes in W, summed over `_L2`/`_L3`
when given (the plain field is then L1 only). `clamp_watts` (2.1) fits a setpoint into what the
EV can do and refuses discharge unless the transfer is bidirectional.

### DER control (2.1)

A bidirectional station is a distributed energy resource, and a network operator's grid code
reaches it as DER controls: volt-watt and frequency-watt curves, a frequency droop, trip curves, a
cap on discharge. The station executes them. A planner has to *read* them, because a discharge a
DER control can lower or stop is one it must not count on. `smart_charging::der` collects the
report, applies the precedence rules, and answers that question:

```rust
use ocpp_kit::decode;
use ocpp_kit::smart_charging::der::{AlarmChange, DerAlarmState, DerReport};
use ocpp_kit::types::DateTime;
use ocpp_kit::v2_1::{NotifyDERAlarmRequest, ReportDERControlRequest};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// The station's answer to GetDERControl { requestId: 7 }, in one part.
let part: ReportDERControlRequest = decode::from_str(r#"{"requestId": 7, "curve": [
    {"id": "vw", "curveType": "VoltWatt", "isDefault": true, "isSuperseded": false,
     "curve": {"priority": 0, "yUnit": "PctMaxW", "curveData": [{"x": 106, "y": 100}, {"x": 110, "y": 0}]}},
    {"id": "vv", "curveType": "VoltVar", "isDefault": true, "isSuperseded": false,
     "curve": {"priority": 0, "yUnit": "PctMaxVar", "curveData": [{"x": 93, "y": 30}, {"x": 107, "y": -30}]}}
]}"#)?;
let mut report = DerReport::new(7);
report.add(&part)?;
let controls = report.finish()?;

let now: DateTime = "2026-11-20T17:00:00Z".parse()?;
// VoltWatt can lower active power; VoltVar only takes reactive headroom.
let limiting: Vec<_> = controls.limiting_at(now).iter().map(|c| c.id.clone()).collect();
assert_eq!(limiting, ["vw"]);

// The station says the curve is taking over.
let mut alarm = DerAlarmState::new();
let notice: NotifyDERAlarmRequest = decode::from_str(
    r#"{"controlType": "VoltWatt", "gridEventFault": "OverVoltage", "timestamp": "2026-11-20T17:05:00Z"}"#)?;
assert_eq!(alarm.apply(&notice), AlarmChange::Raised);
# Ok(()) }
```

| Trap in block R | Here |
|---|---|
| Which control is in effect is a rule (§2.9, R04.FR.09–11): a running scheduled control beats the default, the lowest `priority` value wins, a superseded one never runs | `DerControls::effective(kind, t)` / `effective_at(t)` |
| A report arrives in parts with one `requestId`, `tbc` on all but the last (R04.FR.31/32) | `DerReport` refuses a stray part, a part after the last, duplicate ids, a negative duration, and more than 1 024 controls |
| `NotifyDERStartStop` supersedes controls and ends them (R04.FR.20–22) | `DerControls::apply` returns what changed; an unknown id means "ask again" |
| "There are no multiple levels of alarm" (R05.FR.04): a takeover is never ended | `DerAlarmState` holds one alarm; `Replaced(previous)`, `Repeated`, `Cleared`, `Unmatched` |
| A `SetDERControlRequest` carries exactly the field its type maps to, defaults have no schedule, each curve type has its `yUnit` (R04.FR.13/15/17/50–56) | `der::check(&request)` names the rule it breaks |
| `pctMaxDischargePower` is a trip threshold in §2.64 and the cap itself in R01/R02 | carried as reported; no watt figure is computed from it |

Executing the curves needs grid measurements and is the inverter's job, so it is not here. A
control type the crate does not know counts as limiting.

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

### The energy figures

`Record::energy()` is the difference of the station's start and stop registers, computed
exactly. It returns a `RegisterDelta`, not a number: the figure is reached only through
`unverified_wh()` — so the call site says what it is — and `signed()` hands over the signed
records it arrived with. It converts into nothing, so it cannot slide onto an invoice line
unnamed.

```rust
use ocpp_kit::csms::ledger::{EventKind, Ledger, TransactionEvent};
use ocpp_kit::decimal;
use ocpp_kit::types::{DateTime, Identity};

let station = Identity::new("CS-0001").unwrap();
let at = |text: &str| DateTime::parse(text).unwrap();
let mut ledger = Ledger::new();

ledger.ingest(
    &TransactionEvent::new(station.clone(), "tx-1", 0, EventKind::Started, at("2024-01-01T00:00:00Z"))
        .with_meter(decimal!(2935.600)),
);
ledger.ingest(
    &TransactionEvent::new(station.clone(), "tx-1", 1, EventKind::Ended, at("2024-01-01T01:00:00Z"))
        .with_meter(decimal!(2952.100)),
);

let record = ledger.transaction(&station, "tx-1").unwrap();
assert_eq!(record.energy().unwrap().unverified_wh().to_string(), "16.500");
```

Both registers are [exact decimals](@/docs/messages.md), so the subtraction is exact and the
meter's resolution survives it. Unit conversion is exact too: a `kWh` reading and a 2.x
`unitOfMeasure.multiplier` are applied by moving the decimal point, never by multiplying by
`1000.0`.

Exact is not the same as authoritative. The figure is what the *station* said, and the
registers may not even be the quantity you want: in the OCA's own example message a 1.6
`meterStop` is the meter's **lifetime** total while the signed record beside it reports the
session. Where calibration law requires the billable kWh to be traceable to the meter, that
signed record is the basis — carried through untouched as `Record::signed`. See
[signed meter values](@/docs/metering.md).

```rust,no_run
# use ocpp_kit::csms::ledger::Record;
# fn example(record: &Record) {
for reading in record.signed_with_context("Transaction.End") {
    let _ = &reading.value.signed_meter_data;      // verbatim, for your own verification
}
# }
```

`Record::signed` is one list rather than a start and a stop because 1.6 does not give the two
their own messages: a `StartTransaction` has nowhere to carry a signed record, so both records
arrive in `StopTransaction.transactionData`.

### Surviving a restart

A restart is exactly when a station's retries arrive — it timed out because the CSMS went
away — and an in-memory ledger has forgotten the first copy. `DurableLedger` journals every
input to a `LedgerStore` *before* applying it, and rebuilds by replay:

```rust
use ocpp_kit::csms::ledger::{EventKind, Ingested, TransactionEvent};
use ocpp_kit::csms::ledger_store::{DurableLedger, MemLedgerStore};
use ocpp_kit::types::{DateTime, Identity};

let station = Identity::new("CS-0001").unwrap();
let at = DateTime::parse("2026-11-07T10:00:00Z").unwrap();
let started = TransactionEvent::new(station, "tx-1", 0, EventKind::Started, at);

let mut ledger = DurableLedger::open(MemLedgerStore::new()).unwrap();
assert_eq!(ledger.ingest(&started).unwrap(), Ingested::Applied);

// Restart: only the store survives, and it is enough.
let mut ledger = DurableLedger::open(ledger.into_store()).unwrap();
assert_eq!(ledger.ingest(&started).unwrap(), Ingested::Duplicate);
```

`FileLedgerStore::open(dir)` keeps a `journal.jsonl` and a `snapshot.json`, every write
flushed with `sync_data`; `checkpoint()` folds the journal into the snapshot. For a database,
implement `LedgerStore`'s three methods — `load`, `append`, `checkpoint` — there. If `append`
fails nothing is applied: answer the station with an error so it retries, rather than
acknowledging an event a restart would forget.

`Record` also keeps `evse_id` / `connector_id` from the first event that named one, since only
some messages do. It keeps no `charging_state`: occupancy billing needs the *series* of states
over the session, and a scalar would be the wrong shape for it.

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
the typed original whenever the detail matters. `COVERED_ACTIONS` lists what it models.

Each transaction event carries what a CDR needs and a meter reading cannot supply: the
`charging_state` the station reports, and the `evse_id` / `connector_id` the session is at.

```rust,no_run
# use ocpp_kit::csms::events::DomainEvent;
# fn example(event: &DomainEvent) {
if let DomainEvent::TransactionUpdated { charging_state, evse_id, .. } = event {
    // A register sitting at 0.000 kWh is a taper if the station says `Charging` and an
    // occupancy fee if it says `SuspendedEV`. No amount of metering data settles which.
    let _ = (charging_state.as_deref(), evse_id);
}
# }
```

1.6 has connectors and no EVSEs, and the model says so rather than passing one off as the
other: `StartTransaction.connectorId` fills `connector_id` and leaves `evse_id` empty. A CSMS
addressing a 2.x-shaped inventory with a 1.6 connector number would aim at the wrong outlet.

### What a station draws, per phase

Every sampled value, with the specification's defaults applied:

```rust,no_run
use ocpp_kit::csms::telemetry::{measurand, Total};
use ocpp_kit::types::Decimal;
# fn example(observed: &ocpp_kit::csms::events::Observed) {
let measurements = observed.event.measurements();
let current = measurements.per_phase(measurand::CURRENT_IMPORT);
// [Some(16), Some(16), Some(0.2)] — and 2 phases actually in use.
let _ = (current.lines, current.phases_in_use(Decimal::from(1)));

match measurements.per_phase(measurand::POWER_ACTIVE_IMPORT).total {
    Some(Total::Reported(watts)) => println!("{watts} W, as the station said"),
    Some(Total::Summed(watts)) => println!("{watts} W, the sum of its three phases"),
    None => {}
}
# }
```

The rules, all from 2.x §2.87 and 1.6 §7.45:

| | Rule |
|---|---|
| No `phase` | an *overall* value, not phase 1 |
| `L1`, `L1-N` | both line 1; a line-to-line value (`L1-L2`) is not per-phase, `N` is the neutral |
| No `location` | `Outlet` — what the EV gets; `per_phase_at(m, &Location::Inlet)` for the grid side |
| Units | normalized exactly to W, Wh, A, V, var, varh, VA, VAh, Hz, %: `kW`, `kWh` and the 2.x `multiplier` move the decimal point |
| No unit | the measurand's natural one (`Power.*` → W); the schema's literal `Wh` is right for energy only |
| Unknown unit | kept verbatim, unconverted, with a `UnknownUnit` warning |
| Totals | `Reported`, or `Summed` from all three lines — for active/reactive power and energy only, never for currents or voltages |

Each per-phase slot holds the latest sample; `measurand::*` names the common measurands.

### Into the ledger

`to_ledger_event` is the only bridge, so a CSMS supporting 1.6 and 2.x writes one adapter
rather than two. Three 1.6 asymmetries live behind it:

* It has no transaction-event message, so a mid-transaction reading arrives as `MeterValues`
  naming a `transactionId`. That folds into an `Updated` event, which is what makes
  `StartTransaction` / `MeterValues` / `StopTransaction` genuinely one shape. A 2.x
  `MeterValues` names no transaction and stays out, because it is not part of one.
* It hands the transaction id back in `StartTransaction.conf`, so a start event carries none
  and `to_ledger_event` returns `None`. `to_ledger_event_with_id` takes the id the handler
  assigned. Skipping it loses the *start register*, and the first periodic `MeterValues`
  quietly takes its place — billing the session from a reading taken minutes after charging
  began.
* Where an event carries several readings, which one it means depends on which end of the
  transaction it is: an `Ended` event prefers the `Transaction.End` sample and falls back to
  the last, a `Started` event prefers `Transaction.Begin` and falls back to the first.

### Warnings

Most of what the model drops is detail a CSMS does not need. Some of it is not:

```rust,no_run
# use ocpp_kit::csms::events::Observed;
# fn example(observed: &Observed) {
for warning in &observed.warnings {
    // "unreadable signed data: not a SignedMeterValue document: expected value at line 1 …"
    eprintln!("{warning}");
}
# }
```

`WarningKind` covers unreadable signed data, an unreadable 1.6 reading of any measurand, an
energy register in a unit that is not an energy unit, any other measurand in a unit the kit does
not know, and a value out of range. A warning's `Display` is one line: its detail is whatever the
station sent, with control characters escaped. Each is a station saying something
malformed about a value that decides money, and each otherwise fails silently: a station that
*claims* to send signed meter data and sends something unparseable looks, through the funnel
alone, exactly like one sending none.

No schema catches them. 1.6 types a sampled value as a plain `string` with no numeric
constraint, so `"L"` is a *conforming* reading; 2.x puts no bound on
`unitOfMeasure.multiplier`, so `10¹⁰⁹` conforms too. Both are schema-valid messages no CSMS
can bill, which is why the funnel reports them and the validator cannot.
