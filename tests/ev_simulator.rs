//! The simulated wallbox, from the CSMS's side of the wire.
//!
//! Every message the wallbox sends is checked three ways: against the OCA schema, through the
//! strict decoder a CSMS's handler uses (`CsRequest::decode`), and through the CSMS's own reading
//! of it (`csms::events`, `csms::telemetry`, `EvNeeds`). A simulator a CSMS passes against but a
//! station would not behave like is worse than none.

mod support;

use std::time::Duration;

use ocpp_kit::csms::events::{DomainEvent, observe_v16, observe_v21, observe_v201};
use ocpp_kit::csms::telemetry::{Total, measurand};
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::rpc::ErrorCode;
use ocpp_kit::smart_charging::{EvNeeds, Supply};
use ocpp_kit::testkit::{Ev, EvState, Outgoing, PhaseSwitching, Wallbox};
use ocpp_kit::types::{DateTime, Decimal};
use ocpp_kit::{RawValue, Version, v1_6, v2_0_1, v2_1};
use serde_json::Value;

fn start() -> DateTime {
    "2026-11-20T18:00:00Z".parse().unwrap()
}

fn d(value: i64) -> Decimal {
    Decimal::from(value)
}

/// Everything the CSMS can check about one message, and what it understood.
fn received(version: Version, out: &Outgoing) -> DomainEvent {
    let path = support::schema_path(version, out.action, false).unwrap();
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let instance: Value = serde_json::from_str(&out.payload).unwrap();
    let problems = support::validate(&schema, &instance);
    assert!(
        problems.is_empty(),
        "{} {}: {problems:?}",
        out.action,
        out.payload
    );

    let raw = RawValue::from_string(out.payload.clone()).unwrap();
    let strict = DecodeOptions::strict();
    match version {
        Version::V1_6 => {
            let action = v1_6::Action::from_wire(out.action).unwrap();
            observe_v16(&v1_6::CsRequest::decode(action, &raw, &strict).unwrap()).event
        }
        Version::V2_0_1 => {
            let action = v2_0_1::Action::from_wire(out.action).unwrap();
            observe_v201(&v2_0_1::CsRequest::decode(action, &raw, &strict).unwrap()).event
        }
        _ => {
            let action = v2_1::Action::from_wire(out.action).unwrap();
            observe_v21(&v2_1::CsRequest::decode(action, &raw, &strict).unwrap()).event
        }
    }
}

/// Every message checked; the `seqNo`s of the transaction events, in order.
fn check_all(version: Version, sent: &[Outgoing]) -> Vec<i32> {
    let mut seq = Vec::new();
    for out in sent {
        let event = received(version, out);
        if out.action == "TransactionEvent" {
            match event {
                DomainEvent::TransactionStarted { seq_no, .. }
                | DomainEvent::TransactionUpdated { seq_no, .. }
                | DomainEvent::TransactionEnded { seq_no, .. } => seq.push(seq_no.unwrap()),
                other => panic!("a TransactionEvent read as {other:?}"),
            }
        }
    }
    seq
}

fn actions(sent: &[Outgoing]) -> Vec<&'static str> {
    sent.iter().map(|o| o.action).collect()
}

fn tx_default_6a(version: Version) -> &'static str {
    match version {
        Version::V1_6 => {
            r#"{"connectorId":1,"csChargingProfiles":{"chargingProfileId":1,"stackLevel":0,
                "chargingProfilePurpose":"TxDefaultProfile","chargingProfileKind":"Relative",
                "chargingSchedule":{"chargingRateUnit":"A","chargingSchedulePeriod":[
                {"startPeriod":0,"limit":6}]}}}"#
        }
        _ => {
            r#"{"evseId":1,"chargingProfile":{"id":1,"stackLevel":0,
                "chargingProfilePurpose":"TxDefaultProfile","chargingProfileKind":"Relative",
                "chargingSchedule":[{"id":1,"chargingRateUnit":"A","chargingSchedulePeriod":[
                {"startPeriod":0,"limit":6}]}]}}"#
        }
    }
}

/// A plug, a profile, an hour, an unplug: every message valid, the profile followed, the
/// telemetry reading back what the vehicle drew.
fn a_session(version: Version) {
    let mut wallbox = Wallbox::new(version, start());
    let mut sent = wallbox
        .plug_in(
            Ev::builder()
                .soc(30)
                .reaction(Duration::from_secs(2))
                .build(),
        )
        .unwrap();
    if version == Version::V1_6 {
        sent.extend(wallbox.answered(
            "StartTransaction",
            r#"{"transactionId":42,"idTagInfo":{"status":"Accepted"}}"#,
        ));
    }
    let answer = wallbox
        .handle("SetChargingProfile", tx_default_6a(version))
        .unwrap()
        .unwrap();
    assert!(answer.contains("Accepted"), "{answer}");
    sent.extend(wallbox.advance(Duration::from_secs(3600)));
    assert_eq!(wallbox.ev().unwrap().reading().current, d(6));

    // The last periodic sample, read the way a CSMS reads it.
    let last = sent
        .iter()
        .rev()
        .find(|o| o.action == "MeterValues" || o.payload.contains("MeterValuePeriodic"))
        .unwrap();
    let event = received(version, last);
    let currents = event.measurements().per_phase(measurand::CURRENT_IMPORT);
    assert_eq!(
        currents.lines,
        [Some(d(6)), Some(d(6)), Some(d(6))],
        "{version}"
    );
    let power = event
        .measurements()
        .per_phase(measurand::POWER_ACTIVE_IMPORT);
    assert_eq!(power.total, Some(Total::Reported(d(4140))));

    let (more, ev) = wallbox.unplug().unwrap();
    sent.extend(more);
    assert_eq!(ev.state(), EvState::Unplugged);

    let seq = check_all(version, &sent);
    if version == Version::V1_6 {
        assert_eq!(seq, Vec::<i32>::new());
        assert_eq!(sent.first().unwrap().action, "StatusNotification");
        assert!(actions(&sent).contains(&"StopTransaction"));
        assert!(
            sent.iter()
                .filter(|o| o.action == "MeterValues")
                .all(|o| o.payload.contains(r#""transactionId":42"#))
        );
    } else {
        assert_eq!(
            seq,
            (0..i32::try_from(seq.len()).unwrap()).collect::<Vec<_>>(),
            "strictly increasing from 0"
        );
        assert!(seq.len() > 60, "a sample a minute for an hour");
    }
    assert_eq!(sent.last().unwrap().action, "StatusNotification");
}

#[test]
fn a_session_on_16() {
    a_session(Version::V1_6);
}

#[test]
fn a_session_on_201() {
    a_session(Version::V2_0_1);
}

#[test]
fn a_session_on_21() {
    a_session(Version::V2_1);
}

#[test]
fn plugging_in_on_2x_starts_a_transaction_and_reports_the_needs() {
    for version in [Version::V2_0_1, Version::V2_1] {
        let mut wallbox = Wallbox::new(version, start());
        let ev = Ev::builder()
            .battery_wh(50_000)
            .soc(60)
            .target_soc(80)
            .current(6, 16)
            .departure("2026-11-21T07:00:00Z".parse().unwrap())
            .build();
        let sent = wallbox.plug_in(ev).unwrap();
        assert_eq!(
            actions(&sent),
            [
                "StatusNotification",
                "TransactionEvent",
                "NotifyEVChargingNeeds"
            ]
        );
        let DomainEvent::TransactionStarted {
            transaction_id,
            meter_start,
            ..
        } = received(version, &sent[1])
        else {
            panic!("not a start");
        };
        assert_eq!(transaction_id.as_deref(), Some("SIM-1"));
        assert_eq!(meter_start.unwrap().wh, Decimal::ZERO);

        // What the planner reads back: 20 % of 50 kWh, 6–16 A on three phases.
        let needs = if version == Version::V2_0_1 {
            let request: v2_0_1::NotifyEVChargingNeedsRequest =
                serde_json::from_str(&sent[2].payload).unwrap();
            EvNeeds::from_v2_0_1(&request.charging_needs)
        } else {
            let request: v2_1::NotifyEVChargingNeedsRequest =
                serde_json::from_str(&sent[2].payload).unwrap();
            EvNeeds::from_v2_1(&request.charging_needs)
        };
        assert_eq!(needs.energy, Some(d(10_000)));
        assert_eq!(
            (needs.min_current, needs.max_current),
            (Some(d(6)), Some(d(16)))
        );
        assert_eq!(needs.phases, Some(3));
        assert_eq!(needs.max_charge_power(Supply::default()), Some(d(11_040)));
        assert_eq!(
            needs.departure,
            Some("2026-11-21T07:00:00Z".parse().unwrap())
        );
    }
}

#[test]
fn on_16_no_sample_carries_an_id_the_csms_did_not_assign() {
    let mut wallbox = Wallbox::new(Version::V1_6, start());
    let sent = wallbox.plug_in(Ev::default()).unwrap();
    assert_eq!(actions(&sent), ["StatusNotification", "StartTransaction"]);
    let before = wallbox.advance(Duration::from_secs(600));
    assert!(
        !actions(&before).contains(&"MeterValues"),
        "skipped, not invented"
    );

    // Unplugged before the answer: the stop waits for the id, and keeps the unplug's time.
    let (unplugged, _) = wallbox.unplug().unwrap();
    let unplugged_at = wallbox.now();
    wallbox.advance(Duration::from_secs(30));
    assert!(!actions(&unplugged).contains(&"StopTransaction"));
    let released = wallbox.answered(
        "StartTransaction",
        r#"{"transactionId":7,"idTagInfo":{"status":"Accepted"}}"#,
    );
    assert_eq!(actions(&released), ["StopTransaction"]);
    let DomainEvent::TransactionEnded {
        transaction_id,
        stopped_reason,
        ..
    } = received(Version::V1_6, &released[0])
    else {
        panic!("not an end");
    };
    assert_eq!(transaction_id, "7");
    assert!(
        released[0]
            .payload
            .contains(&format!(r#""timestamp":"{unplugged_at}""#)),
        "{}",
        released[0].payload
    );
    assert_eq!(stopped_reason.as_deref(), Some("EVDisconnected"));
    check_all(Version::V1_6, &[before, unplugged].concat());
}

#[test]
fn a_dynamic_setpoint_makes_the_vehicle_discharge_and_lapses_without_updates() {
    let mut wallbox = Wallbox::new(Version::V2_1, start());
    let ev = Ev::builder()
        .soc(80)
        .bidirectional(7000)
        .reaction(Duration::from_secs(1))
        .build();
    let mut sent = wallbox.plug_in(ev).unwrap();
    let needs: v2_1::NotifyEVChargingNeedsRequest = serde_json::from_str(&sent[2].payload).unwrap();
    let needs = EvNeeds::from_v2_1(&needs.charging_needs);
    assert!(needs.bidirectional);
    assert_eq!(needs.max_discharge, Some(d(7000)));

    let profile = r#"{"evseId":1,"chargingProfile":{"id":9,"stackLevel":1,
        "chargingProfilePurpose":"TxProfile","chargingProfileKind":"Dynamic","transactionId":"SIM-1",
        "chargingSchedule":[{"id":9,"chargingRateUnit":"W","duration":30,"chargingSchedulePeriod":[
        {"startPeriod":0,"operationMode":"CentralSetpoint","setpoint":0,"dischargeLimit":-7000}]}]}}"#;
    assert!(
        wallbox
            .handle("SetChargingProfile", profile)
            .unwrap()
            .unwrap()
            .contains("Accepted")
    );
    let update = r#"{"chargingProfileId":9,"scheduleUpdate":{"setpoint":-2000}}"#;
    assert!(
        wallbox
            .handle("UpdateDynamicSchedule", update)
            .unwrap()
            .unwrap()
            .contains("Accepted")
    );
    sent.extend(wallbox.advance(Duration::from_secs(2)));
    let reading = wallbox.ev().unwrap().reading();
    assert_eq!(
        (reading.power, reading.state),
        (d(-2000), EvState::Discharging)
    );

    // The arbiter keeps updating: still discharging after a minute.
    for _ in 0..6 {
        sent.extend(wallbox.advance(Duration::from_secs(10)));
        wallbox
            .handle("UpdateDynamicSchedule", update)
            .unwrap()
            .unwrap();
    }
    assert_eq!(wallbox.ev().unwrap().reading().power, d(-2000));

    // The arbiter stops: 30 s later the profile lapses (K28.FR.13) and the vehicle charges again.
    sent.extend(wallbox.advance(Duration::from_secs(35)));
    assert!(wallbox.ev().unwrap().reading().power > Decimal::ZERO);

    let samples = sent
        .iter()
        .filter(|o| o.payload.contains("Energy.Active.Export.Register"));
    assert!(
        samples.count() > 0,
        "a bidirectional vehicle reports its export register"
    );
    check_all(Version::V2_1, &sent);

    assert!(
        wallbox
            .handle(
                "UpdateDynamicSchedule",
                r#"{"chargingProfileId":99,"scheduleUpdate":{"setpoint":0}}"#
            )
            .unwrap()
            .unwrap()
            .contains("Rejected")
    );
}

#[test]
fn a_tx_profile_is_capped_by_the_station_maximum_and_cleared_with_its_transaction() {
    let mut wallbox = Wallbox::new(Version::V2_0_1, start());
    wallbox
        .plug_in(Ev::builder().reaction(Duration::ZERO).build())
        .unwrap();
    let max = r#"{"evseId":0,"chargingProfile":{"id":1,"stackLevel":0,
        "chargingProfilePurpose":"ChargingStationMaxProfile","chargingProfileKind":"Absolute",
        "chargingSchedule":[{"id":1,"chargingRateUnit":"A","startSchedule":"2026-11-20T00:00:00Z",
        "chargingSchedulePeriod":[{"startPeriod":0,"limit":10}]}]}}"#;
    let tx = r#"{"evseId":1,"chargingProfile":{"id":2,"stackLevel":0,"transactionId":"SIM-1",
        "chargingProfilePurpose":"TxProfile","chargingProfileKind":"Relative",
        "chargingSchedule":[{"id":2,"chargingRateUnit":"W","chargingSchedulePeriod":[
        {"startPeriod":0,"limit":11040}]}]}}"#;
    for profile in [max, tx] {
        assert!(
            wallbox
                .handle("SetChargingProfile", profile)
                .unwrap()
                .unwrap()
                .contains("Accepted")
        );
    }
    wallbox.advance(Duration::from_secs(1));
    assert_eq!(
        wallbox.ev().unwrap().reading().current,
        d(10),
        "16 A asked, 10 A allowed"
    );

    // One phase from the energy manager: a pause, then single phase.
    let one_phase = r#"{"evseId":1,"chargingProfile":{"id":2,"stackLevel":0,"transactionId":"SIM-1",
        "chargingProfilePurpose":"TxProfile","chargingProfileKind":"Relative",
        "chargingSchedule":[{"id":2,"chargingRateUnit":"A","chargingSchedulePeriod":[
        {"startPeriod":0,"limit":8,"numberPhases":1}]}]}}"#;
    wallbox
        .handle("SetChargingProfile", one_phase)
        .unwrap()
        .unwrap();
    wallbox.advance(Duration::from_secs(1));
    assert_eq!(wallbox.ev().unwrap().state(), EvState::SwitchingPhases);
    wallbox.advance(Duration::from_secs(5));
    let reading = wallbox.ev().unwrap().reading();
    assert_eq!((reading.phases, reading.current), (1, d(8)));

    wallbox.unplug().unwrap();
    assert_eq!(
        wallbox.profiles().profiles().len(),
        1,
        "the TxProfile went with its transaction"
    );
}

#[test]
fn a_vehicle_that_does_not_survive_a_phase_switch_stops_until_replugged() {
    let mut wallbox = Wallbox::new(Version::V2_1, start());
    let ev = Ev::builder()
        .reaction(Duration::ZERO)
        .phase_switching(PhaseSwitching::StopsCharging)
        .build();
    wallbox.plug_in(ev).unwrap();
    wallbox.advance(Duration::from_secs(1));
    let one_phase = r#"{"evseId":1,"chargingProfile":{"id":3,"stackLevel":0,
        "chargingProfilePurpose":"TxDefaultProfile","chargingProfileKind":"Relative",
        "chargingSchedule":[{"id":3,"chargingRateUnit":"A","chargingSchedulePeriod":[
        {"startPeriod":0,"limit":16,"numberPhases":1}]}]}}"#;
    wallbox
        .handle("SetChargingProfile", one_phase)
        .unwrap()
        .unwrap();
    let sent = wallbox.advance(Duration::from_secs(60));
    assert_eq!(wallbox.ev().unwrap().state(), EvState::Faulted);
    let reported = sent
        .iter()
        .any(|o| o.payload.contains(r#""chargingState":"SuspendedEV""#));
    assert!(reported, "the CSMS sees the vehicle stop");
}

#[test]
fn what_the_wallbox_cannot_apply_is_answered_not_panicked() {
    for version in [Version::V1_6, Version::V2_0_1, Version::V2_1] {
        let mut wallbox = Wallbox::new(version, start());
        let not_an_object = wallbox
            .handle("SetChargingProfile", "[]")
            .unwrap()
            .unwrap_err();
        assert_eq!(not_an_object.code, ErrorCode::FormatViolation);
        let missing = wallbox
            .handle("SetChargingProfile", "{}")
            .unwrap()
            .unwrap_err();
        assert_eq!(missing.code, ErrorCode::OccurrenceConstraintViolation);

        // A TxProfile without a transaction is refused (K01.FR.09); nothing is installed.
        let tx_profile = match version {
            Version::V1_6 => {
                r#"{"connectorId":1,"csChargingProfiles":{"chargingProfileId":5,"stackLevel":0,
                    "chargingProfilePurpose":"TxProfile","chargingProfileKind":"Relative",
                    "chargingSchedule":{"chargingRateUnit":"A","chargingSchedulePeriod":[
                    {"startPeriod":0,"limit":6}]}}}"#
            }
            _ => {
                r#"{"evseId":1,"chargingProfile":{"id":5,"stackLevel":0,
                    "chargingProfilePurpose":"TxProfile","chargingProfileKind":"Relative",
                    "chargingSchedule":[{"id":5,"chargingRateUnit":"A","chargingSchedulePeriod":[
                    {"startPeriod":0,"limit":6}]}]}}"#
            }
        };
        let answer = wallbox
            .handle("SetChargingProfile", tx_profile)
            .unwrap()
            .unwrap();
        assert!(answer.contains("Rejected"), "{version}: {answer}");
        assert_eq!(wallbox.profiles().profiles(), []);

        let clear = match version {
            Version::V1_6 => r#"{"id":5}"#,
            _ => r#"{"chargingProfileId":5}"#,
        };
        assert!(
            wallbox
                .handle("ClearChargingProfile", clear)
                .unwrap()
                .unwrap()
                .contains("Unknown")
        );
        assert!(wallbox.handle("Reset", "{}").is_none(), "not the wallbox's");
    }
}

#[test]
fn without_a_sample_interval_only_state_changes_are_reported() {
    let mut wallbox = Wallbox::new(Version::V2_0_1, start()).sample_every(None);
    wallbox
        .plug_in(Ev::builder().reaction(Duration::ZERO).build())
        .unwrap();
    let sent = wallbox.advance(Duration::from_secs(3600));
    assert!(
        sent.iter()
            .all(|o| !o.payload.contains("MeterValuePeriodic"))
    );
    assert_eq!(
        sent.len(),
        1,
        "EVConnected → Charging: {:?}",
        actions(&sent)
    );
    assert_eq!(wallbox.advance(Duration::ZERO), []);
}

fn register_in(out: &Outgoing) -> Option<String> {
    let v: Value = serde_json::from_str(&out.payload).unwrap();
    let values = v["meterValue"].as_array()?.last()?["sampledValue"]
        .as_array()?
        .clone();
    values
        .iter()
        .find(|s| s["measurand"] == "Energy.Active.Import.Register")
        // 1.6 sends the value as a string, 2.x as a number.
        .map(|s| {
            s["value"]
                .as_str()
                .map_or_else(|| s["value"].to_string(), str::to_owned)
        })
}

/// 1.6 §6.45/§6.49: `meterStart`/`meterStop` are whole Wh, so the register sampled at the same
/// instant is too — by truncation, never ahead of the energy delivered.
#[test]
fn whole_wh_on_1_6_one_register_value_per_instant() {
    let mut fractional_seen = false;
    for seconds in [1u64, 7, 13, 61, 333] {
        let mut wallbox =
            Wallbox::new(Version::V1_6, start()).sample_every(Some(Duration::from_secs(seconds)));
        wallbox
            .plug_in(Ev::builder().reaction(Duration::ZERO).build())
            .unwrap();
        wallbox.answered(
            "StartTransaction",
            r#"{"transactionId":7,"idTagInfo":{"status":"Accepted"}}"#,
        );
        let sent = wallbox.advance(Duration::from_secs(seconds));
        let exact = wallbox.ev().unwrap().reading().imported;
        fractional_seen |= exact.trunc_to(0) != Some(exact);

        let sample = sent
            .iter()
            .rfind(|o| o.action == "MeterValues")
            .expect("a periodic sample at the instant");
        let v: Value = serde_json::from_str(&sample.payload).unwrap();
        assert_eq!(
            v["meterValue"][0]["timestamp"].as_str().unwrap(),
            wallbox.now().to_string()
        );
        let register = register_in(sample).unwrap();

        let (stopped, _) = wallbox.unplug().unwrap();
        let stop = stopped
            .iter()
            .find(|o| o.action == "StopTransaction")
            .unwrap();
        let stop: Value = serde_json::from_str(&stop.payload).unwrap();
        assert_eq!(
            stop["timestamp"].as_str().unwrap(),
            wallbox.now().to_string()
        );
        let meter_stop = stop["meterStop"].as_i64().unwrap();

        assert_eq!(
            register,
            meter_stop.to_string(),
            "{seconds}s: one value per instant"
        );
        assert_eq!(
            Some(Decimal::from(meter_stop)),
            exact.trunc_to(0),
            "truncated, not rounded"
        );
        for out in &sent {
            received(Version::V1_6, out);
        }
    }
    assert!(fractional_seen, "the sweep must hit a fractional register");
}

#[test]
fn whole_wh_is_a_1_6_rule_and_2x_registers_stay_exact() {
    let mut wallbox =
        Wallbox::new(Version::V2_0_1, start()).sample_every(Some(Duration::from_secs(7)));
    wallbox
        .plug_in(Ev::builder().reaction(Duration::ZERO).build())
        .unwrap();
    let sent = wallbox.advance(Duration::from_secs(7));
    let exact = wallbox.ev().unwrap().reading().imported;
    assert_ne!(
        exact.trunc_to(0),
        Some(exact),
        "7 s of charging is fractional Wh"
    );
    let reported = sent.iter().rev().find_map(register_in).unwrap();
    assert_eq!(reported.parse::<Decimal>().unwrap(), exact);
}
