//! The testkit, used the way a downstream crate would use it.
//!
//! These are not tests *of* the testkit so much as proof that it is usable: if writing them
//! is awkward here, it will be awkward everywhere.

#![cfg(feature = "testkit")]

use std::time::Duration;

use ocpp_kit::Version;
use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Role, Timer};
use ocpp_kit::testkit::Recorder;

/// The engine takes the driver's clock on every entry point. A test that is not
/// exercising a timer supplies the origin and moves on.
const NOW: Instant = Instant::ZERO;

fn raw(json: &str) -> Box<ocpp_kit::RawValue> {
    ocpp_kit::RawValue::from_string(json.to_string()).unwrap()
}

#[test]
fn a_recorder_answers_the_questions_an_engine_test_actually_asks() {
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
    recorder.drain(&mut engine);

    // The frame, and the id the engine minted for it — which a test has no other way to learn.
    assert!(recorder.only_frame().contains("BootNotification"));
    let id = recorder.only_frame_id();
    assert!(recorder.timer(Timer::CallTimeout).is_some());

    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":300,"status":"Accepted"}}]"#
    )));
    recorder.drain(&mut engine);

    let outcomes = recorder.outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].action, "BootNotification");
    assert!(outcomes[0].result.is_ok());
    assert!(recorder.failures().is_empty());

    // A peer that breaks a rule shows up on its own channel rather than as a failure.
    engine.handle(NOW, Input::Received(r#"[3,"never-sent",{}]"#));
    recorder.drain(&mut engine);
    assert_eq!(recorder.violations().len(), 1);
}

#[test]
fn a_recorder_makes_a_timing_rule_readable() {
    let mut engine = Engine::new(
        EngineConfig::new(Role::ChargingStation, Version::V2_1)
            .with_call_timeout(Duration::from_secs(30)),
    );
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
    recorder.drain(&mut engine);
    assert_eq!(
        recorder.timer(Timer::CallTimeout),
        Some(Instant::from_millis(30_000))
    );

    engine.handle(Instant::from_millis(30_000), Input::Timeout);
    recorder.drain(&mut engine);
    assert_eq!(
        recorder.failures(),
        vec![("BootNotification", &ocpp_kit::engine::CallFailure::Timeout)]
    );
}

#[cfg(feature = "tokio")]
mod peers {
    use super::Duration;
    use ocpp_kit::Version;
    use ocpp_kit::testkit::{MockCsms, MockStation};

    /// The shape of an integration test someone writing a station would write: point it at a
    /// CSMS that behaves, and assert on what the CSMS saw.
    #[tokio::test]
    async fn a_station_can_be_tested_against_a_csms_in_two_lines() {
        let csms = MockCsms::start().await.expect("the mock CSMS starts");
        let station = MockStation::connect(csms.url(), "CS-0001").expect("the station connects");

        let boot = station.boot(Version::V2_1).await.expect("boot succeeds");
        assert!(boot.get().contains("Accepted"), "{}", boot.get());

        assert!(
            csms.wait_for("BootNotification", Duration::from_secs(5))
                .await
        );
        let exchange = csms
            .exchanges()
            .into_iter()
            .find(|exchange| exchange.action == "BootNotification")
            .expect("the CSMS recorded it");
        assert_eq!(exchange.identity.as_str(), "CS-0001");
        assert!(exchange.payload.contains("PowerUp"));

        station.shutdown(Duration::from_secs(2)).await;
    }

    /// And the mirror image: someone writing a CSMS wants a station that answers, so they can
    /// test the calls their CSMS makes.
    #[tokio::test]
    async fn a_csms_can_call_a_mock_station_back() {
        let csms = MockCsms::start().await.expect("the mock CSMS starts");
        let station = MockStation::connect(csms.url(), "CS-0002").expect("the station connects");
        station.boot(Version::V2_1).await.expect("boot succeeds");

        let identity = station.identity().clone();
        let session = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(session) = csms.handle().session(&identity).await {
                    return session;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the session appears in the router");

        let reset = session
            .call(ocpp_kit::v2_1::ResetRequest::new(
                ocpp_kit::v2_1::ResetEnum::Immediate,
            ))
            .await
            .expect("the station answers");
        assert_eq!(reset.status, ocpp_kit::v2_1::ResetStatus::Accepted);
        assert!(station.saw("Reset"));
    }

    /// A mock that answers `CALLERROR` is how the failure paths get exercised.
    #[tokio::test]
    async fn a_mock_can_be_made_to_refuse() {
        use ocpp_kit::rpc::CallError;

        let csms = MockCsms::builder()
            .answer(|_, request| Err(CallError::not_supported(&request.action)))
            .start()
            .await
            .expect("the mock CSMS starts");
        let station = MockStation::connect(csms.url(), "CS-0003").expect("the station connects");

        let error = station
            .boot(Version::V2_1)
            .await
            .expect_err("the mock refuses everything");
        assert_eq!(error.code, ocpp_kit::rpc::ErrorCode::NotSupported);
    }
}

/// `Sim` is the crate's own answer to "how do I test a timing rule", so it is checked the
/// same way anything else is: by pinning a rule with it.
#[test]
fn a_sim_reads_as_a_transcript_and_drives_the_clock() {
    use ocpp_kit::engine::{BootState, Timer};
    use ocpp_kit::testkit::Sim;

    let mut sim = Sim::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
    sim.connect(Version::V2_1);
    sim.call("BootNotification", r#"{"reason":"PowerUp"}"#)
        .unwrap();
    let id = sim.only_frame_id();

    // The CSMS says `Pending` with a 60-second interval (B02.FR.04).
    sim.recv(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":60,"status":"Pending"}}]"#
    ));
    assert_eq!(sim.engine().boot_state(), BootState::Pending);
    assert_eq!(
        sim.armed_at(Timer::BootRetry),
        Some(Instant::from_millis(60_000))
    );

    // Nothing may go out while the CSMS is still configuring the station (B02.FR.02).
    sim.call("Heartbeat", "{}").unwrap();
    assert!(sim.frames().is_empty(), "{:?}", sim.frames());

    // Jump to whatever is due next rather than guessing an interval.
    assert!(sim.advance_to_next_timer());
    assert_eq!(sim.engine().boot_state(), BootState::Idle);
    assert_eq!(sim.now(), Instant::from_millis(60_000));
}
