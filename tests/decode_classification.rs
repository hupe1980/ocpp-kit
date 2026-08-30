//! Every deserialization failure must map to the OCPP error code the specification names —
//! the part that libraries most often collapse into a single `FormatViolation`.

use ocpp_kit::RawValue;
use ocpp_kit::decode::{
    DateTimeLeniency, DecodeError, DecodeErrorKind, DecodeOptions, NumericStrings, UnknownFields,
    decode_payload,
};
use ocpp_kit::rpc::{CallError, ErrorCode};
use ocpp_kit::v2_1;

fn decode<T>(json: &str, options: &DecodeOptions) -> Result<T, DecodeError>
where
    T: serde::de::DeserializeOwned + serde::Serialize + ocpp_kit::validate::Validate,
{
    let raw = RawValue::from_string(json.to_string()).expect("valid json");
    decode_payload::<T>(&raw, options)
}

fn kind_of(json: &str) -> DecodeErrorKind {
    decode::<v2_1::BootNotificationRequest>(json, &DecodeOptions::strict())
        .unwrap_err()
        .kind
}

fn code_of(json: &str) -> ErrorCode {
    let error =
        decode::<v2_1::BootNotificationRequest>(json, &DecodeOptions::strict()).unwrap_err();
    CallError::from(error).code
}

#[test]
fn a_valid_payload_decodes() {
    let request: v2_1::BootNotificationRequest = decode(
        r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}"#,
        &DecodeOptions::strict(),
    )
    .unwrap();
    assert_eq!(request.reason, v2_1::BootReason::PowerUp);
    assert_eq!(request.charging_station.model, "M1");
}

#[test]
fn missing_required_field_is_an_occurrence_violation() {
    let json = r#"{"chargingStation":{"model":"M1","vendorName":"ACME"}}"#;
    assert_eq!(kind_of(json), DecodeErrorKind::Occurrence);
    assert_eq!(code_of(json), ErrorCode::OccurrenceConstraintViolation);
}

#[test]
fn wrong_json_type_is_a_type_violation() {
    let json = r#"{"reason":"PowerUp","chargingStation":{"model":12,"vendorName":"ACME"}}"#;
    assert_eq!(kind_of(json), DecodeErrorKind::Type);
    assert_eq!(code_of(json), ErrorCode::TypeConstraintViolation);
    let error =
        decode::<v2_1::BootNotificationRequest>(json, &DecodeOptions::strict()).unwrap_err();
    assert_eq!(error.path, "/chargingStation/model");
}

#[test]
fn too_long_a_string_is_a_property_violation_with_a_pointer() {
    let json = format!(
        r#"{{"reason":"PowerUp","chargingStation":{{"model":"{}","vendorName":"ACME"}}}}"#,
        "M".repeat(21)
    );
    let error =
        decode::<v2_1::BootNotificationRequest>(&json, &DecodeOptions::strict()).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Property);
    assert_eq!(error.path, "/chargingStation/model");
    assert!(error.reason.contains("maxLength 20"), "{}", error.reason);
    assert_eq!(
        CallError::from(error).code,
        ErrorCode::PropertyConstraintViolation
    );
}

#[test]
fn an_undefined_enum_value_is_a_property_violation_by_default() {
    let json = r#"{"reason":"Levitation","chargingStation":{"model":"M1","vendorName":"ACME"}}"#;
    assert_eq!(kind_of(json), DecodeErrorKind::Property);

    // …but is preserved when the peer is known to be creative.
    let request: v2_1::BootNotificationRequest =
        decode(json, &DecodeOptions::lenient()).expect("lenient mode keeps it");
    assert_eq!(
        request.reason,
        v2_1::BootReason::UnknownValue("Levitation".into())
    );
    assert_eq!(request.reason.as_str(), "Levitation");
    assert!(!request.reason.is_known());
}

#[test]
fn a_non_object_payload_is_a_format_violation() {
    assert_eq!(kind_of("[]"), DecodeErrorKind::Format);
    assert_eq!(kind_of("\"BootNotification\""), DecodeErrorKind::Format);
    assert_eq!(code_of("[]"), ErrorCode::FormatViolation);
}

#[test]
fn undefined_members_are_ignored_by_default_and_rejected_when_asked() {
    let json =
        r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"},"whoops":1}"#;
    assert!(decode::<v2_1::BootNotificationRequest>(json, &DecodeOptions::strict()).is_ok());

    let error =
        decode::<v2_1::BootNotificationRequest>(json, &DecodeOptions::pedantic()).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::UnknownField);
    assert_eq!(error.path, "/whoops");
    assert_eq!(CallError::from(error).code, ErrorCode::ProtocolError);
}

#[test]
fn nested_undefined_members_are_found_too() {
    let json = r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME","colour":"red"}}"#;
    let error =
        decode::<v2_1::BootNotificationRequest>(json, &DecodeOptions::pedantic()).unwrap_err();
    assert_eq!(error.path, "/chargingStation/colour");
}

#[test]
fn custom_data_keeps_its_extension_members_even_in_pedantic_mode() {
    let json = r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"},
                   "customData":{"vendorId":"acme","fleet":"north"}}"#;
    let request: v2_1::BootNotificationRequest = decode(json, &DecodeOptions::pedantic()).unwrap();
    let custom = request.custom_data.expect("customData");
    assert_eq!(custom.vendor_id, "acme");
    assert_eq!(custom.extra["fleet"], serde_json::json!("north"));
}

#[test]
fn strict_date_times_require_an_offset() {
    let json = r#"{"timestamp":"2024-01-01T10:00:00","eventType":"Started","seqNo":0,
                   "triggerReason":"Authorized","transactionInfo":{"transactionId":"t1"}}"#;
    let error =
        decode::<v2_1::TransactionEventRequest>(json, &DecodeOptions::strict()).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Property);
    assert_eq!(error.path, "/timestamp");
}

#[test]
fn lenient_mode_repairs_the_time_stamps_field_devices_actually_send() {
    for wire in ["2024-01-01T10:00:00", "2024-01-01 10:00:00"] {
        let json = format!(
            r#"{{"timestamp":"{wire}","eventType":"Started","seqNo":0,
                 "triggerReason":"Authorized","transactionInfo":{{"transactionId":"t1"}}}}"#
        );
        let request: v2_1::TransactionEventRequest =
            decode(&json, &DecodeOptions::lenient()).expect(wire);
        assert_eq!(request.timestamp.to_string(), "2024-01-01T10:00:00Z");
    }
}

#[test]
fn numeric_strings_are_coerced_only_when_asked() {
    let json = r#"{"timestamp":"2024-01-01T10:00:00Z","eventType":"Started","seqNo":"7",
                   "triggerReason":"Authorized","transactionInfo":{"transactionId":"t1"}}"#;
    let error =
        decode::<v2_1::TransactionEventRequest>(json, &DecodeOptions::strict()).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Type);
    assert_eq!(error.path, "/seqNo");

    let request: v2_1::TransactionEventRequest =
        decode(json, &DecodeOptions::lenient()).expect("coerced");
    assert_eq!(request.seq_no, 7);
}

#[test]
fn repairs_are_bounded() {
    // Three broken members, but only two repairs allowed.
    let json = r#"{"timestamp":"2024-01-01 10:00:00","eventType":"Started","seqNo":"7",
                   "triggerReason":"Authorized","transactionInfo":{"transactionId":"t1"},
                   "offline":false,"numberOfPhasesUsed":"3"}"#;
    let mut options = DecodeOptions::lenient();
    options.max_repairs = 2;
    assert!(decode::<v2_1::TransactionEventRequest>(json, &options).is_err());
    options.max_repairs = 8;
    assert!(decode::<v2_1::TransactionEventRequest>(json, &options).is_ok());
}

#[test]
fn oversized_payloads_are_refused_before_parsing() {
    let json = format!(
        r#"{{"reason":"PowerUp","chargingStation":{{"model":"M1","vendorName":"{}"}}}}"#,
        "A".repeat(4096)
    );
    let options = DecodeOptions::strict().with_max_payload_size(1024);
    let error = decode::<v2_1::BootNotificationRequest>(&json, &options).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::Format);
    assert!(error.reason.contains("limit is 1024"));
}

#[test]
fn leniency_knobs_are_independent() {
    let mut options = DecodeOptions::strict();
    options.datetime = DateTimeLeniency::AllowNaive;
    // A missing offset is fine now…
    let json = r#"{"timestamp":"2024-01-01T10:00:00","eventType":"Started","seqNo":0,
                   "triggerReason":"Authorized","transactionInfo":{"transactionId":"t1"}}"#;
    assert!(decode::<v2_1::TransactionEventRequest>(json, &options).is_ok());
    // …but a space separator still is not, and numeric strings still are not.
    let spaced = json.replace("01T10", "01 10");
    assert!(decode::<v2_1::TransactionEventRequest>(&spaced, &options).is_err());
    assert_eq!(options.numeric_strings, NumericStrings::Reject);
    assert_eq!(options.unknown_fields, UnknownFields::Ignore);
}

#[test]
fn dispatch_unions_decode_by_action() {
    let raw = RawValue::from_string(
        r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}"#.to_string(),
    )
    .unwrap();
    let request = v2_1::CsRequest::decode(
        v2_1::Action::BootNotification,
        &raw,
        &DecodeOptions::strict(),
    )
    .unwrap();
    assert_eq!(request.action(), v2_1::Action::BootNotification);
    assert!(matches!(request, v2_1::CsRequest::BootNotification(_)));

    // A CSMS-originated action is not decodable as a Charging Station request.
    let error =
        v2_1::CsRequest::decode(v2_1::Action::Reset, &raw, &DecodeOptions::strict()).unwrap_err();
    assert_eq!(error.kind, DecodeErrorKind::UnsupportedAction);
    assert_eq!(CallError::from(error).code, ErrorCode::NotSupported);
}
