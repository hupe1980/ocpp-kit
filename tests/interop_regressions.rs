//! Regressions found by running against implementations other than this one.
//!
//! Each test names the behaviour an independent peer exercised and the specification text that
//! decides it. A self-test suite cannot find these: both of its sides read the specification
//! the same way.

use ocpp_kit::RawValue;
use ocpp_kit::decode::{DecodeError, DecodeErrorKind, DecodeOptions, WholeFloats, decode_payload};
use ocpp_kit::message::Origin;
use ocpp_kit::rpc::{CallError, ErrorCode};
use ocpp_kit::{v1_6, v2_0_1, v2_1};

fn decode<T>(json: &str, options: &DecodeOptions) -> Result<T, DecodeError>
where
    T: serde::de::DeserializeOwned + serde::Serialize + ocpp_kit::validate::Validate,
{
    let raw = RawValue::from_string(json.to_string()).expect("valid json");
    decode_payload::<T>(&raw, options)
}

/// What `EVerest` sends: `NotifyEVChargingNeeds` with `"energyAmount": 20000.0`.
fn charging_needs(energy: &str) -> String {
    format!(
        r#"{{"evseId":1,"chargingNeeds":{{"requestedEnergyTransfer":"AC_three_phase","acChargingParameters":{{"energyAmount":{energy},"evMinCurrent":6,"evMaxCurrent":32,"evMaxVoltage":400}}}}}}"#
    )
}

// ---------------------------------------------------------------------------
// `integer` under draft-06: any number with a zero fractional part
// ---------------------------------------------------------------------------

#[test]
fn a_draft_06_integer_accepts_a_whole_number_written_with_a_fraction() {
    for energy in ["20000", "20000.0", "2e4", "20000.000"] {
        let request: v2_0_1::NotifyEVChargingNeedsRequest =
            decode(&charging_needs(energy), &DecodeOptions::strict())
                .unwrap_or_else(|e| panic!("{energy}: {e}"));
        let ac = request.charging_needs.ac_charging_parameters.unwrap();
        assert_eq!(ac.energy_amount, 20000, "{energy}");
    }
}

#[test]
fn a_whole_float_is_sent_back_as_an_integer() {
    let request: v2_0_1::NotifyEVChargingNeedsRequest =
        decode(&charging_needs("20000.0"), &DecodeOptions::strict()).unwrap();
    let wire = serde_json::to_string(&request).unwrap();
    assert!(wire.contains(r#""energyAmount":20000,"#), "{wire}");
}

#[test]
fn a_real_fraction_is_still_a_type_constraint_violation() {
    let error = decode::<v2_0_1::NotifyEVChargingNeedsRequest>(
        &charging_needs("20000.5"),
        &DecodeOptions::lenient(),
    )
    .unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Type);
    assert_eq!(
        error.path,
        "/chargingNeeds/acChargingParameters/energyAmount"
    );
    assert_eq!(
        CallError::from(error).code,
        ErrorCode::TypeConstraintViolation
    );
}

#[test]
fn an_integer_outside_i32_is_still_a_property_violation() {
    for energy in ["3000000000", "3000000000.0"] {
        let error = decode::<v2_0_1::NotifyEVChargingNeedsRequest>(
            &charging_needs(energy),
            &DecodeOptions::strict(),
        )
        .unwrap_err();
        assert_eq!(error.kind, DecodeErrorKind::Property, "{energy}");
    }
}

#[test]
fn whole_floats_reach_optional_members_and_arrays_in_2_1() {
    // `ReserveNowRequest.id` is a required integer, `evseId` an optional one.
    let request: v2_1::ReserveNowRequest = decode(
        r#"{"id":7.0,"expiryDateTime":"2026-11-07T12:00:00Z","idToken":{"idToken":"A","type":"ISO14443"},"evseId":1.0}"#,
        &DecodeOptions::strict(),
    )
    .unwrap();
    assert_eq!(request.id, 7);
    assert_eq!(request.evse_id, Some(1));
}

// ---------------------------------------------------------------------------
// `integer` under draft-04 (the OCPP 1.6 core schemas): no fraction part at all
// ---------------------------------------------------------------------------

const METER_VALUES: &str = r#"{"connectorId":1.0,"meterValue":[{"timestamp":"2026-11-07T12:00:00Z","sampledValue":[{"value":"1"}]}]}"#;

#[test]
fn a_draft_04_integer_rejects_a_fraction_part_when_strict() {
    let error =
        decode::<v1_6::MeterValuesRequest>(METER_VALUES, &DecodeOptions::strict()).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Type);
    assert_eq!(error.path, "/connectorId");
}

#[test]
fn a_lenient_decode_repairs_a_draft_04_whole_float() {
    let request: v1_6::MeterValuesRequest =
        decode(METER_VALUES, &DecodeOptions::lenient()).unwrap();
    assert_eq!(request.connector_id, 1);

    let only_that = DecodeOptions::strict().with_whole_floats(WholeFloats::Coerce);
    let request: v1_6::MeterValuesRequest = decode(METER_VALUES, &only_that).unwrap();
    assert_eq!(request.connector_id, 1);
}

#[test]
fn a_lenient_decode_never_repairs_a_real_fraction() {
    let error = decode::<v1_6::MeterValuesRequest>(
        &METER_VALUES.replace("1.0", "1.5"),
        &DecodeOptions::lenient(),
    )
    .unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Type);
}

#[test]
fn the_1_6_security_extension_schemas_are_draft_06() {
    // `ExtendedTriggerMessage` is a Security Whitepaper schema: draft-06, so `2.0` is an
    // integer there even in 1.6.
    let request: v1_6::ExtendedTriggerMessageRequest = decode(
        r#"{"requestedMessage":"Heartbeat","connectorId":2.0}"#,
        &DecodeOptions::strict(),
    )
    .unwrap();
    assert_eq!(request.connector_id, Some(2));
}

// ---------------------------------------------------------------------------
// Direction and functional block, as the 2.1 Part 2 use cases state them
// ---------------------------------------------------------------------------

#[test]
fn actions_carry_the_direction_and_block_of_their_use_case() {
    // C25.FR.21: "CSMS MAY send a NotifyWebPaymentStartedRequest"; C25.FR.27: the station
    // responds.
    assert_eq!(v2_1::Action::NotifyWebPaymentStarted.origin(), Origin::Csms);
    for (action, block) in [
        (v2_1::Action::NotifyWebPaymentStarted, "C"), // C25
        (v2_1::Action::NotifySettlement, "C"),        // C21
        (v2_1::Action::VatNumberValidation, "C"),     // C18
        (v2_1::Action::Heartbeat, "G"),               // G02
    ] {
        assert_eq!(action.block(), block, "{action:?}");
    }
    assert_eq!(v2_0_1::Action::Heartbeat.block(), "G"); // G02
}

#[test]
fn a_station_decodes_notify_web_payment_started_as_a_csms_request() {
    let raw = RawValue::from_string(r#"{"evseId":1,"timeout":120}"#.to_string()).unwrap();
    let request = v2_1::CsmsRequest::decode(
        v2_1::Action::NotifyWebPaymentStarted,
        &raw,
        &DecodeOptions::strict(),
    )
    .unwrap();
    assert!(matches!(
        request,
        v2_1::CsmsRequest::NotifyWebPaymentStarted(_)
    ));
}

// ---------------------------------------------------------------------------
// Leniency is reported, not silent
// ---------------------------------------------------------------------------

#[test]
fn every_repair_is_reported_with_its_pointer_and_kind() {
    use ocpp_kit::decode::{RepairKind, decode_payload_reporting};
    let json = r#"{"timestamp":"2024-01-01 10:00:00","eventType":"Started","seqNo":"7",
                   "triggerReason":"Authorized","transactionInfo":{"transactionId":"t1"}}"#;
    let raw = RawValue::from_string(json.to_string()).unwrap();
    let decoded =
        decode_payload_reporting::<v2_1::TransactionEventRequest>(&raw, &DecodeOptions::lenient())
            .unwrap();
    let mut seen: Vec<_> = decoded
        .repairs
        .iter()
        .map(|r| (r.pointer.as_str(), r.kind))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("/seqNo", RepairKind::NumericString),
            ("/timestamp", RepairKind::DateTimeWithSpace),
        ]
    );
}

#[test]
fn a_preserved_unknown_enum_value_is_a_repair_too() {
    use ocpp_kit::decode::{RepairKind, decode_payload_reporting};
    let raw = RawValue::from_string(
        r#"{"reason":"SolarFlare","chargingStation":{"model":"M1","vendorName":"ACME"}}"#
            .to_string(),
    )
    .unwrap();
    let decoded =
        decode_payload_reporting::<v2_1::BootNotificationRequest>(&raw, &DecodeOptions::lenient())
            .unwrap();
    assert_eq!(decoded.repairs.len(), 1);
    assert_eq!(decoded.repairs[0].pointer, "/reason");
    assert_eq!(decoded.repairs[0].kind, RepairKind::UnknownEnumValue);
}

#[test]
fn a_conforming_payload_reports_nothing() {
    use ocpp_kit::decode::decode_payload_reporting;
    let decoded = decode_payload_reporting::<v2_0_1::NotifyEVChargingNeedsRequest>(
        &RawValue::from_string(charging_needs("20000.0")).unwrap(),
        &DecodeOptions::lenient(),
    )
    .unwrap();
    // `20000.0` is a draft-06 integer: conforming, so not a repair.
    assert!(decoded.repairs.is_empty(), "{:?}", decoded.repairs);
}

#[test]
fn whole_float_repairs_are_reported_for_1_6() {
    use ocpp_kit::decode::{RepairKind, decode_payload_reporting};
    let decoded = decode_payload_reporting::<v1_6::MeterValuesRequest>(
        &RawValue::from_string(METER_VALUES.to_string()).unwrap(),
        &DecodeOptions::lenient(),
    )
    .unwrap();
    assert_eq!(decoded.repairs.len(), 1);
    assert_eq!(decoded.repairs[0].pointer, "/connectorId");
    assert_eq!(decoded.repairs[0].kind, RepairKind::WholeFloat);
    assert_eq!(decoded.repairs[0].kind.as_str(), "whole_float");
}
