//! Controlling a charging station: what it draws per phase, and what it may draw.
//!
//! The telemetry half is checked across versions — the same physical reading from a 1.6 and a
//! 2.1 station must read the same — and the control half against the OCA schemas: every
//! `SetChargingProfileRequest` the conversions produce is one a station's validator accepts.

#![cfg(all(
    feature = "csms",
    feature = "v1_6",
    feature = "v2_0_1",
    feature = "v2_1"
))]

mod support;

use std::time::Duration;

use ocpp_kit::csms::events::{WarningKind, observe_v16, observe_v21};
use ocpp_kit::csms::telemetry::{Location, Total, Unit, measurand};
use ocpp_kit::smart_charging::{
    Ceiling, ChargingPlan, ConversionError, Period, Profile, ProfileKind, Purpose, RateUnit,
    RateUnits, Recurrency, Schedule, StationLimits, Supply, Target,
};
use ocpp_kit::types::{DateTime, Decimal};
use ocpp_kit::{Version, v1_6, v2_0_1, v2_1};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

fn v16(json: &str) -> ocpp_kit::csms::events::Observed {
    let request: v1_6::MeterValuesRequest = serde_json::from_str(json).unwrap();
    observe_v16(&v1_6::CsRequest::MeterValues(request))
}

fn v21(json: &str) -> ocpp_kit::csms::events::Observed {
    let request: v2_1::MeterValuesRequest = serde_json::from_str(json).unwrap();
    observe_v21(&v2_1::CsRequest::MeterValues(request))
}

/// One physical state — 16 A, 16 A, 0.2 A, 7.4 kW total, 231 V on line 2 — as a 1.6 station
/// (strings, `kW`, `Lx-N` for power) and a 2.1 station (numbers, `multiplier`, no unit) send it.
#[test]
fn the_same_reading_reads_the_same_from_16_and_21() {
    let from_16 = v16(
        r#"{"connectorId":1,"meterValue":[{"timestamp":"2026-11-20T12:00:00Z","sampledValue":[
            {"value":"16","measurand":"Current.Import","phase":"L1","unit":"A"},
            {"value":"16","measurand":"Current.Import","phase":"L2","unit":"A"},
            {"value":"0.2","measurand":"Current.Import","phase":"L3","unit":"A"},
            {"value":"7.4","measurand":"Power.Active.Import","unit":"kW"},
            {"value":"231","measurand":"Voltage","phase":"L2-N","unit":"V"}
        ]}]}"#,
    );
    let from_21 = v21(
        r#"{"evseId":1,"meterValue":[{"timestamp":"2026-11-20T12:00:00Z","sampledValue":[
            {"value":16,"measurand":"Current.Import","phase":"L1"},
            {"value":16,"measurand":"Current.Import","phase":"L2"},
            {"value":0.2,"measurand":"Current.Import","phase":"L3"},
            {"value":74,"measurand":"Power.Active.Import","unitOfMeasure":{"multiplier":2}},
            {"value":231,"measurand":"Voltage","phase":"L2-N"}
        ]}]}"#,
    );
    for observed in [&from_16, &from_21] {
        assert!(observed.warnings.is_empty(), "{:?}", observed.warnings);
        let m = observed.event.measurements();
        let current = m.per_phase(measurand::CURRENT_IMPORT);
        assert_eq!(
            current.lines,
            [
                Some(Decimal::from(16)),
                Some(Decimal::from(16)),
                Some(Decimal::from_literal("0.2"))
            ]
        );
        assert_eq!(current.total, None, "currents are never summed");
        assert_eq!(current.unit, Some(Unit::A));
        assert_eq!(current.phases_in_use(Decimal::from(1)), Some(2));

        let power = m.per_phase(measurand::POWER_ACTIVE_IMPORT);
        assert_eq!(power.total, Some(Total::Reported(Decimal::from(7400))));
        assert_eq!(power.unit, Some(Unit::W));

        let voltage = m.per_phase(measurand::VOLTAGE);
        assert_eq!(voltage.lines[1], Some(Decimal::from(231)), "L2-N is line 2");
    }
    assert_eq!(
        from_16
            .event
            .measurements()
            .per_phase(measurand::CURRENT_IMPORT),
        from_21
            .event
            .measurements()
            .per_phase(measurand::CURRENT_IMPORT)
    );
}

#[test]
fn phase_powers_add_up_and_line_to_line_is_no_phase() {
    let observed = v21(
        r#"{"evseId":1,"meterValue":[{"timestamp":"2026-11-20T12:00:00Z","sampledValue":[
            {"value":3680,"measurand":"Power.Active.Import","phase":"L1-N","unitOfMeasure":{"unit":"W"}},
            {"value":3680,"measurand":"Power.Active.Import","phase":"L2-N","unitOfMeasure":{"unit":"W"}},
            {"value":40,"measurand":"Power.Active.Import","phase":"L3-N","unitOfMeasure":{"unit":"W"}},
            {"value":400,"measurand":"Voltage","phase":"L1-L2"}
        ]}]}"#,
    );
    let m = observed.event.measurements();
    assert_eq!(
        m.per_phase(measurand::POWER_ACTIVE_IMPORT).total,
        Some(Total::Summed(Decimal::from(7400)))
    );
    let voltage = m.per_phase(measurand::VOLTAGE);
    assert_eq!(
        voltage.lines,
        [None, None, None],
        "L1-L2 is not a per-phase value"
    );
    assert_eq!(m.len(), 4, "but it is still carried");
}

#[test]
fn defaults_apply_and_the_outlet_is_what_the_ev_gets() {
    let observed = v21(
        r#"{"evseId":1,"meterValue":[{"timestamp":"2026-11-20T12:00:00Z","sampledValue":[
            {"value":11040,"measurand":"Power.Active.Import","location":"Inlet"},
            {"value":7360,"measurand":"Power.Active.Import"},
            {"value":1500.5}
        ]}]}"#,
    );
    let m = observed.event.measurements();
    assert_eq!(
        m.per_phase(measurand::POWER_ACTIVE_IMPORT)
            .total
            .map(Total::value),
        Some(Decimal::from(7360))
    );
    assert_eq!(
        m.per_phase_at(measurand::POWER_ACTIVE_IMPORT, &Location::Inlet)
            .total
            .map(Total::value),
        Some(Decimal::from(11040))
    );
    let register = &m[2];
    assert_eq!(register.measurand, measurand::ENERGY_ACTIVE_IMPORT_REGISTER);
    assert_eq!(register.context, "Sample.Periodic");
    assert_eq!(register.location, Location::Outlet);
    assert_eq!(register.unit, Unit::Wh);
}

#[test]
fn an_unknown_unit_is_kept_and_reported_never_guessed() {
    let observed = v21(
        r#"{"evseId":1,"meterValue":[{"timestamp":"2026-11-20T12:00:00Z","sampledValue":[
            {"value":7,"measurand":"Power.Active.Import","unitOfMeasure":{"unit":"horsepower"}}
        ]}]}"#,
    );
    let measurement = &observed.event.measurements()[0];
    assert_eq!(measurement.unit, Unit::Other("horsepower".into()));
    assert_eq!(measurement.value, Decimal::from(7));
    assert_eq!(observed.warnings[0].kind, WarningKind::UnknownUnit);
}

/// A 1.6 value is any string. One that is not a number is a warning — rendered on one line.
#[test]
fn a_station_cannot_forge_a_log_line_through_a_warning() {
    let observed = v16(
        r#"{"connectorId":1,"meterValue":[{"timestamp":"2026-11-20T12:00:00Z","sampledValue":[
            {"value":"12\nFAKE ERROR admin logged in","measurand":"Current.Import"}
        ]}]}"#,
    );
    let line = observed.warnings[0].to_string();
    assert!(!line.contains('\n'), "{line}");
    assert!(line.contains(r"12\nFAKE"), "{line}");
}

// ---------------------------------------------------------------------------
// Profiles across the wire
// ---------------------------------------------------------------------------

fn schema_check(version: Version, request: &impl serde::Serialize) {
    let path = support::schema_path(version, "SetChargingProfile", false).unwrap();
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let instance = serde_json::to_value(request).unwrap();
    let problems = support::validate(&schema, &instance);
    assert!(problems.is_empty(), "{version}: {problems:?}\n{instance}");
}

fn sample_profile() -> Profile {
    let start: DateTime = "2026-11-20T06:00:00Z".parse().unwrap();
    let schedule = Schedule::new(
        7,
        RateUnit::A,
        vec![
            Period::new(0, Decimal::from(16)).phases(3),
            Period::new(1800, Decimal::from_literal("10.5")).phases(1),
        ],
    )
    .starting(start)
    .lasting(86_400);
    let mut profile = Profile::new(
        7,
        2,
        Purpose::TxDefaultProfile,
        ProfileKind::Recurring,
        schedule,
    )
    .on_evse(1)
    .recurring(Recurrency::Daily)
    .valid(Some(start), None);
    profile.schedules[0].min_charging_rate = Some(Decimal::from(6));
    profile
}

#[test]
fn a_profile_round_trips_through_every_version_and_validates() {
    let profile = sample_profile();

    let request = profile.set_request_v1_6().unwrap();
    schema_check(Version::V1_6, &request);
    assert_eq!(request.connector_id, 1);
    assert_eq!(
        Profile::from_v1_6(&request.cs_charging_profiles, 1).unwrap(),
        profile
    );

    let request = profile.set_request_v2_0_1().unwrap();
    schema_check(Version::V2_0_1, &request);
    assert_eq!(
        Profile::from_v2_0_1(&request.charging_profile, 1).unwrap(),
        profile
    );

    let request = profile.set_request_v2_1().unwrap();
    schema_check(Version::V2_1, &request);
    assert_eq!(
        Profile::from_v2_1(&request.charging_profile, 1).unwrap(),
        profile
    );
}

/// The whole path a controller takes: a plan, the station's own limits, the wire.
#[test]
fn a_plan_becomes_a_request_every_version_accepts() {
    let start: DateTime = "2026-11-20T00:00:00Z".parse().unwrap();
    let ceilings = (0..96).map(|slot| Ceiling::watts(Decimal::from(4_000 + 100 * (slot % 7))));
    let plan = ChargingPlan::new(start, Duration::from_secs(900), ceilings).unwrap();
    let limits = StationLimits::new(10, 3, RateUnits::parse_v16("Current,Power"));
    let planned = plan
        .to_profile(
            1,
            1,
            Target::transaction(1, "4711"),
            &limits,
            Supply::default(),
        )
        .unwrap();
    assert!(planned.coarsened);
    assert!(planned.profile.schedules[0].periods.len() <= 10);
    schema_check(Version::V1_6, &planned.profile.set_request_v1_6().unwrap());
    schema_check(
        Version::V2_0_1,
        &planned.profile.set_request_v2_0_1().unwrap(),
    );
    schema_check(Version::V2_1, &planned.profile.set_request_v2_1().unwrap());
}

#[test]
fn what_a_version_cannot_say_is_refused() {
    let tx = sample_profile().for_transaction("abc");
    assert_eq!(
        tx.to_v1_6(),
        Err(ConversionError::TransactionIdNotInteger("abc".into()))
    );
    assert!(tx.to_v2_0_1().is_ok(), "2.x transaction ids are strings");

    let mut priority = sample_profile();
    priority.purpose = Purpose::PriorityCharging;
    for result in [priority.to_v1_6().err(), priority.to_v2_0_1().err()] {
        assert!(
            matches!(result, Some(ConversionError::PurposeNotInVersion { .. })),
            "{result:?}"
        );
    }
    assert!(priority.to_v2_1().is_ok());

    let mut phase_to_use = sample_profile();
    phase_to_use.schedules[0].periods[1].phase_to_use = Some(2);
    assert_eq!(
        phase_to_use.to_v1_6(),
        Err(ConversionError::UnmodelledField {
            field: "phaseToUse",
            period: Some(1)
        })
    );
}

#[test]
fn a_21_profile_that_is_not_a_plain_ceiling_is_refused() {
    let base = sample_profile().to_v2_1().unwrap();

    let mut per_phase = base.clone();
    per_phase.charging_schedule[0].charging_schedule_period[0].limit_l2 = Some(Decimal::from(10));
    assert_eq!(
        Profile::from_v2_1(&per_phase, 1),
        Err(ConversionError::UnmodelledField {
            field: "limit_L2",
            period: Some(0)
        })
    );

    let mut setpoint = base.clone();
    let period = &mut setpoint.charging_schedule[0].charging_schedule_period[1];
    period.limit = None;
    period.setpoint = Some(Decimal::from(-3000));
    period.operation_mode = Some(v2_1::OperationMode::CentralSetpoint);
    assert!(matches!(
        Profile::from_v2_1(&setpoint, 1),
        Err(ConversionError::UnmodelledField {
            period: Some(1),
            ..
        })
    ));

    let mut no_limit = base.clone();
    no_limit.charging_schedule[0].charging_schedule_period[1].limit = None;
    assert_eq!(
        Profile::from_v2_1(&no_limit, 1),
        Err(ConversionError::PeriodWithoutLimit { period: 1 })
    );

    let mut dynamic = base.clone();
    dynamic.charging_profile_kind = v2_1::ChargingProfileKind::Dynamic;
    assert_eq!(
        Profile::from_v2_1(&dynamic, 1),
        Err(ConversionError::UnmodelledKind("Dynamic"))
    );

    let mut local_time = base;
    local_time.charging_schedule[0].use_local_time = Some(true);
    assert!(matches!(
        Profile::from_v2_1(&local_time, 1),
        Err(ConversionError::UnmodelledField {
            field: "useLocalTime",
            ..
        })
    ));

    // 2.0.1 has nothing beyond a plain ceiling to refuse.
    let wire: v2_0_1::ChargingProfile = sample_profile().to_v2_0_1().unwrap();
    assert!(Profile::from_v2_0_1(&wire, 1).is_ok());
}

// ---------------------------------------------------------------------------
// Bidirectional charging (2.1 block Q)
// ---------------------------------------------------------------------------

fn schema_check_action(action: &str, response: bool, message: &impl serde::Serialize) {
    let path = support::schema_path(Version::V2_1, action, response).unwrap();
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let instance = serde_json::to_value(message).unwrap();
    let problems = support::validate(&schema, &instance);
    assert!(problems.is_empty(), "{action}: {problems:?}\n{instance}");
}

/// Every message the V2X builders produce is one a 2.1 station's validator accepts — with the
/// signs block Q wants, which the schema enforces (`dischargeLimit` ≤ 0).
#[test]
fn every_v2x_message_validates_against_the_21_schemas() {
    use ocpp_kit::smart_charging::v2x::{
        DynamicSetpoint, Operation, OperationModes, Setpoint, V2xPlan, V2xStation,
    };
    let station = V2xStation::new(
        StationLimits::new(24, 3, RateUnits::BOTH),
        OperationModes::parse(
            "ChargingOnly,CentralSetpoint,ExternalSetpoint,LocalLoadBalancing,Idle",
        ),
    );
    let w = Decimal::from;
    let plan = V2xPlan::new(
        "2026-11-20T00:00:00Z".parse().unwrap(),
        Duration::from_secs(3600),
        [
            Operation::charging_only(w(11_000)).unwrap(),
            Operation::central_setpoint(Setpoint::charge(w(3_700)).unwrap()),
            Operation::idle().sleep_evse().unwrap(),
            Operation::central_setpoint(Setpoint::discharge(w(5_000)).unwrap())
                .discharge_at_most(w(6_000))
                .unwrap()
                .charge_at_most(w(11_000))
                .unwrap(),
            Operation::external_setpoint()
                .discharge_at_most(w(4_000))
                .unwrap(),
            Operation::local_load_balancing()
                .charge_at_most(w(7_000))
                .unwrap(),
        ],
    )
    .unwrap();
    let request = plan
        .to_request(3, 1, Target::transaction(1, "tx-1"), RateUnit::W, &station)
        .unwrap();
    schema_check_action("SetChargingProfile", false, &request);

    let mut follow = DynamicSetpoint::new(
        4,
        Target::EvseDefault { evse_id: 1 },
        RateUnit::W,
        Operation::central_setpoint(Setpoint::ZERO)
            .discharge_at_most(w(7_000))
            .unwrap(),
        Duration::from_secs(30),
    )
    .unwrap()
    .pull_every(Duration::from_secs(10))
    .unwrap();
    schema_check_action(
        "SetChargingProfile",
        false,
        &follow.to_request(&station).unwrap(),
    );
    let update = follow
        .update_setpoint(Setpoint::discharge(w(2_400)).unwrap())
        .unwrap();
    schema_check_action("UpdateDynamicSchedule", false, &update);
    schema_check_action("PullDynamicScheduleUpdate", true, &follow.pull_response());

    // A domain `Profile` still refuses a V2X period: it is a ceiling schedule.
    assert!(Profile::from_v2_1(&request.charging_profile, 1).is_err());
}

/// The specification's own DER examples (R04 scenario #1, R01) pass `der::check` and the 2.1
/// schema alike, and a request `check` refuses is still schema-valid — the check catches what
/// the schema cannot.
#[test]
fn der_controls_the_check_accepts_are_schema_valid_and_it_refuses_what_the_schema_cannot() {
    use ocpp_kit::smart_charging::der::{DerRule, check};

    let accepted = [
        r#"{"isDefault":true,"controlId":"droop","controlType":"FreqDroop","freqDroop":{
            "priority":0,"overFreq":50.2,"underFreq":49.8,"overDroop":5,"underDroop":5,
            "responseTime":1}}"#,
        r#"{"isDefault":true,"controlId":"vw","controlType":"VoltWatt","curve":{"priority":0,
            "yUnit":"PctMaxW","curveData":[{"x":106,"y":100},{"x":110,"y":0}]}}"#,
        r#"{"isDefault":true,"controlId":"lmd","controlType":"LimitMaxDischarge",
            "limitMaxDischarge":{"priority":0,"pctMaxDischargePower":50.0}}"#,
    ];
    for json in accepted {
        let request: v2_1::SetDERControlRequest = serde_json::from_str(json).unwrap();
        assert_eq!(check(&request), Ok(()), "{json}");
        schema_check_action("SetDERControl", false, &request);
    }

    let refused: v2_1::SetDERControlRequest = serde_json::from_str(
        r#"{"isDefault":true,"controlId":"vw","controlType":"VoltWatt","curve":{"priority":0,
            "yUnit":"PctMaxVar","curveData":[{"x":106,"y":100}]}}"#,
    )
    .unwrap();
    schema_check_action("SetDERControl", false, &refused);
    assert!(matches!(check(&refused), Err(DerRule::WrongYUnit { .. })));
}
