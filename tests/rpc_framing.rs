//! OCPP-J framing conformance (Part 4 §4.2 / 1.6J §4.2).

use ocpp_kit::Version;
use ocpp_kit::rpc::{CallError, ErrorCode, Frame, FrameError, MessageTypeId};
use ocpp_kit::types::MessageId;

fn call(text: &str, version: Version) -> Frame<'_> {
    Frame::parse(text, version).expect("frame parses")
}

#[test]
fn parses_and_reserializes_a_call() {
    let text = r#"[2,"19223201","BootNotification",{"reason":"PowerUp"}]"#;
    let frame = call(text, Version::V2_1);
    assert_eq!(frame.message_type(), MessageTypeId::Call);
    assert_eq!(frame.id().as_str(), "19223201");
    assert_eq!(frame.action(), Some("BootNotification"));
    assert_eq!(frame.payload().unwrap().get(), r#"{"reason":"PowerUp"}"#);
    assert_eq!(frame.to_json(Version::V2_1).unwrap(), text);
}

#[test]
fn parses_call_result_and_call_error() {
    let result = call(
        r#"[3,"19223201",{"currentTime":"2013-02-01T20:53:32.486Z"}]"#,
        Version::V2_1,
    );
    assert_eq!(result.message_type(), MessageTypeId::CallResult);

    let text = r#"[4,"162376037","NotSupported","SetDisplayMessageRequest not implemented",{}]"#;
    let frame = call(text, Version::V2_1);
    let Frame::CallError { id, error } = &frame else {
        panic!("expected CALLERROR")
    };
    assert_eq!(id.as_str(), "162376037");
    assert_eq!(error.code, ErrorCode::NotSupported);
    assert_eq!(frame.to_json(Version::V2_1).unwrap(), text);
}

#[test]
fn send_and_call_result_error_are_21_only() {
    let send = r#"[6,"stream-7","NotifyPeriodicEventStream",{"id":1,"pending":0,"basetime":"2024-01-01T00:00:00Z","data":[]}]"#;
    assert_eq!(
        call(send, Version::V2_1).message_type(),
        MessageTypeId::Send
    );
    assert!(matches!(
        Frame::parse(send, Version::V2_0_1),
        Err(FrameError::MessageTypeNotInVersion {
            message_type: MessageTypeId::Send,
            ..
        })
    ));

    let cre = r#"[5,"7","FormatViolation","",{}]"#;
    assert_eq!(
        call(cre, Version::V2_1).message_type(),
        MessageTypeId::CallResultError
    );
    assert!(Frame::parse(cre, Version::V1_6).is_err());
}

#[test]
fn unknown_message_type_is_ignored_on_16_and_21_but_answered_on_201() {
    let text = r#"[7,"1","Whatever",{}]"#;
    for version in [Version::V1_6, Version::V2_0_1, Version::V2_1] {
        let error = Frame::parse(text, version).unwrap_err();
        assert!(matches!(error, FrameError::UnknownMessageType { .. }));
        // Part 4 §4.4 changed between 2.0.1 and 2.1.
        assert_eq!(error.is_ignorable(version), version != Version::V2_0_1);
        assert_eq!(error.error_code(), ErrorCode::MessageTypeNotSupported);
    }
}

#[test]
fn unreadable_message_id_is_answered_with_minus_one() {
    for text in [
        r#"[2,123,"Heartbeat",{}]"#,
        r#"[2,"","Heartbeat",{}]"#,
        "[2]",
    ] {
        let error = Frame::parse(text, Version::V2_1).unwrap_err();
        assert_eq!(error.reply_id(), MessageId::unreadable());
        assert_eq!(error.reply_id().as_str(), "-1");
    }
    assert_eq!(
        Frame::parse(r#"[2,"1","Heartbeat",{}]"#, Version::V2_1)
            .unwrap()
            .id()
            .as_str(),
        "1"
    );
}

#[test]
fn malformed_frames_are_rpc_framework_errors() {
    for text in [
        "not json",
        "{}",
        "[]",
        r#"["2","1","Heartbeat",{}]"#,
        r#"[2,"1"]"#,
    ] {
        let error = Frame::parse(text, Version::V2_1).unwrap_err();
        assert_eq!(error.error_code(), ErrorCode::RpcFrameworkError, "{text}");
    }
}

#[test]
fn over_long_message_ids_are_echoed_verbatim_but_flagged() {
    let long = "x".repeat(40);
    let text = format!(r#"[2,"{long}","Heartbeat",{{}}]"#);
    let frame = Frame::parse(&text, Version::V2_1).unwrap();
    assert_eq!(frame.id().as_str(), long);
    assert!(!frame.id().is_conforming());
}

#[test]
fn error_codes_use_the_versions_own_spelling() {
    // 1.6J prints `FormationViolation` and — with a single `r` — `OccurenceConstraintViolation`.
    assert_eq!(
        ErrorCode::FormatViolation.as_wire(Version::V1_6),
        "FormationViolation"
    );
    assert_eq!(
        ErrorCode::FormatViolation.as_wire(Version::V2_1),
        "FormatViolation"
    );
    assert_eq!(
        ErrorCode::OccurrenceConstraintViolation.as_wire(Version::V1_6),
        "OccurenceConstraintViolation"
    );
    assert_eq!(
        ErrorCode::OccurrenceConstraintViolation.as_wire(Version::V2_0_1),
        "OccurrenceConstraintViolation"
    );
    // Codes 1.6J does not define degrade to GenericError rather than being invented.
    assert_eq!(
        ErrorCode::RpcFrameworkError.as_wire(Version::V1_6),
        "GenericError"
    );
    assert_eq!(
        ErrorCode::MessageTypeNotSupported.as_wire(Version::V1_6),
        "GenericError"
    );
    assert!(!ErrorCode::RpcFrameworkError.is_defined_in(Version::V1_6));

    // Parsing accepts either spelling regardless of version.
    assert_eq!(
        ErrorCode::parse("FormationViolation"),
        ErrorCode::FormatViolation
    );
    assert_eq!(
        ErrorCode::parse("FormatViolation"),
        ErrorCode::FormatViolation
    );
    assert_eq!(
        ErrorCode::parse("OccurenceConstraintViolation"),
        ErrorCode::OccurrenceConstraintViolation
    );
}

#[test]
fn call_error_frames_round_trip_through_owned_form() {
    let error = CallError::new(ErrorCode::InternalError, "storage unavailable");
    let frame = Frame::CallError {
        id: MessageId::new("7").unwrap(),
        error: (&error).into(),
    };
    let json = frame.to_json(Version::V1_6).unwrap();
    assert_eq!(json, r#"[4,"7","InternalError","storage unavailable",{}]"#);

    let parsed = Frame::parse(&json, Version::V1_6).unwrap();
    let Frame::CallError {
        error: parsed_error,
        ..
    } = &parsed
    else {
        panic!()
    };
    assert_eq!(parsed_error.to_call_error(), error);
    assert_eq!(parsed.into_owned().to_json(Version::V1_6).unwrap(), json);
}
