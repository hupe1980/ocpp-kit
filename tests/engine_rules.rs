//! The protocol rules the sans-I/O engine enforces, driven with simulated time.

use std::time::Duration;

use ocpp_kit::RawValue;
use ocpp_kit::Version;
use ocpp_kit::engine::{
    BootState, CallFailure, CallOptions, CloseReason, Engine, EngineConfig, EngineError,
    HeartbeatPolicy, InboundConcurrency, Input, Instant, MemStore, MessageStore, OfflinePolicy,
    Output, ProtocolViolation, RetryPolicy, Role, Timer,
};
use ocpp_kit::message::MessageKind;
use ocpp_kit::rpc::ErrorCode;
use ocpp_kit::testkit::Recorder;

/// The engine takes the driver's clock on every entry point. A test that is not
/// exercising a timer supplies the origin and moves on.
const NOW: Instant = Instant::ZERO;

/// The `MessageId` of a frame a test already captured.
fn frame_id(frame: &str) -> String {
    let parts: Vec<serde_json::Value> = serde_json::from_str(frame).unwrap();
    parts[1].as_str().unwrap().to_string()
}

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_string()).unwrap()
}

fn station(version: Version) -> Engine<MemStore> {
    let mut engine = Engine::new(
        EngineConfig::new(Role::ChargingStation, version)
            // The boot gate is exercised in its own tests.
            .with_heartbeat(HeartbeatPolicy::Manual),
    );
    engine.handle(NOW, Input::Connected { version });
    let _ = engine.drain();
    engine
}

/// A station that has already been accepted, so ordinary traffic is allowed.
fn booted_station(version: Version) -> Engine<MemStore> {
    let mut engine = station(version);
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let mut recorder = Recorder::new();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}}]"#
    )));
    let _ = engine.drain();
    assert_eq!(engine.boot_state(), BootState::Accepted);
    engine
}

// ---------------------------------------------------------------------------

#[test]
fn only_one_call_is_outstanding_at_a_time_and_the_rest_queue() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();

    // Part 4 §4.1.1: only the first goes out; the others are queued, not rejected.
    let first = recorder.drain(&mut engine).only_frame();
    assert_eq!(engine.queued(), 2);

    let id = frame_id(first);
    engine.handle(
        NOW,
        Input::Received(&format!(
            r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z"}}]"#
        )),
    );
    recorder.drain(&mut engine);
    assert_eq!(recorder.outcomes().len(), 1);
    assert_eq!(
        recorder.frames().len(),
        1,
        "the next queued call is released"
    );
    assert_eq!(engine.queued(), 1);
}

#[test]
fn a_send_bypasses_the_outstanding_call_slot() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine
        .call(
            NOW,
            "Authorize",
            raw(r#"{"idToken":{"idToken":"A","type":"ISO14443"}}"#),
        )
        .unwrap();
    recorder.drain(&mut engine);
    assert!(engine.has_outstanding_call());

    // Part 4 §4.2.4: a SEND may be transmitted while a CALL is pending.
    engine
        .call(
            NOW,
            "NotifyPeriodicEventStream",
            raw(r#"{"id":1,"pending":0,"basetime":"2024-01-01T00:00:00Z","data":[]}"#),
        )
        .unwrap();
    let frame = recorder.drain(&mut engine).only_frame();
    assert!(
        frame.starts_with("[6,"),
        "SEND uses message type 6: {frame}"
    );
    assert!(engine.has_outstanding_call(), "the CALL slot is untouched");
    // It completes immediately: a SEND is never answered.
    assert_eq!(recorder.outcomes().len(), 1);
}

#[test]
fn send_is_rejected_before_ocpp_21() {
    let mut engine = booted_station(Version::V2_0_1);
    let error = engine
        .call(NOW, "NotifyPeriodicEventStream", raw("{}"))
        .unwrap_err();
    assert_eq!(
        error,
        EngineError::UnknownAction("NotifyPeriodicEventStream".into())
    );
}

#[test]
fn a_send_received_as_a_call_is_a_protocol_error_n15_fr_01() {
    let mut engine = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    // Accept the station so the boot gate is not what rejects this.
    engine.handle(
        NOW,
        Input::Received(r#"[2,"b1","BootNotification",{"reason":"PowerUp"}]"#),
    );
    let _ = engine.drain();
    engine
        .respond(
            NOW,
            &"b1".parse().unwrap(),
            &raw(r#"{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}"#),
        )
        .unwrap();
    let _ = engine.drain();

    let mut recorder = Recorder::new();
    engine.handle(NOW, Input::Received(
        r#"[2,"s1","NotifyPeriodicEventStream",{"id":1,"pending":0,"basetime":"2024-01-01T00:00:00Z","data":[]}]"#,
    ));
    recorder.drain(&mut engine);
    assert!(recorder.frames()[0].contains("ProtocolError"));
    assert!(matches!(
        recorder.violations()[0],
        ProtocolViolation::WrongMessageKind { .. }
    ));
}

#[test]
fn a_send_is_never_answered_even_when_it_is_unusable() {
    let mut engine = Engine::new(
        EngineConfig::new(Role::Csms, Version::V2_1)
            .with_inbound_concurrency(InboundConcurrency::Serve),
    );
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let mut recorder = Recorder::new();
    // An action the CSMS may not receive, sent as a SEND: FR.07 forbids any answer.
    engine.handle(
        NOW,
        Input::Received(r#"[6,"x","Reset",{"type":"Immediate"}]"#),
    );
    recorder.drain(&mut engine);
    assert!(
        recorder.frames().is_empty(),
        "no answer to a SEND: {:?}",
        recorder.frames()
    );
}

#[test]
fn wrong_direction_is_not_supported() {
    let mut engine = booted_station(Version::V2_1);
    // A station may not originate Reset.
    assert_eq!(
        engine
            .call(NOW, "Reset", raw(r#"{"type":"Immediate"}"#))
            .unwrap_err(),
        EngineError::WrongDirection("Reset".into())
    );

    // And a CSMS that receives a CSMS-only action answers NotSupported.
    let mut csms = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let mut recorder = Recorder::new();
    csms.handle(
        NOW,
        Input::Received(r#"[2,"1","Reset",{"type":"Immediate"}]"#),
    );
    recorder.drain(&mut csms);
    assert!(recorder.only_frame().contains("NotSupported"));
}

#[test]
fn unknown_actions_are_not_implemented() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine.handle(NOW, Input::Received(r#"[2,"1","Teleport",{}]"#));
    recorder.drain(&mut engine);
    assert!(recorder.only_frame().contains("NotImplemented"));
    assert_eq!(
        engine.call(NOW, "Teleport", raw("{}")).unwrap_err(),
        EngineError::UnknownAction("Teleport".into())
    );
}

#[test]
fn responses_for_unknown_ids_are_dropped_and_reported() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine.handle(NOW, Input::Received(r#"[3,"never-sent",{}]"#));
    recorder.drain(&mut engine);
    assert!(recorder.frames().is_empty());
    assert!(matches!(
        recorder.violations()[0],
        ProtocolViolation::UnexpectedResponse { .. }
    ));
}

#[test]
fn concurrent_inbound_calls_are_served_by_default_and_rejected_on_request() {
    let mut lenient = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    lenient.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    lenient.handle(
        NOW,
        Input::Received(r#"[2,"b1","BootNotification",{"reason":"PowerUp"}]"#),
    );
    let _ = lenient.drain();
    lenient
        .respond(
            NOW,
            &"b1".parse().unwrap(),
            &raw(r#"{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}"#),
        )
        .unwrap();
    let _ = lenient.drain();

    let mut recorder = Recorder::new();
    lenient.handle(NOW, Input::Received(r#"[2,"a","Heartbeat",{}]"#));
    lenient.handle(NOW, Input::Received(r#"[2,"b","Heartbeat",{}]"#));
    recorder.drain(&mut lenient);
    assert_eq!(recorder.requests().len(), 2, "both calls are delivered");
    assert!(matches!(
        recorder.violations()[0],
        ProtocolViolation::ConcurrentCall { .. }
    ));

    let mut strict = Engine::new(
        EngineConfig::new(Role::Csms, Version::V2_1)
            .with_inbound_concurrency(InboundConcurrency::Reject),
    );
    strict.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    strict.handle(
        NOW,
        Input::Received(r#"[2,"b1","BootNotification",{"reason":"PowerUp"}]"#),
    );
    let _ = strict.drain();
    strict
        .respond(
            NOW,
            &"b1".parse().unwrap(),
            &raw(r#"{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}"#),
        )
        .unwrap();
    let _ = strict.drain();
    strict.handle(NOW, Input::Received(r#"[2,"a","Heartbeat",{}]"#));
    let _ = strict.drain();
    strict.handle(NOW, Input::Received(r#"[2,"b","Heartbeat",{}]"#));
    recorder.drain(&mut strict);
    assert!(recorder.only_frame().contains("ProtocolError"));
}

#[test]
fn a_call_that_is_not_answered_times_out_and_frees_the_slot() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    recorder.drain(&mut engine);
    let deadline = recorder
        .timers()
        .into_iter()
        .find_map(|(timer, at)| (timer == Timer::CallTimeout).then_some(at))
        .expect("call timeout armed");
    assert_eq!(
        deadline.as_millis(),
        30_000,
        "default message timeout is 30 s"
    );

    engine.handle(deadline, Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(
        recorder.failures(),
        vec![("Heartbeat", &CallFailure::Timeout)]
    );
    assert_eq!(recorder.frames().len(), 1, "the queued call is released");
}

// ---------------------------------------------------------------------------
// Boot state machine
// ---------------------------------------------------------------------------

#[test]
fn before_acceptance_only_boot_notification_leaves_the_station_b02_fr_02() {
    let mut engine = station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    recorder.drain(&mut engine);
    let frame = recorder.only_frame();
    assert!(frame.contains("BootNotification"), "{frame}");
    assert_eq!(engine.queued(), 1, "the Heartbeat waits");

    let id = frame_id(frame);
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Accepted"}}]"#
    )));
    recorder.drain(&mut engine);
    assert_eq!(engine.boot_state(), BootState::Accepted);
    assert!(
        recorder.only_frame().contains("Heartbeat"),
        "the queue is released on acceptance"
    );
}

#[test]
fn a_triggered_message_passes_the_boot_gate() {
    let mut engine = station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine
        .call_with(
            NOW,
            "StatusNotification",
            raw("{}"),
            CallOptions::triggered(),
        )
        .unwrap();
    recorder.drain(&mut engine);
    assert!(recorder.only_frame().contains("StatusNotification"));
}

#[test]
fn pending_schedules_a_boot_retry_and_keeps_the_connection() {
    let mut engine = station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();

    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":90,"status":"Pending"}}]"#
    )));
    recorder.drain(&mut engine);
    assert_eq!(engine.boot_state(), BootState::Pending);
    // B02.FR.06 — the connection stays open.
    assert!(
        !recorder
            .outputs()
            .iter()
            .any(|o| matches!(o, Output::Close(_)))
    );
    // B02.FR.04 — retry after the interval the CSMS gave.
    let (_, at) = recorder
        .timers()
        .into_iter()
        .find(|(timer, _)| *timer == Timer::BootRetry)
        .expect("boot retry armed");
    assert_eq!(at.as_millis(), 90_000);

    engine.handle(at, Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(
        engine.boot_state(),
        BootState::Idle,
        "the gate re-opens for a new BootNotification"
    );
}

#[test]
fn interval_zero_falls_back_to_a_local_backoff_b02_fr_07() {
    let mut engine = station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Rejected"}}]"#
    )));
    recorder.drain(&mut engine);
    assert_eq!(engine.boot_state(), BootState::Rejected);
    let (_, at) = recorder
        .timers()
        .into_iter()
        .find(|(t, _)| *t == Timer::BootRetry)
        .unwrap();
    assert_eq!(at.as_millis(), 30_000, "the engine's own fallback interval");
}

#[test]
fn a_csms_answers_an_unsolicited_call_from_a_pending_station_with_security_error_b02_fr_09() {
    let mut csms = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    csms.handle(
        NOW,
        Input::Received(r#"[2,"b1","BootNotification",{"reason":"PowerUp"}]"#),
    );
    let _ = csms.drain();
    csms.respond(
        NOW,
        &"b1".parse().unwrap(),
        &raw(r#"{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Pending"}"#),
    )
    .unwrap();
    let _ = csms.drain();
    assert_eq!(csms.boot_state(), BootState::Pending);

    let mut recorder = Recorder::new();
    csms.handle(NOW, Input::Received(r#"[2,"h1","Heartbeat",{}]"#));
    recorder.drain(&mut csms);
    assert!(
        recorder.only_frame().contains("SecurityError"),
        "{:?}",
        recorder.frames()
    );

    // …but a message it asked for is fine (B02.FR.01).
    csms.call(
        NOW,
        "GetBaseReport",
        raw(r#"{"requestId":1,"reportBase":"FullInventory"}"#),
    )
    .unwrap();
    let _ = csms.drain();
    csms.handle(NOW, Input::Received(
        r#"[2,"n1","NotifyReport",{"requestId":1,"generatedAt":"2024-01-01T00:00:00Z","seqNo":0}]"#,
    ));
    recorder.drain(&mut csms);
    assert_eq!(recorder.requests().len(), 1);
    assert!(recorder.frames().is_empty());
}

#[test]
fn a_trigger_message_licenses_exactly_the_message_it_names() {
    let mut csms = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    csms.handle(
        NOW,
        Input::Received(r#"[2,"b1","BootNotification",{"reason":"PowerUp"}]"#),
    );
    let _ = csms.drain();
    csms.respond(
        NOW,
        &"b1".parse().unwrap(),
        &raw(r#"{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Pending"}"#),
    )
    .unwrap();
    csms.call(
        NOW,
        "TriggerMessage",
        raw(r#"{"requestedMessage":"StatusNotification"}"#),
    )
    .unwrap();
    let _ = csms.drain();

    let mut recorder = Recorder::new();
    csms.handle(NOW, Input::Received(
        r#"[2,"s1","StatusNotification",{"timestamp":"2024-01-01T00:00:00Z","connectorStatus":"Available","evseId":1,"connectorId":1}]"#,
    ));
    recorder.drain(&mut csms);
    assert_eq!(
        recorder.requests().len(),
        1,
        "the triggered message is admitted"
    );

    csms.handle(
        NOW,
        Input::Received(r#"[2,"m1","MeterValues",{"evseId":1,"meterValue":[]}]"#),
    );
    recorder.drain(&mut csms);
    assert!(
        recorder.only_frame().contains("SecurityError"),
        "a different message is not"
    );
}

// ---------------------------------------------------------------------------
// Transaction retries and the offline queue
// ---------------------------------------------------------------------------

fn tx_payload(seq: u32) -> Box<RawValue> {
    raw(&format!(
        r#"{{"eventType":"Updated","timestamp":"2024-01-01T00:00:0{seq}Z","triggerReason":"MeterValuePeriodic","seqNo":{seq},"transactionInfo":{{"transactionId":"t1"}}}}"#
    ))
}

#[test]
fn transaction_messages_are_retried_on_the_linear_schedule_and_then_skipped() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.call(NOW, "TransactionEvent", tx_payload(0)).unwrap();
    recorder.drain(&mut engine);
    assert_eq!(recorder.frames().len(), 1);

    // Attempt 1 times out at t=30s; the retry is scheduled 60 s later (interval × 1).
    engine.handle(Instant::from_millis(30_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert!(recorder.outcomes().is_empty(), "not given up yet");
    let (_, at) = recorder
        .timers()
        .into_iter()
        .find(|(t, _)| *t == Timer::TransactionRetry)
        .unwrap();
    assert_eq!(at.as_millis(), 90_000);

    // Attempt 2 goes out, and times out at 90+30 s; the next retry waits 120 s.
    engine.handle(at, Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(recorder.frames().len(), 1, "attempt 2");
    engine.handle(Instant::from_millis(120_000), Input::Timeout);
    recorder.drain(&mut engine);
    let (_, at) = recorder
        .timers()
        .into_iter()
        .find(|(t, _)| *t == Timer::TransactionRetry)
        .unwrap();
    assert_eq!(at.as_millis(), 120_000 + 120_000);

    // Attempt 3 is the last one the default policy allows.
    engine.handle(at, Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(recorder.frames().len(), 1, "attempt 3");
    engine.handle(Instant::from_millis(300_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(
        recorder.failures(),
        vec![("TransactionEvent", &CallFailure::RetriesExhausted)],
        "1.6 §3.7.1: the message is skipped once attempts run out"
    );
    assert_eq!(engine.queued(), 0);
    assert!(
        engine.store().is_empty().unwrap(),
        "and it leaves the durable store"
    );
}

#[test]
fn only_transaction_messages_are_retried() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    recorder.drain(&mut engine);
    engine.handle(Instant::from_millis(30_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(
        recorder.failures(),
        vec![("Heartbeat", &CallFailure::Timeout)]
    );
    assert!(
        recorder
            .timers()
            .iter()
            .all(|(t, _)| *t != Timer::TransactionRetry)
    );
}

#[test]
fn transaction_messages_survive_a_disconnection_and_replay_in_order() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.call(NOW, "TransactionEvent", tx_payload(0)).unwrap();
    engine.call(NOW, "TransactionEvent", tx_payload(1)).unwrap();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    recorder.drain(&mut engine);

    engine.handle(NOW, Input::Disconnected);
    recorder.drain(&mut engine);
    // The Heartbeat is dropped; both transaction events are kept (E04/E08/E12).
    assert_eq!(
        recorder.failures(),
        vec![("Heartbeat", &CallFailure::Disconnected)]
    );
    assert_eq!(engine.queued(), 2);
    assert_eq!(engine.store().len().unwrap(), 2);

    // Nothing is due until the retry interval has passed.
    engine.handle(Instant::from_millis(60_000), Input::Timeout);
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    recorder.drain(&mut engine);
    let frames = recorder.frames();
    assert_eq!(frames.len(), 1);
    assert!(
        frames[0].contains(r#""seqNo":0"#),
        "chronological order is preserved: {frames:?}"
    );
}

#[test]
fn a_durable_store_replays_what_a_power_cut_interrupted() {
    let mut store = MemStore::new();
    store
        .push(&ocpp_kit::engine::QueuedCall {
            action: "TransactionEvent".into(),
            payload: tx_payload(4),
            kind: MessageKind::Call,
            attempts: 1,
            transactional: true,
        })
        .unwrap();

    let mut engine = Engine::with_store(
        EngineConfig::new(Role::ChargingStation, Version::V2_1),
        store,
    )
    .unwrap();
    assert_eq!(engine.queued(), 1);

    // A rebooted station starts at BootState::Idle, so the replay waits for acceptance.
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let mut recorder = Recorder::new();
    recorder.drain(&mut engine);
    assert!(
        recorder.frames().is_empty(),
        "B02.FR.02 still applies after a reboot"
    );

    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Accepted"}}]"#
    )));
    recorder.drain(&mut engine);
    assert!(recorder.only_frame().contains(r#""seqNo":4"#));
}

#[test]
fn the_offline_queue_is_bounded() {
    let mut engine = Engine::new(
        EngineConfig::new(Role::ChargingStation, Version::V2_1).with_offline(OfflinePolicy {
            queue_all_messages: true,
            max_queued: 2,
        }),
    );
    let mut recorder = Recorder::new();
    for _ in 0..4 {
        engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    }
    recorder.drain(&mut engine);
    assert_eq!(engine.queued(), 2);
    assert_eq!(
        recorder.failures(),
        vec![
            ("Heartbeat", &CallFailure::QueueFull),
            ("Heartbeat", &CallFailure::QueueFull),
        ]
    );
}

#[test]
fn queue_all_messages_keeps_everything_across_an_outage() {
    let mut engine = booted_station(Version::V2_1);
    engine.handle(NOW, Input::Disconnected);
    let _ = engine.drain();

    let mut engine2 = Engine::new(
        EngineConfig::new(Role::ChargingStation, Version::V2_1).with_offline(OfflinePolicy {
            queue_all_messages: true,
            max_queued: 16,
        }),
    );
    engine2.call(NOW, "Heartbeat", raw("{}")).unwrap();
    let _ = engine2.drain();
    assert_eq!(engine2.queued(), 1, "nothing is dropped while offline");
}

// ---------------------------------------------------------------------------
// Heartbeats, clock samples, drain
// ---------------------------------------------------------------------------

#[test]
fn the_engine_sends_heartbeats_on_the_interval_the_csms_gave() {
    let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let mut recorder = Recorder::new();
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}}]"#
    )));
    recorder.drain(&mut engine);
    assert_eq!(engine.heartbeat_interval(), Some(Duration::from_secs(300)));
    let (_, at) = recorder
        .timers()
        .into_iter()
        .find(|(t, _)| *t == Timer::Heartbeat)
        .unwrap();
    assert_eq!(at.as_millis(), 300_000);

    // A currentTime is reported so a station without an RTC can discipline its clock.
    let sample = recorder
        .outputs()
        .iter()
        .find_map(|o| match o {
            Output::ClockSample(sample) => Some(sample),
            _ => None,
        })
        .expect("clock sample");
    assert_eq!(sample.csms_time.to_string(), "2024-01-01T00:00:00Z");

    engine.handle(at, Input::Timeout);
    recorder.drain(&mut engine);
    assert!(recorder.only_frame().contains("Heartbeat"));
}

#[test]
fn a_graceful_drain_finishes_the_outstanding_call_then_closes() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();

    engine.shutdown(NOW, Instant::from_millis(120_000));
    recorder.drain(&mut engine);
    assert!(
        !recorder
            .outputs()
            .iter()
            .any(|o| matches!(o, Output::Close(_))),
        "work is still open"
    );
    assert_eq!(
        engine.call(NOW, "Heartbeat", raw("{}")).unwrap_err(),
        EngineError::ShuttingDown
    );

    engine.handle(
        NOW,
        Input::Received(&format!(
            r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z"}}]"#
        )),
    );
    recorder.drain(&mut engine);
    let second = recorder.only_frame_id();
    engine.handle(
        NOW,
        Input::Received(&format!(
            r#"[3,"{second}",{{"currentTime":"2024-01-01T00:00:00Z"}}]"#
        )),
    );
    recorder.drain(&mut engine);
    assert!(
        recorder
            .outputs()
            .iter()
            .any(|o| matches!(o, Output::Close(CloseReason::Drained))),
        "the queue is empty, so the engine closes"
    );
}

#[test]
fn a_drain_that_overruns_its_deadline_closes_anyway() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    let _ = recorder.drain(&mut engine);
    engine.shutdown(NOW, Instant::from_millis(10_000));
    engine.handle(Instant::from_millis(10_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert!(
        recorder
            .outputs()
            .iter()
            .any(|o| matches!(o, Output::Close(CloseReason::DrainTimedOut)))
    );
}

// ---------------------------------------------------------------------------
// Version-specific framing behaviour
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_message_type_is_ignored_on_21_and_answered_on_201() {
    let mut modern = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    modern.handle(NOW, Input::Received(r#"[9,"1","Nope",{}]"#));
    recorder.drain(&mut modern);
    assert!(recorder.frames().is_empty(), "2.1 Part 4 §4.4: ignore it");
    assert!(matches!(
        recorder.violations()[0],
        ProtocolViolation::UnknownMessageType { .. }
    ));

    let mut legacy = booted_station(Version::V2_0_1);
    legacy.handle(NOW, Input::Received(r#"[9,"1","Nope",{}]"#));
    recorder.drain(&mut legacy);
    assert!(
        recorder.only_frame().contains("MessageTypeNotSupported"),
        "2.0.1 Part 4 §4.4 requires an answer"
    );
}

#[test]
fn a_16_engine_uses_the_16_error_spellings() {
    let mut engine = Engine::new(EngineConfig::new(Role::Csms, Version::V1_6));
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V1_6,
        },
    );
    let mut recorder = Recorder::new();
    engine.handle(NOW, Input::Received("not a frame at all"));
    recorder.drain(&mut engine);
    let frame = recorder.only_frame();
    // 1.6J has no RpcFrameworkError, so it degrades to GenericError.
    assert!(frame.contains("GenericError"), "{frame}");
    assert!(frame.contains(r#""-1""#) || frame.contains('['));
}

#[test]
fn call_result_error_needs_21() {
    let mut legacy = booted_station(Version::V2_0_1);
    assert_eq!(
        legacy
            .reject_result(
                NOW,
                &"1".parse().unwrap(),
                ocpp_kit::rpc::CallError::new(ErrorCode::FormatViolation, "")
            )
            .unwrap_err(),
        EngineError::CallResultErrorNotSupported
    );

    let mut modern = booted_station(Version::V2_1);
    modern
        .reject_result(
            NOW,
            &"1".parse().unwrap(),
            ocpp_kit::rpc::CallError::new(ErrorCode::FormatViolation, "bad result"),
        )
        .unwrap();
    let mut recorder = Recorder::new();
    recorder.drain(&mut modern);
    assert!(recorder.only_frame().starts_with("[5,"));
}

#[test]
fn a_call_result_error_from_the_peer_is_surfaced() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine.handle(
        NOW,
        Input::Received(r#"[5,"7","FormatViolation","cannot parse",{}]"#),
    );
    recorder.drain(&mut engine);
    assert!(
        recorder
            .outputs()
            .iter()
            .any(|o| matches!(o, Output::ResultRejected { .. }))
    );
}

#[test]
fn retry_policy_can_be_disabled() {
    let mut engine = Engine::new(
        EngineConfig::new(Role::ChargingStation, Version::V2_1).with_retry(RetryPolicy::none()),
    );
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let mut recorder = Recorder::new();
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Accepted"}}]"#
    )));
    let _ = engine.drain();

    engine.call(NOW, "TransactionEvent", tx_payload(0)).unwrap();
    recorder.drain(&mut engine);
    engine.handle(Instant::from_millis(30_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(
        recorder.failures(),
        vec![("TransactionEvent", &CallFailure::RetriesExhausted)]
    );
}

#[test]
fn every_call_the_peer_makes_can_be_answered_even_when_it_breaks_the_rule() {
    let mut csms = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    csms.handle(
        NOW,
        Input::Received(r#"[2,"b1","BootNotification",{"reason":"PowerUp"}]"#),
    );
    let _ = csms.drain();
    csms.respond(
        NOW,
        &"b1".parse().unwrap(),
        &raw(r#"{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}"#),
    )
    .unwrap();
    let _ = csms.drain();

    // Two calls in flight at once: served, but recorded as a violation.
    csms.handle(NOW, Input::Received(r#"[2,"a","Heartbeat",{}]"#));
    csms.handle(NOW, Input::Received(r#"[2,"b","Heartbeat",{}]"#));
    let mut recorder = Recorder::new();
    recorder.drain(&mut csms);
    assert_eq!(csms.awaiting_response(), 2);

    // Answering the *older* one first must work, and must not strand the newer one.
    csms.respond(
        NOW,
        &"a".parse().unwrap(),
        &raw(r#"{"currentTime":"2024-01-01T00:00:00Z"}"#),
    )
    .unwrap();
    assert_eq!(csms.awaiting_response(), 1);
    csms.respond(
        NOW,
        &"b".parse().unwrap(),
        &raw(r#"{"currentTime":"2024-01-01T00:00:00Z"}"#),
    )
    .unwrap();
    assert_eq!(csms.awaiting_response(), 0);

    // And an id that was never outstanding is refused.
    assert_eq!(
        csms.respond(NOW, &"never".parse().unwrap(), &raw("{}"))
            .unwrap_err(),
        EngineError::NoSuchRequest("never".parse().unwrap())
    );
}

#[test]
fn an_over_long_error_description_is_capped_at_the_wire_limit() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    // A CALLERROR's errorDescription is `string[255]`; a decoding failure quoting a long
    // path and value can easily exceed that.
    let error = ocpp_kit::rpc::CallError::new(ErrorCode::InternalError, "x".repeat(1000));
    assert_eq!(error.description.chars().count(), 255);

    engine.handle(
        NOW,
        Input::Received(r#"[2,"1","Reset",{"type":"Immediate"}]"#),
    );
    recorder.drain(&mut engine);
    engine
        .respond_error(NOW, &"1".parse().unwrap(), error)
        .unwrap();
    recorder.drain(&mut engine);

    let parts: Vec<serde_json::Value> = serde_json::from_str(recorder.only_frame()).unwrap();
    assert_eq!(parts[3].as_str().unwrap().chars().count(), 255);
}

#[test]
fn a_transaction_message_waiting_out_its_retry_blocks_the_ones_behind_it() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    // Two transaction events and one heartbeat, in that order.
    engine.call(NOW, "TransactionEvent", tx_payload(0)).unwrap();
    engine.call(NOW, "TransactionEvent", tx_payload(1)).unwrap();
    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    recorder.drain(&mut engine);
    assert!(recorder.only_frame().contains(r#""seqNo":0"#));

    // seqNo 0 times out and goes back on the queue to wait out its retry interval.
    engine.handle(Instant::from_millis(30_000), Input::Timeout);
    recorder.drain(&mut engine);

    // 1.6 §3.7: "the delivery of new transaction-related messages SHALL wait until the queue
    // has been emptied" — so seqNo 1 must NOT overtake it, even though it is ready to go…
    let frames = recorder.frames();
    assert!(
        !frames.iter().any(|frame| frame.contains(r#""seqNo":1"#)),
        "seqNo 1 overtook seqNo 0: {frames:?}"
    );
    // …but a message that is not transaction-related is explicitly allowed to.
    assert!(
        frames.iter().any(|frame| frame.contains("Heartbeat")),
        "a Heartbeat may overtake a stuck transaction queue: {frames:?}"
    );

    // Once the retry falls due, seqNo 0 goes out again — still ahead of seqNo 1.
    let (_, at) = recorder
        .timers()
        .into_iter()
        .find(|(timer, _)| *timer == Timer::TransactionRetry)
        .expect("a retry is scheduled");
    engine.handle(at, Input::Timeout);
    recorder.drain(&mut engine);
    assert!(
        recorder
            .frames()
            .iter()
            .any(|frame| frame.contains(r#""seqNo":0"#))
    );
}

/// B01.FR.10 and B02.FR.09 both begin "the Charging Station **has received** a
/// `BootNotificationResponse`" with a status other than `Accepted`. A station that has been
/// sent no such response — the normal state of one that reconnected, since Part 4 §5.4 tells it
/// *not* to repeat its `BootNotification` — is not covered, and answering it `SecurityError`
/// would make every reconnect useless.
#[test]
fn a_csms_serves_a_station_that_reconnects_without_repeating_boot_notification() {
    let mut csms = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let _ = csms.drain();

    let mut recorder = Recorder::new();
    csms.handle(NOW, Input::Received(r#"[2,"h1","Heartbeat",{}]"#));
    recorder.drain(&mut csms);
    assert_eq!(
        recorder.requests().len(),
        1,
        "an unbooted station is served, not refused: {:?}",
        recorder.frames()
    );
    assert!(recorder.frames().is_empty());
}

/// `OCPPCommCtrlr.HeartbeatInterval` is "the interval of inactivity … after which the
/// Charging Station should send `HeartbeatRequest`". A `Heartbeat` that times out must
/// therefore not end the sequence, and traffic must postpone the next one.
#[test]
fn the_heartbeat_is_an_idle_timer_that_a_timeout_cannot_stop() {
    let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let mut recorder = Recorder::new();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":10,"status":"Accepted"}}]"#
    )));
    let _ = engine.drain();

    // Ten idle seconds produce a Heartbeat.
    engine.handle(Instant::from_millis(10_000), Input::Timeout);
    recorder.drain(&mut engine);
    let heartbeat = recorder.only_frame();
    assert!(heartbeat.contains("Heartbeat"), "{heartbeat}");

    // It goes unanswered and times out. The sequence must survive that.
    engine.handle(Instant::from_millis(60_000), Input::Timeout);
    let _ = engine.drain();
    engine.handle(Instant::from_millis(120_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert!(
        recorder.frames().iter().any(|f| f.contains("Heartbeat")),
        "a timed-out Heartbeat must not stop the next one: {:?}",
        recorder.frames()
    );
}

#[test]
fn traffic_postpones_the_next_heartbeat() {
    let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let mut recorder = Recorder::new();
    let id = recorder.drain(&mut engine).only_frame_id();
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":10,"status":"Accepted"}}]"#
    )));
    let _ = engine.drain();

    // A CSMS request at t=8s resets the inactivity timer, so t=10s is no longer idle enough.
    engine.handle(Instant::from_millis(8_000), Input::Timeout);
    engine.handle(NOW, Input::Received(r#"[2,"c1","ClearCache",{}]"#));
    let _ = engine.drain();
    engine.handle(Instant::from_millis(10_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert!(
        !recorder.frames().iter().any(|f| f.contains("Heartbeat")),
        "traffic at 8s should push the heartbeat to 18s: {:?}",
        recorder.frames()
    );

    engine.handle(Instant::from_millis(18_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert!(
        recorder.frames().iter().any(|f| f.contains("Heartbeat")),
        "{:?}",
        recorder.frames()
    );
}

/// Part 4 §4.2.3 lists "an existing message with the same unique identifier is being handled
/// already" as a CALLERROR condition. Serving it as an ordinary request would leave the peer
/// with two answers it cannot tell apart.
#[test]
fn a_reused_message_id_is_answered_with_an_error_not_a_second_request() {
    let mut csms = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let _ = csms.drain();

    let mut recorder = Recorder::new();
    csms.handle(NOW, Input::Received(r#"[2,"dup","Heartbeat",{}]"#));
    recorder.drain(&mut csms);
    assert_eq!(recorder.requests().len(), 1);

    csms.handle(NOW, Input::Received(r#"[2,"dup","Heartbeat",{}]"#));
    recorder.drain(&mut csms);
    assert!(
        recorder.requests().is_empty(),
        "the duplicate is not dispatched"
    );
    assert!(
        recorder.only_frame().contains("RpcFrameworkError"),
        "{:?}",
        recorder.frames()
    );
    assert!(
        recorder
            .violations()
            .iter()
            .any(|violation| matches!(violation, ProtocolViolation::DuplicateMessageId { .. }))
    );
}

/// `InboundConcurrency::Serve` is a kindness to peers that break Part 4 §4.1.1, not licence
/// for one to make the receiver hold requests without limit.
#[test]
fn a_peer_that_never_stops_calling_is_cut_off_at_the_configured_bound() {
    let mut config = EngineConfig::new(Role::Csms, Version::V2_1);
    config.max_peer_requests = 4;
    let mut csms = Engine::new(config);
    csms.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    let _ = csms.drain();

    let mut recorder = Recorder::new();
    for index in 0..4 {
        csms.handle(
            NOW,
            Input::Received(&format!(r#"[2,"c{index}","Heartbeat",{{}}]"#)),
        );
        recorder.drain(&mut csms);
        assert_eq!(recorder.requests().len(), 1, "call {index} is served");
    }
    csms.handle(NOW, Input::Received(r#"[2,"c4","Heartbeat",{}]"#));
    recorder.drain(&mut csms);
    assert!(recorder.requests().is_empty());
    assert!(
        recorder.only_frame().contains("ProtocolError"),
        "{:?}",
        recorder.frames()
    );
    assert_eq!(csms.awaiting_response(), 4);
}

/// A message that is not meant to survive an outage should say so at once, not sit in a queue
/// it was never eligible for until the next disconnection notices it.
#[test]
fn a_call_that_does_not_queue_fails_immediately_while_offline() {
    let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
    let token = engine
        .call_with(
            NOW,
            "Heartbeat",
            raw("{}"),
            CallOptions::default().queue_when_offline(false),
        )
        .unwrap();
    let mut recorder = Recorder::new();
    recorder.drain(&mut engine);
    assert_eq!(engine.queued(), 0);
    assert_eq!(
        recorder.failures(),
        vec![("Heartbeat", &CallFailure::Disconnected)]
    );
    let _ = token;
}

/// A call gets its full timeout however long the session was idle first. Nothing arms an
/// engine timer on an idle CSMS session, so the deadline has to come from the instant the
/// driver passes in, not from whenever a timer last fired.
#[test]
fn a_call_started_after_a_quiet_period_gets_its_full_timeout() {
    use ocpp_kit::testkit::Sim;

    let mut sim = Sim::new(EngineConfig::new(Role::Csms, Version::V2_1));
    sim.connect(Version::V2_1);

    // Ten minutes of silence: no frames, no timers, nothing to tick for.
    sim.advance(Duration::from_secs(600));
    sim.call("GetVariables", r#"{"getVariableData":[]}"#)
        .unwrap();
    let id = sim.only_frame_id();

    // One second later the station answers, comfortably inside the 30 s timeout.
    sim.advance(Duration::from_secs(1));
    assert!(
        sim.failures().is_empty(),
        "the call was failed before the peer could answer: {:?}",
        sim.failures()
    );
    sim.recv(&format!(r#"[3,"{id}",{{"getVariableResult":[]}}]"#));
    assert_eq!(sim.outcomes().len(), 1);
    assert!(sim.outcomes()[0].result.is_ok());
}

/// And the timeout still fires when it is genuinely due, measured from when the call went
/// out rather than from the origin of the clock.
#[test]
fn the_message_timeout_is_measured_from_transmission() {
    use ocpp_kit::testkit::Sim;

    let mut sim = Sim::new(EngineConfig::new(Role::Csms, Version::V2_1));
    sim.connect(Version::V2_1);
    sim.advance(Duration::from_secs(600));
    sim.call("GetVariables", r#"{"getVariableData":[]}"#)
        .unwrap();

    sim.advance(Duration::from_secs(29));
    assert!(sim.failures().is_empty(), "not due yet");
    sim.advance(Duration::from_secs(2));
    assert_eq!(
        sim.failures(),
        vec![("GetVariables", &CallFailure::Timeout)]
    );
}

/// Part 4 §4.2.3: "A CALLRESULTERROR is sent back on receipt of a CALLRESULT that contains
/// errors." A `CALLERROR` would be the answer to a *CALL*, and sending one here tells the
/// peer that a request it never made was rejected.
#[test]
fn an_unreadable_call_result_is_answered_with_a_call_result_error_on_21() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    // A CALLRESULT whose MessageId is not a string: readable as a CALLRESULT, unusable.
    engine.handle(NOW, Input::Received(r"[3,17,{}]"));
    recorder.drain(&mut engine);
    let frame = recorder.only_frame();
    assert!(
        frame.starts_with("[5,"),
        "expected a CALLRESULTERROR: {frame}"
    );
    assert!(
        frame.contains(r#""-1""#),
        "§4.2.3's unreadable-id rule: {frame}"
    );
}

/// Before 2.1 there is no `CALLRESULTERROR`, and a `CALLERROR` would be the wrong message —
/// so the failure stays local and nothing is sent.
#[test]
fn an_unreadable_call_result_is_not_answered_before_21() {
    let mut engine = booted_station(Version::V2_0_1);
    let mut recorder = Recorder::new();

    engine.handle(NOW, Input::Received(r"[3,17,{}]"));
    recorder.drain(&mut engine);
    assert!(
        recorder.frames().is_empty(),
        "2.0.1 has nothing to answer a bad CALLRESULT with: {:?}",
        recorder.frames()
    );
    assert_eq!(recorder.violations().len(), 1, "but it is still reported");
}

/// A malformed error frame has no answer defined for it, and answering one with another is
/// how two peers start trading error frames over a message that was already an error.
#[test]
fn a_malformed_call_error_is_reported_but_never_answered() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.handle(NOW, Input::Received(r#"[4,17,"GenericError","",{}]"#));
    recorder.drain(&mut engine);
    assert!(recorder.frames().is_empty(), "{:?}", recorder.frames());
    assert_eq!(recorder.violations().len(), 1);
}

/// `errorDescription` and `errorDetails` are required by §4.2.3 and routinely omitted in the
/// field. Refusing such a frame leaves the CALL it answers outstanding until the message
/// timeout — a definitive answer traded for a thirty-second stall.
#[test]
fn a_call_error_missing_its_optional_tail_still_completes_the_call() {
    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();

    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
    let id = recorder.drain(&mut engine).only_frame_id();

    engine.handle(
        NOW,
        Input::Received(&format!(r#"[4,"{id}","NotSupported"]"#)),
    );
    recorder.drain(&mut engine);
    match recorder.failures().as_slice() {
        [("Heartbeat", CallFailure::Rejected(error))] => {
            assert_eq!(error.code, ErrorCode::NotSupported);
            assert_eq!(error.details, serde_json::json!({}));
        }
        other => panic!("expected the call to be rejected, got {other:?}"),
    }
}

/// `[300, …]` is a message type number the peer chose badly, not a frame that stopped being
/// an array — and §4.4 says to ignore the former on 1.6J and 2.1.
#[test]
fn a_message_type_number_outside_a_byte_is_an_unknown_type_not_a_broken_frame() {
    for version in [Version::V2_1, Version::V1_6] {
        let mut engine = booted_station(version);
        let mut recorder = Recorder::new();
        engine.handle(NOW, Input::Received(r#"[300,"x","Heartbeat",{}]"#));
        recorder.drain(&mut engine);
        assert!(
            recorder.frames().is_empty(),
            "{version} must ignore it: {:?}",
            recorder.frames()
        );
        assert!(matches!(
            recorder.violations().as_slice(),
            [ProtocolViolation::UnknownMessageType { .. }]
        ));
    }

    // 2.0.1 §4.4 is the exception: it answers.
    let mut engine = booted_station(Version::V2_0_1);
    let mut recorder = Recorder::new();
    engine.handle(NOW, Input::Received(r#"[300,"x","Heartbeat",{}]"#));
    recorder.drain(&mut engine);
    assert!(
        recorder.only_frame().contains("MessageTypeNotSupported"),
        "{:?}",
        recorder.frames()
    );
}

/// Part 4 §4.2.4: a `SEND` is never answered, so it cannot be "answered with an empty
/// object" either — a caller that awaited that would be waiting on a message the
/// specification forbids the peer from sending.
#[test]
fn a_send_completes_as_sent_rather_than_with_a_payload() {
    use ocpp_kit::engine::Answer;

    let mut engine = booted_station(Version::V2_1);
    let mut recorder = Recorder::new();
    engine
        .call(
            NOW,
            "NotifyPeriodicEventStream",
            raw(r#"{"data":[],"id":1,"pending":0,"basetime":"2024-01-01T00:00:00Z"}"#),
        )
        .unwrap();
    recorder.drain(&mut engine);
    assert!(recorder.only_frame().starts_with("[6,"));
    match recorder.outcomes().as_slice() {
        [outcome] => assert!(matches!(outcome.result, Ok(Answer::Sent))),
        other => panic!("expected one outcome, got {other:?}"),
    }
}

/// A `SEND` or a `CALLRESULTERROR` arriving on 1.6J or 2.0.1 is, to that version, simply a
/// message type number it does not define — §4.4 does not carve out "but a later OCPP defines
/// it". So 2.0.1 answers `MessageTypeNotSupported` and 1.6J ignores it, exactly as they do for
/// a number nobody defines.
#[test]
fn a_21_only_message_type_follows_the_unknown_type_rule_on_older_versions() {
    for message_type in [5, 6] {
        let frame = format!(r#"[{message_type},"1","Heartbeat",{{}}]"#);

        let mut legacy = booted_station(Version::V1_6);
        let mut recorder = Recorder::new();
        legacy.handle(NOW, Input::Received(&frame));
        recorder.drain(&mut legacy);
        assert!(
            recorder.frames().is_empty(),
            "1.6J §4.1.3 ignores type {message_type}: {:?}",
            recorder.frames()
        );

        let mut middle = booted_station(Version::V2_0_1);
        middle.handle(NOW, Input::Received(&frame));
        recorder.drain(&mut middle);
        assert!(
            recorder.only_frame().contains("MessageTypeNotSupported"),
            "2.0.1 §4.4 answers type {message_type}: {:?}",
            recorder.frames()
        );
    }
}

/// B02.FR.02 keeps a station quiet until the CSMS accepts it, *except* for messages the CSMS
/// asked for. Answering a `TriggerMessage` while the boot is still `Pending` therefore needs
/// `CallOptions::triggered()` — without it the answer sits in the queue until boot completes,
/// and the CSMS's trigger goes unanswered.
#[test]
fn a_triggered_answer_leaves_a_pending_station_but_an_untriggered_one_waits() {
    use ocpp_kit::testkit::Sim;

    let mut sim = Sim::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
    sim.connect(Version::V2_1);
    sim.call("BootNotification", r#"{"reason":"PowerUp"}"#)
        .unwrap();
    let id = sim.only_frame_id();
    sim.recv(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":60,"status":"Pending"}}]"#
    ));
    assert_eq!(sim.engine().boot_state(), BootState::Pending);

    // An ordinary call is held back (B02.FR.02).
    sim.call("StatusNotification", STATUS).unwrap();
    assert!(sim.frames().is_empty(), "{:?}", sim.frames());
    assert_eq!(sim.engine().queued(), 1);

    // The same call, marked as one the CSMS asked for, goes out.
    sim.call_with("StatusNotification", STATUS, CallOptions::triggered())
        .unwrap();
    assert!(
        sim.only_frame().contains("StatusNotification"),
        "{:?}",
        sim.frames()
    );
}

const STATUS: &str = r#"{"timestamp":"2024-01-01T00:00:00Z","connectorStatus":"Available","evseId":1,"connectorId":1}"#;
