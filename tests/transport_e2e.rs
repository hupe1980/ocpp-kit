//! End-to-end tests over real sockets: a Charging Station, a CSMS, and a Local Controller
//! between them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ocpp_kit::RawValue;
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{
    Admission, Auth, AuthOutcome, BoxFuture, Csms, CsmsHandle, Ctx, Direction, Ended, Handle,
    Handler, LocalController, NetworkProfile, NetworkProfiles, Refusal, RelayDecision,
    SecurityProfile, SessionEvent, Station, TransportError,
};
use ocpp_kit::types::Identity;
use ocpp_kit::version::NegotiationError;
use ocpp_kit::{Version, v2_1};
use tokio::net::TcpListener;

/// A CSMS that accepts boot notifications and counts what it saw.
struct CsmsSide {
    boots: AtomicUsize,
    transaction_events: AtomicUsize,
    boot_status: v2_1::RegistrationStatus,
}

impl CsmsSide {
    fn new(boot_status: v2_1::RegistrationStatus) -> Self {
        Self {
            boots: AtomicUsize::new(0),
            transaction_events: AtomicUsize::new(0),
            boot_status,
        }
    }
}

impl Handler for CsmsSide {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            match ctx.decode::<v2_1::CsRequest>(&request)? {
                v2_1::CsRequest::BootNotification(_) => {
                    self.boots.fetch_add(1, Ordering::SeqCst);
                    ctx.reply(&v2_1::BootNotificationResponse::new(
                        ocpp_kit::types::DateTime::now(),
                        0,
                        self.boot_status.clone(),
                    ))
                }
                v2_1::CsRequest::Heartbeat(_) => ctx.reply(&v2_1::HeartbeatResponse::new(
                    ocpp_kit::types::DateTime::now(),
                )),
                v2_1::CsRequest::StatusNotification(_) => {
                    ctx.reply(&v2_1::StatusNotificationResponse::new())
                }
                v2_1::CsRequest::TransactionEvent(_) => {
                    self.transaction_events.fetch_add(1, Ordering::SeqCst);
                    ctx.reply(&v2_1::TransactionEventResponse::new())
                }
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

/// A Charging Station that answers `Reset` and `NotifyWebPaymentStarted`, and nothing else.
struct StationSide;

impl Handler for StationSide {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            match ctx.decode::<v2_1::CsmsRequest>(&request)? {
                v2_1::CsmsRequest::Reset(_) => {
                    ctx.reply(&v2_1::ResetResponse::new(v2_1::ResetStatus::Accepted))
                }
                // C25.FR.27: the station answers without parameters.
                v2_1::CsmsRequest::NotifyWebPaymentStarted(_) => {
                    ctx.reply(&v2_1::NotifyWebPaymentStartedResponse::new())
                }
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

async fn start_csms(
    handler: Arc<CsmsSide>,
    authenticator: impl ocpp_kit::transport::Authenticator,
) -> (u16, CsmsHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1, Version::V2_0_1])
        .authenticate(authenticator)
        .handler(ArcHandler(handler))
        .ping_interval(None)
        .build()
        .unwrap();
    let handle = csms.handle();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });
    (port, handle)
}

/// Lets a test keep a reference to the handler it installed.
struct ArcHandler<H>(Arc<H>);

impl<H: Handler> Handler for ArcHandler<H> {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        self.0.on_request(ctx, request)
    }
}

fn station(port: u16, identity: &str) -> Handle {
    station_with(port, identity, true)
}

fn station_with(port: u16, identity: &str, reconnect: bool) -> Handle {
    Station::builder()
        .identity(identity)
        .unwrap()
        .url(format!("ws://127.0.0.1:{port}/ocpp"))
        .versions([Version::V2_1, Version::V2_0_1])
        .security_profile(SecurityProfile::BasicAuth)
        .password(ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap())
        .ping_interval(None)
        .reconnect(reconnect)
        .handler(StationSide)
        .build()
        .unwrap()
        .spawn()
        .unwrap()
}

async fn accept_all(_auth: Auth) -> AuthOutcome {
    AuthOutcome::accept()
}

#[tokio::test]
async fn a_station_boots_and_the_csms_can_call_it_back() {
    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (port, csms) = start_csms(csms_handler.clone(), accept_all).await;

    let station = station(port, "CS-0001");
    let boot = tokio::time::timeout(
        Duration::from_secs(5),
        station.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("boot did not time out")
    .expect("boot succeeded");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Accepted);
    assert_eq!(csms_handler.boots.load(Ordering::SeqCst), 1);

    // Once accepted, ordinary traffic flows.
    let beat = station.call(v2_1::HeartbeatRequest::new()).await.unwrap();
    assert!(beat.current_time.to_string().ends_with('Z'));

    // And the CSMS can address the station by identity.
    let identity = Identity::new("CS-0001").unwrap();
    station.wait_ready().await.unwrap();
    let reset = tokio::time::timeout(
        Duration::from_secs(5),
        csms.call(
            &identity,
            v2_1::ResetRequest::new(v2_1::ResetEnum::Immediate),
        ),
    )
    .await
    .expect("reset did not time out")
    .expect("reset succeeded");
    assert_eq!(reset.status, v2_1::ResetStatus::Accepted);

    station.shutdown(Duration::from_secs(2)).await;
}

/// C25.FR.21: the **CSMS** sends `NotifyWebPaymentStarted` after decoding a QR-code URL,
/// and the station answers it (C25.FR.27).
#[tokio::test]
async fn the_csms_notifies_the_station_that_a_web_payment_started() {
    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (port, csms) = start_csms(csms_handler, accept_all).await;
    let station = station(port, "CS-QR01");
    boot(&station).await;
    station.wait_ready().await.unwrap();

    let identity = Identity::new("CS-QR01").unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        csms.call(&identity, v2_1::NotifyWebPaymentStartedRequest::new(1, 120)),
    )
    .await
    .expect("did not time out")
    .expect("the station answered");
    assert_eq!(
        v2_1::Action::NotifyWebPaymentStarted.origin(),
        ocpp_kit::message::Origin::Csms
    );

    station.shutdown(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn bad_credentials_get_a_401_and_an_unknown_identity_a_404() {
    let handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (port, csms) = start_csms(handler, |auth: Auth| async move {
        match auth.identity.as_str() {
            "CS-KNOWN" => AuthOutcome::accept(),
            "CS-WRONGPW" => AuthOutcome::Reject,
            _ => AuthOutcome::Unknown,
        }
    })
    .await;
    let mut events = csms.events();

    // The station's reconnect loop keeps retrying, so build it without reconnect.
    for (identity, expected) in [("CS-WRONGPW", 401u16), ("CS-MISSING", 404)] {
        let ended = Station::builder()
            .identity(identity)
            .unwrap()
            .url(format!("ws://127.0.0.1:{port}/ocpp"))
            .versions([Version::V2_1])
            .password(ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap())
            .reconnect(false)
            .ping_interval(None)
            .build()
            .unwrap()
            .run()
            .await
            .unwrap();
        // The station says exactly what the CSMS answered.
        match &ended {
            Ended::ConnectFailed(error) => assert!(
                matches!(**error, TransportError::Rejected { status } if status == expected),
                "{ended}"
            ),
            other => panic!("expected a connect failure, got {other}"),
        }
        assert!(
            ended.to_string().contains(&format!("HTTP {expected}")),
            "{ended}"
        );

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event arrived")
            .unwrap();
        match event {
            SessionEvent::Refused { reason, .. } => {
                assert_eq!(reason.status(), Some(expected));
                let typed = if expected == 401 {
                    matches!(reason, Refusal::Rejected)
                } else {
                    matches!(reason, Refusal::Unknown)
                };
                assert!(typed, "{reason}");
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_station_and_a_csms_with_no_common_version_close_cleanly() {
    let handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        // The CSMS speaks only 2.1.
        .versions([Version::V2_1])
        .authenticate(accept_all)
        .handler(ArcHandler(handler))
        .ping_interval(None)
        .build()
        .unwrap();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    // The station speaks only 1.6. Part 4 §3.2: the handshake succeeds *without* a
    // subprotocol header, and the connection is closed immediately — so negotiation fails
    // rather than the two silently talking past each other.
    let outcome = Station::builder()
        .identity("CS-OLD")
        .unwrap()
        .url(format!("ws://127.0.0.1:{port}/ocpp"))
        .versions([Version::V1_6])
        .password(
            ocpp_kit::transport::BasicAuthPassword::hex("00112233445566778899aabbccddeeff")
                .unwrap(),
        )
        .reconnect(false)
        .ping_interval(None)
        .build()
        .unwrap()
        .run()
        .await;
    let error = outcome.unwrap();
    assert!(
        matches!(&error, Ended::ConnectFailed(e) if matches!(**e, TransportError::Negotiation(NegotiationError::NoSubprotocol))),
        "expected a negotiation failure, got {error}"
    );
    assert!(error.to_string().contains("did not select"), "{error}");
}

#[tokio::test]
async fn a_local_controller_is_invisible_to_the_csms() {
    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (csms_port, csms) = start_csms(csms_handler.clone(), accept_all).await;

    let lc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lc_port = lc_listener.local_addr().unwrap().port();
    let controller = LocalController::builder()
        .bind(lc_listener.local_addr().unwrap())
        .upstream(format!("ws://127.0.0.1:{csms_port}/ocpp"))
        // Local load management: the site controller owns SetChargingProfile.
        .relay(
            |_: &Identity, direction: Direction, frame: &ocpp_kit::rpc::Frame<'_>| {
                if direction == Direction::Southbound
                    && frame.action() == Some("SetChargingProfile")
                {
                    RelayDecision::Reject(CallError::not_supported("SetChargingProfile"))
                } else {
                    RelayDecision::Forward
                }
            },
        )
        .build()
        .unwrap();
    tokio::spawn(async move {
        let _ = controller.serve_on(lc_listener).await;
    });

    let station = station(lc_port, "CS-BEHIND-LC");
    let boot = tokio::time::timeout(
        Duration::from_secs(5),
        station.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("boot did not time out")
    .expect("boot succeeded");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Accepted);

    // The CSMS sees the station under its own identity, as §6.2 requires.
    let identity = Identity::new("CS-BEHIND-LC").unwrap();
    assert!(csms.session(&identity).await.is_some());

    // A CSMS call the Local Controller claims is answered locally, without reaching the
    // station.
    let profile = v2_1::ChargingProfile::new(
        1,
        0,
        v2_1::ChargingProfilePurpose::TxDefaultProfile,
        v2_1::ChargingProfileKind::Absolute,
        vec![v2_1::ChargingSchedule::new(
            1,
            v2_1::ChargingRateUnit::A,
            vec![v2_1::ChargingSchedulePeriod::new(0)],
        )],
    );
    let error = csms
        .call(&identity, v2_1::SetChargingProfileRequest::new(1, profile))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("NotSupported"), "{error}");

    // Everything else still reaches the station.
    let reset = csms
        .call(
            &identity,
            v2_1::ResetRequest::new(v2_1::ResetEnum::Immediate),
        )
        .await
        .unwrap();
    assert_eq!(reset.status, v2_1::ResetStatus::Accepted);

    station.shutdown(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn a_pending_boot_keeps_the_connection_and_blocks_ordinary_traffic() {
    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Pending));
    let (port, _csms) = start_csms(csms_handler, accept_all).await;

    let station = station(port, "CS-PENDING");
    let boot = tokio::time::timeout(
        Duration::from_secs(5),
        station.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("boot did not time out")
    .expect("boot succeeded");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Pending);

    // B02.FR.06 — the connection stays open …
    assert!(station.state().connected);
    // … but B02.FR.02 keeps ordinary traffic queued rather than sending it.
    let heartbeat = tokio::time::timeout(
        Duration::from_millis(300),
        station.call(v2_1::HeartbeatRequest::new()),
    )
    .await;
    assert!(
        heartbeat.is_err(),
        "the heartbeat must not have been answered"
    );
    assert_eq!(station.state().queued, 1);
}

#[tokio::test]
async fn a_station_fails_over_to_its_next_network_configuration_slot() {
    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (working_port, csms) = start_csms(csms_handler, accept_all).await;

    // Slot 1 is the priority, and nothing is listening on it. Port 1 is reserved and refuses
    // connections immediately, so the test does not wait on a timeout.
    let password = || ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap();
    let profiles = NetworkProfiles::new([
        NetworkProfile::new(0, format!("ws://127.0.0.1:{working_port}/ocpp"))
            .security_profile(SecurityProfile::BasicAuth)
            .password(password()),
        NetworkProfile::new(1, "ws://127.0.0.1:1/ocpp")
            .security_profile(SecurityProfile::BasicAuth)
            .password(password()),
    ])
    .priority([1, 0])
    .unwrap()
    .connection_attempts(2);

    let station = Station::builder()
        .identity("CS-FAILOVER")
        .unwrap()
        .network_profiles(profiles)
        .versions([Version::V2_1])
        .backoff(ocpp_kit::engine::Backoff::immediate())
        .ping_interval(None)
        .handler(StationSide)
        .build()
        .unwrap()
        .spawn()
        .unwrap();

    // After NetworkProfileConnectionAttempts failures on slot 1 the station moves to slot 0,
    // where the CSMS is listening — so the boot eventually succeeds.
    let boot = tokio::time::timeout(
        Duration::from_secs(10),
        station.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("the station failed over in time")
    .expect("boot succeeded");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Accepted);

    let identity = Identity::new("CS-FAILOVER").unwrap();
    assert!(
        csms.session(&identity).await.is_some(),
        "it arrived over the fallback slot"
    );

    station.shutdown(Duration::from_secs(2)).await;
}

async fn boot(handle: &Handle) {
    tokio::time::timeout(
        Duration::from_secs(5),
        handle.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("boot did not time out")
    .expect("boot succeeded");
}

/// A station that reconnects stays reachable through [`CsmsHandle`]. A superseded session
/// tearing down must remove its own router entry, not whichever one now holds the identity.
#[tokio::test]
async fn a_reconnecting_station_stays_reachable_after_it_supersedes_itself() {
    let handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1])
        .authenticate(accept_all)
        .handler(ArcHandler(handler))
        .ping_interval(None)
        // A short drain so the superseded session is definitely gone — and has had its
        // chance to remove the wrong entry — well inside the window checked below.
        .supersede_drain(Duration::from_millis(100))
        .build()
        .unwrap();
    let csms = server.handle();
    tokio::spawn(async move {
        let _ = server.serve_on(listener).await;
    });
    let identity = Identity::new("CS-SUPERSEDE").unwrap();

    // Neither station reconnects: two reconnecting stations under one identity would simply
    // supersede each other for ever, which is a configuration error, not a test.
    let first = station_with(port, identity.as_str(), false);
    boot(&first).await;

    // A second connection under the same identity supersedes the first.
    let second = station_with(port, identity.as_str(), false);
    boot(&second).await;

    // Once the superseded session has finished tearing down, the survivor must still be the
    // one the router hands out — and it must still work.
    // Long enough that the superseded session's teardown has certainly run.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for _ in 0..40 {
        if csms.sessions().await.contains(&identity) {
            let session = csms.session(&identity).await.expect("a live session");
            let reset = session.call(v2_1::ResetRequest::new(v2_1::ResetEnum::Immediate));
            if tokio::time::timeout(Duration::from_secs(2), reset)
                .await
                .is_ok_and(|outcome| outcome.is_ok())
            {
                drop((first, second));
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the identity lost its session to the one it replaced");
}

/// Part 4 §5.3: the WebSocket ping is the end-to-end liveness check. A peer that stops
/// answering has to end the session, or a silently dropped mobile connection looks healthy
/// for as long as the operating system's TCP timeout lasts.
#[tokio::test]
async fn a_connection_that_stops_answering_pings_is_closed() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1])
        .authenticate(accept_all)
        .handler(ArcHandler(handler))
        .keepalive(
            ocpp_kit::transport::Keepalive::every(Duration::from_millis(50))
                .with_timeout(Duration::from_millis(150)),
        )
        .build()
        .unwrap();
    let events = csms.handle();
    let mut closed = events.events();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    // A client that completes the handshake and then never answers anything — the shape a
    // connection takes when the network drops it without telling either end.
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket
        .write_all(
            format!(
                "GET /ocpp/CS-MUTE HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Protocol: ocpp2.1\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = [0u8; 256];
    let read = socket.read(&mut response).await.unwrap();
    assert!(
        String::from_utf8_lossy(&response[..read]).contains("101"),
        "the handshake should succeed"
    );

    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(SessionEvent::Closed { identity, reason }) = closed.recv().await {
                return (identity, reason);
            }
        }
    })
    .await
    .expect("the session must be closed, not left hanging");
    assert_eq!(closed.0.as_str(), "CS-MUTE");
    assert!(
        matches!(
            closed.1,
            ocpp_kit::transport::DisconnectReason::PongTimeout(_)
        ),
        "{}",
        closed.1
    );
    drop(socket);
}

/// Part 4 §5.3's liveness check is point-to-point and a Local Controller is two connections,
/// so it runs on each. A station attached to a controller that stops answering must be
/// disconnected, or it stays "online" and does not queue.
#[tokio::test]
async fn a_local_controller_ends_a_leg_that_stops_answering_pings() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (csms_port, _csms) = start_csms(csms_handler, accept_all).await;

    let lc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let lc_port = lc_listener.local_addr().unwrap().port();
    let controller = LocalController::builder()
        .bind(lc_listener.local_addr().unwrap())
        .upstream(format!("ws://127.0.0.1:{csms_port}/ocpp"))
        .keepalive(
            ocpp_kit::transport::Keepalive::every(Duration::from_millis(50))
                .with_timeout(Duration::from_millis(150)),
        )
        .build()
        .unwrap();
    tokio::spawn(async move {
        let _ = controller.serve_on(lc_listener).await;
    });

    // A "station" that completes the handshake and then answers nothing — not even a pong.
    // This is the shape a mobile network leaves behind when it drops a connection without
    // telling either end: the socket stays writable for minutes.
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", lc_port))
        .await
        .unwrap();
    socket
        .write_all(
            format!(
                "GET /ocpp/CS-MUTE-LC HTTP/1.1\r\nHost: 127.0.0.1:{lc_port}\r\n\
                 Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Protocol: ocpp2.1\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut head = [0u8; 512];
    let read = socket.read(&mut head).await.unwrap();
    assert!(
        String::from_utf8_lossy(&head[..read]).contains("101"),
        "the controller should have accepted the station"
    );

    // Read until the controller gives up on us. Without a pong deadline this would block
    // until the test's timeout, which is exactly the bug: the station would sit "connected"
    // to a controller that had stopped listening.
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buffer = [0u8; 512];
        loop {
            match socket.read(&mut buffer).await {
                // A close frame: the controller ended the leg deliberately.
                Ok(n) if buffer[..n].first().is_some_and(|byte| byte & 0x0F == 0x08) => {
                    return true;
                }
                // Anything else it sent us — a ping — is not the end.
                Ok(n) if n > 0 => {}
                // EOF or a reset: the leg is gone either way.
                _ => return true,
            }
        }
    })
    .await;

    assert_eq!(
        ended,
        Ok(true),
        "a leg that stops answering pings must be closed, not left hanging"
    );
}

/// A00.FR.204 makes the Basic username the identity from the URL; A00.FR.207 makes validating
/// it the CSMS's job. The check runs before the `Authenticator`, so an authenticator that
/// trusts the header — as the one below does — still cannot admit a peer under someone else's
/// identity.
#[tokio::test]
async fn a_basic_username_that_is_not_the_url_identity_is_refused() {
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    // An authenticator that trusts whatever the Authorization header says, which is exactly
    // the shape the rule exists to protect.
    let (port, _csms) = start_csms(handler, |auth: Auth| async move {
        match &auth.credentials {
            ocpp_kit::transport::Credentials::Basic { user, .. } if user == "CS-ATTACKER" => {
                AuthOutcome::accept()
            }
            _ => AuthOutcome::Reject,
        }
    })
    .await;

    let credentials = base64::engine::general_purpose::STANDARD.encode("CS-ATTACKER:secret");
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket
        .write_all(
            format!(
                "GET /ocpp/CS-VICTIM HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Authorization: Basic {credentials}\r\n\
                 Sec-WebSocket-Protocol: ocpp2.1\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = [0u8; 512];
    let read = socket.read(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response[..read]);
    assert!(
        response.contains("401"),
        "a username that is not the URL identity must be refused, got: {response}"
    );
}

/// A CSMS that authenticates nobody should be something you *chose*, not something you got by
/// forgetting a builder call — and nothing in such a server's logs would tell you which it was.
#[test]
fn a_csms_without_an_authenticator_does_not_build() {
    let Err(error) = Csms::builder().bind("127.0.0.1:0".parse().unwrap()).build() else {
        panic!("a CSMS with no authenticator must not build");
    };
    assert!(
        error.to_string().contains("authenticator"),
        "the error should name what is missing: {error}"
    );

    // Accepting everyone is available, but it has to be written down.
    assert!(
        Csms::builder()
            .bind("127.0.0.1:0".parse().unwrap())
            .authenticate(ocpp_kit::transport::AcceptEveryStation)
            .build()
            .is_ok()
    );
}

/// The builder rejects a security profile whose credentials do not match it, and the
/// documentation shows configurations that must therefore actually build.
///
/// Doc snippets are compiled but their `async fn` bodies are never run, so a `build()` that
/// returns `Err` is invisible there. This runs them.
#[test]
fn the_documented_security_profile_configurations_build() {
    use ocpp_kit::transport::{BasicAuthPassword, ClientTls};

    let password = || BasicAuthPassword::utf8("a-sixteen-plus-character-secret").unwrap();

    // Profile 1 — plain WebSocket, HTTP Basic.
    assert!(
        Station::builder()
            .identity("CS-0001")
            .unwrap()
            .url("ws://csms.example.com/ocpp")
            .versions([Version::V2_1])
            .security_profile(SecurityProfile::BasicAuth)
            .password(password())
            .build()
            .is_ok()
    );

    // Profile 2 — TLS plus HTTP Basic. This is the one the README and both index pages show.
    assert!(
        Station::builder()
            .identity("CS-0001")
            .unwrap()
            .url("wss://csms.example.com/ocpp")
            .versions([Version::V2_1, Version::V2_0_1])
            .security_profile(SecurityProfile::TlsBasicAuth)
            .password(password())
            .tls(ClientTls::with_webpki_roots().unwrap())
            .build()
            .is_ok()
    );

    // A TLS profile without a TLS configuration is a build error, not a runtime surprise.
    let Err(error) = Station::builder()
        .identity("CS-0001")
        .unwrap()
        .url("wss://csms.example.com/ocpp")
        .versions([Version::V2_1])
        .security_profile(SecurityProfile::TlsBasicAuth)
        .password(password())
        .build()
    else {
        panic!("a TLS profile with no TLS configuration must not build");
    };
    assert!(error.to_string().contains("TLS"), "{error}");

    // …and so is a TLS profile pointed at a `ws://` URL.
    assert!(
        Station::builder()
            .identity("CS-0001")
            .unwrap()
            .url("ws://csms.example.com/ocpp")
            .versions([Version::V2_1])
            .security_profile(SecurityProfile::TlsBasicAuth)
            .password(password())
            .tls(ClientTls::with_webpki_roots().unwrap())
            .build()
            .is_err()
    );
}

/// A CSMS maps a connection's identity to something of its own, and the authenticator is the
/// one place that has already had to do the lookup. What it resolves rides the session, so a
/// handler does not keep a second map keyed on `Identity` — and does not have to decide what
/// to do when that map misses for a station the authenticator definitely admitted.
#[tokio::test]
async fn what_the_authenticator_resolved_reaches_the_handler_and_the_handle() {
    /// The application's own view of a charge point.
    #[derive(Debug, PartialEq)]
    struct ChargePoint {
        row_id: u64,
        tenant: String,
    }

    /// A handler that reports whether it could see the resolved charge point.
    struct Resolving(Arc<AtomicUsize>);

    impl Handler for Resolving {
        fn on_request(
            &self,
            ctx: Ctx,
            request: IncomingRequest,
        ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
            let seen = ctx.session::<ChargePoint>().is_some_and(|point| {
                point.row_id == 42 && point.tenant == "acme" && ctx.identity().as_str() == "CS-0001"
            });
            Box::pin(async move {
                if seen {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
                match ctx.decode::<v2_1::CsRequest>(&request)? {
                    v2_1::CsRequest::BootNotification(_) => {
                        ctx.reply(&v2_1::BootNotificationResponse::new(
                            ocpp_kit::types::DateTime::now(),
                            0,
                            v2_1::RegistrationStatus::Accepted,
                        ))
                    }
                    other => Err(CallError::not_supported(other.action().as_str())),
                }
            })
        }
    }

    let resolved = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1])
        .authenticate(|auth: Auth| async move {
            // The lookup happens once, here, where the station has to be looked up anyway.
            if auth.identity.as_str() == "CS-0001" {
                Admission::new()
                    .context(ChargePoint {
                        row_id: 42,
                        tenant: "acme".into(),
                    })
                    .into()
            } else {
                AuthOutcome::Unknown
            }
        })
        .handler(Resolving(resolved.clone()))
        .ping_interval(None)
        .build()
        .unwrap();
    let csms_handle = csms.handle();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    let station = station(port, "CS-0001");
    station
        .call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        ))
        .await
        .unwrap();
    assert_eq!(resolved.load(Ordering::SeqCst), 1);

    // And the CSMS-side handle carries it too, so code that calls *out* to a station sees the
    // same resolution as code answering a call from one.
    let identity = Identity::new("CS-0001").unwrap();
    let to_station = csms_handle
        .session(&identity)
        .await
        .expect("a connected station");
    assert_eq!(
        to_station
            .session::<ChargePoint>()
            .map(|point| point.row_id),
        Some(42)
    );
    // A type that was never stored is `None`, not a panic.
    assert!(to_station.session::<String>().is_none());
}

/// Plain OCPP-J 1.6 has no authentication; many 1.6 fleets run that way. The station sends no
/// `Authorization` header, and the CSMS reports profile 0 rather than profile 1.
#[tokio::test]
async fn a_1_6_station_on_profile_0_connects_without_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let seen_tx = std::sync::Mutex::new(Some(seen_tx));
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V1_6])
        .authenticate(move |auth: Auth| {
            if let Some(tx) = seen_tx.lock().unwrap().take() {
                let _ = tx.send((
                    auth.profile,
                    matches!(auth.credentials, ocpp_kit::transport::Credentials::None),
                ));
            }
            async { AuthOutcome::accept() }
        })
        .handler(StationSide)
        .ping_interval(None)
        .build()
        .unwrap();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    let station = Station::builder()
        .identity("CS-OPEN16")
        .unwrap()
        .url(format!("ws://127.0.0.1:{port}/ocpp"))
        .versions([Version::V1_6])
        .security_profile(SecurityProfile::Unsecured)
        .ping_interval(None)
        .handler(StationSide)
        .build()
        .unwrap()
        .spawn()
        .unwrap();

    let (profile, no_credentials) = tokio::time::timeout(Duration::from_secs(5), seen_rx)
        .await
        .expect("the CSMS saw the handshake")
        .unwrap();
    assert_eq!(profile, SecurityProfile::Unsecured);
    assert_eq!(profile.number(), 0);
    assert!(no_credentials);
    station.shutdown(Duration::from_secs(2)).await;
}

/// A00.FR.202: a 2.x station SHALL authenticate. Profile 0 offering 2.x is a configuration
/// error, caught before anything connects.
#[test]
fn profile_0_is_refused_for_a_station_that_offers_2_x() {
    let error = Station::builder()
        .identity("CS-OPEN21")
        .unwrap()
        .url("ws://127.0.0.1:1/ocpp")
        .versions([Version::V2_1, Version::V1_6])
        .security_profile(SecurityProfile::Unsecured)
        .handler(StationSide)
        .build()
        .err()
        .expect("refused");
    assert!(error.to_string().contains("A00.FR.202"), "{error}");
}

/// A CSMS sees what its fleet sends: a lenient decode in a handler surfaces on the fleet-wide
/// event stream, tagged with the station, the action, the pointer and the kind.
#[tokio::test]
async fn repairs_reach_the_csms_event_stream() {
    use ocpp_kit::decode::{DecodeOptions, RepairKind};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1])
        .authenticate(accept_all)
        .decode_options_for(|_| DecodeOptions::lenient())
        .handler(ArcHandler(Arc::new(CsmsSide::new(
            v2_1::RegistrationStatus::Accepted,
        ))))
        .ping_interval(None)
        .build()
        .unwrap();
    let mut events = csms.handle().events();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });

    let station = station(port, "CS-LAX01");
    // A boot whose `reason` is not an OCPP value: decodes leniently, as `UnknownValue`.
    let payload = RawValue::from_string(
        r#"{"reason":"SolarFlare","chargingStation":{"model":"M1","vendorName":"ACME"}}"#.into(),
    )
    .unwrap();
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        station.call_raw(
            "BootNotification",
            payload,
            ocpp_kit::engine::CallOptions::default(),
        ),
    )
    .await
    .expect("did not time out")
    .expect("answered");

    let repaired = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(SessionEvent::Repaired {
                identity,
                action,
                repairs,
                ..
            }) = events.recv().await
            {
                return (identity, action, repairs);
            }
        }
    })
    .await
    .expect("a Repaired event");
    assert_eq!(repaired.0.as_str(), "CS-LAX01");
    assert_eq!(repaired.1, "BootNotification");
    assert_eq!(repaired.2.len(), 1);
    assert_eq!(repaired.2[0].pointer, "/reason");
    assert_eq!(repaired.2[0].kind, RepairKind::UnknownEnumValue);
    station.shutdown(Duration::from_secs(2)).await;
}

/// The CSMS reports whether `permessage-deflate` was negotiated, and a session's
/// `Disconnected` event carries the close frame the peer sent.
#[cfg(feature = "compression")]
#[tokio::test]
async fn sessions_report_compression_and_the_peers_close_frame() {
    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (port, csms) = start_csms(csms_handler, accept_all).await;
    let mut fleet = csms.events();
    let station = station(port, "CS-ZIP01");

    let compressed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(SessionEvent::Opened { compressed, .. }) = fleet.recv().await {
                return compressed;
            }
        }
    })
    .await
    .expect("an Opened event");
    assert!(compressed, "both ends offer permessage-deflate by default");

    let identity = Identity::new("CS-ZIP01").unwrap();
    let session = csms.session(&identity).await.expect("a session");
    let mut events = session.events();
    station.shutdown(Duration::from_secs(2)).await;

    let peer_close = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(ocpp_kit::transport::Event::Disconnected {
                reason: ocpp_kit::transport::DisconnectReason::PeerClosed(peer_close),
            }) = events.recv().await
            {
                return peer_close;
            }
        }
    })
    .await
    .expect("a Disconnected event");
    let frame = peer_close.expect("the station sent a close frame");
    assert_eq!(frame.code, ocpp_kit::transport::CloseCode::NORMAL);
}

/// B10.FR.10 (2.0.1 edition 4, 2.1 edition 2): a station that connects over a *different*
/// network configuration slot registers again, even though it never rebooted.
#[tokio::test]
async fn switching_network_profile_requires_a_new_boot_notification() {
    let first = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    // Slot 0's CSMS lets the station in once, then refuses it, forcing the switch.
    let admitted = Arc::new(AtomicUsize::new(0));
    let gate = admitted.clone();
    let (first_port, first_csms) = start_csms(first, move |_auth: Auth| {
        let gate = gate.clone();
        async move {
            if gate.fetch_add(1, Ordering::SeqCst) == 0 {
                AuthOutcome::accept()
            } else {
                AuthOutcome::Reject
            }
        }
    })
    .await;
    let second = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (second_port, _second_csms) = start_csms(second.clone(), accept_all).await;

    let password = || ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap();
    let profiles = NetworkProfiles::new([
        NetworkProfile::new(0, format!("ws://127.0.0.1:{first_port}/ocpp"))
            .security_profile(SecurityProfile::BasicAuth)
            .password(password()),
        NetworkProfile::new(1, format!("ws://127.0.0.1:{second_port}/ocpp"))
            .security_profile(SecurityProfile::BasicAuth)
            .password(password()),
    ])
    .priority([0, 1])
    .unwrap()
    .connection_attempts(1);
    let station = Station::builder()
        .identity("CS-SWITCH")
        .unwrap()
        .network_profiles(profiles)
        .versions([Version::V2_1])
        .backoff(ocpp_kit::engine::Backoff::immediate())
        .ping_interval(None)
        .handler(StationSide)
        .build()
        .unwrap()
        .spawn()
        .unwrap();
    let mut events = station.events();
    boot(&station).await;

    // Slot 0 drops the session; the reconnect is refused there and lands on slot 1.
    first_csms.shutdown(Duration::from_secs(1)).await;
    let reopened = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(ocpp_kit::transport::Event::BootState(ocpp_kit::engine::BootState::Idle)) => {
                    break;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => panic!("events ended: {error}"),
            }
        }
    })
    .await;
    assert!(reopened.is_ok(), "the boot gate re-opened after the switch");
    boot(&station).await;
    assert_eq!(
        second.boots.load(Ordering::SeqCst),
        1,
        "registered with slot 1's CSMS"
    );

    station.shutdown(Duration::from_secs(2)).await;
}

/// A store two owners share: the station's engine, and "another process" that pushes.
#[derive(Clone, Default)]
struct SharedStore(Arc<std::sync::Mutex<ocpp_kit::engine::MemStore>>);

impl ocpp_kit::engine::MessageStore for SharedStore {
    fn push(
        &mut self,
        entry: &ocpp_kit::engine::QueuedCall,
    ) -> Result<ocpp_kit::engine::Seq, ocpp_kit::engine::StoreError> {
        self.0.lock().unwrap().push(entry)
    }
    fn pending(
        &self,
    ) -> Result<
        Vec<(ocpp_kit::engine::Seq, ocpp_kit::engine::QueuedCall)>,
        ocpp_kit::engine::StoreError,
    > {
        self.0.lock().unwrap().pending()
    }
    fn ack(&mut self, seq: ocpp_kit::engine::Seq) -> Result<(), ocpp_kit::engine::StoreError> {
        self.0.lock().unwrap().ack(seq)
    }
    fn set_attempts(
        &mut self,
        seq: ocpp_kit::engine::Seq,
        attempts: u32,
    ) -> Result<(), ocpp_kit::engine::StoreError> {
        self.0.lock().unwrap().set_attempts(seq, attempts)
    }
    fn len(&self) -> Result<usize, ocpp_kit::engine::StoreError> {
        self.0.lock().unwrap().len()
    }
}

/// `store_poll`: a message another process pushes after the station connected is sent
/// without a restart, and only once.
#[tokio::test]
async fn a_station_with_store_poll_sends_what_another_process_queued() {
    use ocpp_kit::engine::MessageStore as _;

    let csms_handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (port, _csms) = start_csms(csms_handler.clone(), accept_all).await;
    let store = SharedStore::default();
    let station = Station::builder()
        .identity("CS-POLL")
        .unwrap()
        .url(format!("ws://127.0.0.1:{port}/ocpp"))
        .versions([Version::V2_1])
        .security_profile(SecurityProfile::BasicAuth)
        .password(ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap())
        .ping_interval(None)
        .handler(StationSide)
        .store(store.clone())
        .store_poll(Duration::from_millis(20))
        .build()
        .unwrap()
        .spawn()
        .unwrap();
    boot(&station).await;

    // The other process.
    store
        .clone()
        .push(&ocpp_kit::engine::QueuedCall {
            action: "TransactionEvent".into(),
            payload: RawValue::from_string(
                r#"{"eventType":"Updated","timestamp":"2026-11-07T12:00:00Z","triggerReason":"MeterValuePeriodic","seqNo":1,"transactionInfo":{"transactionId":"tx-meter"}}"#
                    .into(),
            )
            .unwrap(),
            kind: ocpp_kit::message::MessageKind::Call,
            attempts: 0,
            transactional: true,
        })
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while store.len().unwrap() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the pushed message was delivered and acknowledged");
    // Several more polls: none may send it again.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(csms_handler.transaction_events.load(Ordering::SeqCst), 1);

    station.shutdown(Duration::from_secs(2)).await;
}

// ---------------------------------------------------------------------------
// Station edge: idle timeout and message rate, driven by an outside WebSocket client
// ---------------------------------------------------------------------------

type ReferenceSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// A tokio-tungstenite client speaking `ocpp2.1`. It answers pings by itself whenever the
/// test reads from it, which is exactly what a peer that is alive but silent looks like.
async fn reference_client(port: u16, identity: &str) -> ReferenceSocket {
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://127.0.0.1:{port}/ocpp/{identity}"))
        .header("Host", format!("127.0.0.1:{port}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .header("Sec-WebSocket-Protocol", "ocpp2.1")
        .body(())
        .unwrap();
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    socket
}

async fn start_csms_with(
    configure: impl FnOnce(ocpp_kit::transport::CsmsBuilder) -> ocpp_kit::transport::CsmsBuilder,
) -> (u16, CsmsHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let builder = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1])
        .authenticate(accept_all)
        .handler(ArcHandler(Arc::new(CsmsSide::new(
            v2_1::RegistrationStatus::Accepted,
        ))));
    let csms = configure(builder).build().unwrap();
    let handle = csms.handle();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });
    (port, handle)
}

async fn closed_reason(
    events: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    within: Duration,
) -> Option<ocpp_kit::transport::DisconnectReason> {
    tokio::time::timeout(within, async {
        loop {
            if let Ok(SessionEvent::Closed { reason, .. }) = events.recv().await {
                return reason;
            }
        }
    })
    .await
    .ok()
}

/// A peer that answers every ping but never speaks OCPP is closed after the idle timeout —
/// keepalive alone would keep it for ever.
#[tokio::test]
async fn a_station_that_only_answers_pings_is_closed_as_idle() {
    use futures_util::StreamExt as _;
    use ocpp_kit::transport::{DisconnectReason, Keepalive};

    let idle = Duration::from_millis(400);
    let (port, csms) = start_csms_with(|builder| {
        builder
            .keepalive(
                Keepalive::every(Duration::from_millis(50))
                    .with_timeout(Duration::from_millis(200)),
            )
            .idle_timeout(idle)
    })
    .await;
    let mut events = csms.events();

    let mut socket = reference_client(port, "CS-SILENT").await;
    let pongs = tokio::spawn(async move {
        // Reading is what makes tungstenite answer the pings; the peer says nothing else.
        while let Some(Ok(_)) = socket.next().await {}
    });

    let started = std::time::Instant::now();
    let reason = closed_reason(&mut events, Duration::from_secs(5))
        .await
        .expect("the silent session is closed");
    assert_eq!(reason, DisconnectReason::Idle(idle));
    assert!(started.elapsed() >= idle, "not before the timeout");
    assert!(started.elapsed() < idle + Duration::from_secs(1), "SC-003");
    pongs.abort();
}

/// Off unless named: without `idle_timeout`, a silent peer that answers pings is left alone,
/// exactly as before the setting existed.
#[tokio::test]
async fn without_an_idle_timeout_a_silent_station_stays() {
    use futures_util::StreamExt as _;
    use ocpp_kit::transport::Keepalive;

    let (port, csms) = start_csms_with(|builder| {
        builder.keepalive(
            Keepalive::every(Duration::from_millis(50)).with_timeout(Duration::from_millis(200)),
        )
    })
    .await;
    let mut events = csms.events();
    let mut socket = reference_client(port, "CS-QUIET").await;
    let pongs = tokio::spawn(async move { while let Some(Ok(_)) = socket.next().await {} });

    assert!(
        closed_reason(&mut events, Duration::from_millis(800))
            .await
            .is_none(),
        "no idle close when none was configured"
    );
    assert!(
        csms.session(&Identity::new("CS-QUIET").unwrap())
            .await
            .is_some()
    );
    pongs.abort();
}

/// G02.FR.04: any message is activity, so a station that speaks more often than the idle
/// timeout stays connected however long it lives.
#[tokio::test]
async fn a_station_that_talks_is_never_idle() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message as RefMessage;

    let idle = Duration::from_millis(300);
    let (port, csms) = start_csms_with(|builder| builder.idle_timeout(idle)).await;
    let mut events = csms.events();

    let mut socket = reference_client(port, "CS-CHATTY").await;
    for n in 0..8 {
        socket
            .send(RefMessage::text(format!(r#"[2,"h{n}","Heartbeat",{{}}]"#)))
            .await
            .unwrap();
        let _ = socket.next().await;
        tokio::time::sleep(idle / 2).await;
    }
    assert!(
        closed_reason(&mut events, Duration::from_millis(10))
            .await
            .is_none(),
        "a station sending every T/2 stays connected"
    );
    assert!(
        csms.session(&Identity::new("CS-CHATTY").unwrap())
            .await
            .is_some()
    );
}

/// The rate over a real socket: the burst reaches the handler, the rest is answered
/// `SecurityError`, and the fleet hears about it once.
#[tokio::test]
async fn a_flooding_station_is_answered_security_error_and_reported_once() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use ocpp_kit::engine::RateLimit;
    use tokio_tungstenite::tungstenite::Message as RefMessage;

    let (port, csms) = start_csms_with(|builder| {
        builder.message_rate(RateLimit::per_minute(60).unwrap().with_burst(3).unwrap())
    })
    .await;
    let mut events = csms.events();

    let mut socket = reference_client(port, "CS-FLOOD").await;
    let mut served = 0;
    let mut refused = 0;
    for n in 0..20 {
        socket
            .send(RefMessage::text(format!(r#"[2,"h{n}","Heartbeat",{{}}]"#)))
            .await
            .unwrap();
        // Skip the keepalive's pings; tungstenite answers them on its own.
        let answer = loop {
            match socket.next().await {
                Some(Ok(RefMessage::Text(answer))) => break answer,
                Some(Ok(_)) => {}
                other => panic!("an answer, not {other:?}"),
            }
        };
        if answer.starts_with("[3,") {
            served += 1;
        } else {
            assert!(answer.contains("SecurityError"), "{answer}");
            refused += 1;
        }
    }
    assert_eq!((served, refused), (3, 17));

    let mut reports = 0;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await
    {
        if let SessionEvent::RateLimited { identity } = event {
            assert_eq!(identity.as_str(), "CS-FLOOD");
            reports += 1;
        }
    }
    assert_eq!(reports, 1);
}

// ---------------------------------------------------------------------------
// Mixed profiles on one listener: ServerTls::with_optional_client_auth
// ---------------------------------------------------------------------------

#[cfg(feature = "rustls")]
mod mixed_profiles {
    use super::*;
    use ocpp_kit::transport::{ClientTls, Credentials, ServerTls};
    use tokio::sync::mpsc;

    const TLS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tls");

    fn fixture(name: &str) -> String {
        format!("{TLS}/{name}")
    }

    /// What the authenticator was told, per connection.
    type Seen = (SecurityProfile, &'static str);

    /// A CSMS on one optional-client-auth listener. `CS-CERT` is provisioned for profile 3,
    /// `CS-BASIC` for profile 2; the authenticator enforces A00.FR.002 and reports what it saw.
    async fn start() -> (u16, CsmsHandle, mpsc::UnboundedReceiver<Seen>) {
        let (seen_tx, seen) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let tls = ServerTls::with_optional_client_auth(
            fixture("csms.pem"),
            fixture("csms.key"),
            fixture("ca.pem"),
        )
        .unwrap();
        let csms = Csms::builder()
            .bind(listener.local_addr().unwrap())
            .versions([Version::V2_1])
            .tls(tls)
            .authenticate(move |auth: Auth| {
                let seen_tx = seen_tx.clone();
                async move {
                    let presented = match auth.credentials {
                        Credentials::ClientCertificate { .. } => "certificate",
                        Credentials::Basic { .. } => "basic",
                        _ => "nothing",
                    };
                    let _ = seen_tx.send((auth.profile, presented));
                    let provisioned = match auth.identity.as_str() {
                        "CS-CERT" => SecurityProfile::TlsClientCertificate,
                        "CS-BASIC" => SecurityProfile::TlsBasicAuth,
                        _ => return AuthOutcome::Unknown,
                    };
                    // A00.FR.002 — the line the documentation asks every mixed listener for.
                    if auth.profile != provisioned {
                        return AuthOutcome::Reject;
                    }
                    AuthOutcome::accept()
                }
            })
            .handler(ArcHandler(Arc::new(CsmsSide::new(
                v2_1::RegistrationStatus::Accepted,
            ))))
            .ping_interval(None)
            .build()
            .unwrap();
        let handle = csms.handle();
        tokio::spawn(async move {
            let _ = csms.serve_on(listener).await;
        });
        (port, handle, seen)
    }

    fn station(port: u16, identity: &str, profile: SecurityProfile, tls: ClientTls) -> Handle {
        let mut builder = Station::builder()
            .identity(identity)
            .unwrap()
            .url(format!("wss://127.0.0.1:{port}/ocpp"))
            .versions([Version::V2_1])
            .security_profile(profile)
            .tls(tls)
            .ping_interval(None)
            .reconnect(false)
            .handler(StationSide);
        if profile == SecurityProfile::TlsBasicAuth {
            builder = builder.password(
                ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap(),
            );
        }
        builder.build().unwrap().spawn().unwrap()
    }

    fn trusting_client(certificate: Option<(&str, &str)>) -> ClientTls {
        let mut builder = ClientTls::builder().root_file(fixture("ca.pem")).unwrap();
        if let Some((chain, key)) = certificate {
            builder = builder
                .client_certificate(fixture(chain), fixture(key))
                .unwrap();
        }
        builder.build().unwrap()
    }

    async fn opened_within(
        events: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
        within: Duration,
    ) -> Option<String> {
        tokio::time::timeout(within, async {
            loop {
                match events.recv().await {
                    Ok(SessionEvent::Opened { identity, .. }) => return Some(identity.to_string()),
                    Ok(SessionEvent::Refused { reason, .. }) => {
                        return Some(match (reason.status(), &reason) {
                            (Some(status), _) => format!("refused {status}"),
                            (None, Refusal::Failed(_)) => "refused: handshake failed".into(),
                            (None, _) => format!("refused: {reason}"),
                        });
                    }
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn opened(events: &mut tokio::sync::broadcast::Receiver<SessionEvent>) -> Option<String> {
        opened_within(events, Duration::from_secs(5)).await
    }

    /// the profile is what the connection presented, not what the listener allows.
    #[tokio::test]
    async fn one_listener_serves_profile_3_and_profile_2() {
        let (port, csms, mut seen) = start().await;
        let mut events = csms.events();

        let _cert = station(
            port,
            "CS-CERT",
            SecurityProfile::TlsClientCertificate,
            trusting_client(Some(("station.pem", "station.key"))),
        );
        assert_eq!(opened(&mut events).await.as_deref(), Some("CS-CERT"));
        assert_eq!(
            seen.recv().await,
            Some((SecurityProfile::TlsClientCertificate, "certificate"))
        );

        let _basic = station(
            port,
            "CS-BASIC",
            SecurityProfile::TlsBasicAuth,
            trusting_client(None),
        );
        assert_eq!(opened(&mut events).await.as_deref(), Some("CS-BASIC"));
        assert_eq!(
            seen.recv().await,
            Some((SecurityProfile::TlsBasicAuth, "basic"))
        );
    }

    /// SC-001: a CSMS certificate for another name ends the station with the TLS error, not
    /// "connection lost".
    #[tokio::test]
    async fn a_csms_certificate_for_another_name_is_the_reason_given() {
        let (port, _csms, _seen) = start().await;
        let handle = Station::builder()
            .identity("CS-BASIC")
            .unwrap()
            // The fixture certificate is for 127.0.0.1 only.
            .url(format!("wss://localhost:{port}/ocpp"))
            .versions([Version::V2_1])
            .security_profile(SecurityProfile::TlsBasicAuth)
            .password(ocpp_kit::transport::BasicAuthPassword::utf8("0123456789abcdef").unwrap())
            .tls(trusting_client(None))
            .ping_interval(None)
            .reconnect(false)
            .handler(StationSide)
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), handle.closed())
            .await
            .unwrap();
        assert!(matches!(ended, Ended::ConnectFailed(_)), "{ended}");
        let text = ended.to_string().to_lowercase();
        assert!(
            text.contains("certificate") || text.contains("name"),
            "{text}"
        );
    }

    /// A00.FR.407: optional means "may be absent", never "need not verify".
    #[tokio::test]
    async fn a_certificate_from_an_untrusted_issuer_fails_the_handshake() {
        let (port, csms, mut seen) = start().await;
        let mut events = csms.events();

        let _rogue = station(
            port,
            "CS-CERT",
            SecurityProfile::TlsClientCertificate,
            trusting_client(Some(("rogue-station.pem", "rogue-station.key"))),
        );
        assert_eq!(
            opened_within(&mut events, Duration::from_secs(5))
                .await
                .as_deref(),
            Some("refused: handshake failed"),
            "no session: TLS ended it, and the operator is told"
        );
        assert!(
            seen.try_recv().is_err(),
            "the authenticator was never asked"
        );
    }

    /// A00.FR.002: a profile-3 station connecting without its certificate is a
    /// downgrade, and the authenticator refuses it.
    #[tokio::test]
    async fn a_profile_3_station_without_its_certificate_is_refused() {
        let (port, csms, mut seen) = start().await;
        let mut events = csms.events();

        let _downgrade = station(
            port,
            "CS-CERT",
            SecurityProfile::TlsBasicAuth,
            trusting_client(None),
        );
        assert_eq!(opened(&mut events).await.as_deref(), Some("refused 401"));
        assert_eq!(
            seen.recv().await,
            Some((SecurityProfile::TlsBasicAuth, "basic"))
        );
    }
}

// --- Edge diagnostics: why a connection failed, which version a station gets ---------------

mod edge {
    use super::*;
    use ocpp_kit::engine::Backoff;
    use ocpp_kit::transport::{BasicAuthPassword, DisconnectReason, Event, StationBuilder};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    const PASSWORD: &str = "0123456789abcdef";

    fn builder(port: u16, identity: &str, versions: &[Version]) -> StationBuilder {
        Station::builder()
            .identity(identity)
            .unwrap()
            .url(format!("ws://127.0.0.1:{port}/ocpp"))
            .versions(versions.iter().copied())
            .security_profile(SecurityProfile::BasicAuth)
            .password(BasicAuthPassword::utf8(PASSWORD).unwrap())
            .ping_interval(None)
            .reconnect(false)
            .handler(StationSide)
    }

    /// A CSMS serving 2.1 and 2.0.1 whose authenticator pins per identity:
    /// `CS-PIN201` → 2.0.1, `CS-PIN16` → 1.6, `CS-WRONGPW` rejected, anything else admitted.
    async fn pinning_csms(
        configure: impl FnOnce(ocpp_kit::transport::CsmsBuilder) -> ocpp_kit::transport::CsmsBuilder,
    ) -> (u16, CsmsHandle) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let csms = configure(
            Csms::builder()
                .bind(listener.local_addr().unwrap())
                .versions([Version::V2_1, Version::V2_0_1])
                .authenticate(|auth: Auth| async move {
                    match auth.identity.as_str() {
                        "CS-PIN201" => Admission::new().version(Version::V2_0_1).into(),
                        "CS-PIN16" => Admission::new().version(Version::V1_6).into(),
                        "CS-WRONGPW" => AuthOutcome::Reject,
                        _ => AuthOutcome::accept(),
                    }
                })
                .handler(ArcHandler(Arc::new(CsmsSide::new(
                    v2_1::RegistrationStatus::Accepted,
                ))))
                .ping_interval(None),
        )
        .build()
        .unwrap();
        let handle = csms.handle();
        tokio::spawn(async move {
            let _ = csms.serve_on(listener).await;
        });
        (port, handle)
    }

    async fn next_refusal(
        events: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    ) -> (Option<Identity>, Refusal) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let SessionEvent::Refused {
                    identity, reason, ..
                } = events.recv().await.unwrap()
                {
                    return (identity, reason);
                }
            }
        })
        .await
        .expect("a refusal arrived")
    }

    async fn next_opened(events: &mut tokio::sync::broadcast::Receiver<SessionEvent>) -> Version {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.unwrap() {
                    SessionEvent::Opened { version, .. } => return version,
                    SessionEvent::Refused { reason, .. } => panic!("refused: {reason}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("a session opened")
    }

    /// Writes a raw upgrade request and returns every byte the CSMS sent until it closed (or
    /// went quiet for a second).
    async fn raw(port: u16, request: &str) -> Vec<u8> {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut received = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(Ok(read)) =
            tokio::time::timeout(Duration::from_secs(1), socket.read(&mut chunk)).await
        {
            if read == 0 {
                break;
            }
            received.extend_from_slice(&chunk[..read]);
        }
        received
    }

    fn upgrade(identity: &str, protocol: &str, user: &str, password: &str) -> String {
        use base64::Engine as _;
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        format!(
            "GET /ocpp/{identity} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {protocol}\r\n\
             Authorization: Basic {credentials}\r\n\r\n"
        )
    }

    // --- story 1: the station says why ---------------------------------------------------

    /// The cause is kept, not broadcast: asking after the fact never races. The peer accepts
    /// and hangs up, which fails fast on every OS (a refused port takes ~2 s on Windows).
    #[tokio::test]
    async fn edge_station_learns_why_after_the_fact_every_time() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        for _ in 0..1_000 {
            let handle = builder(port, "CS-HANGUP", &[Version::V2_1])
                .build()
                .unwrap()
                .spawn()
                .unwrap();
            let first = tokio::time::timeout(Duration::from_secs(5), handle.closed())
                .await
                .expect("the station ends");
            assert!(matches!(first, Ended::ConnectFailed(_)), "{first}");
            // A clone made after the end asks again: answered at once, with the same cause.
            let late = handle.clone();
            let ended = tokio::time::timeout(Duration::from_millis(100), late.closed())
                .await
                .expect("an ended station answers at once");
            assert_eq!(ended.to_string(), first.to_string());
            let ready = late.wait_ready().await.unwrap_err();
            assert_eq!(ready.to_string(), first.to_string());
        }
    }

    /// A CSMS that completes the handshake without a subprotocol is named as the cause.
    #[tokio::test]
    async fn edge_station_names_a_missing_subprotocol() {
        let (port, _csms) = pinning_csms(|b| b).await;
        let handle = builder(port, "CS-OLD", &[Version::V1_6])
            .password(BasicAuthPassword::hex("00112233445566778899aabbccddeeff").unwrap())
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        let ended = handle.wait_ready().await.unwrap_err();
        assert!(
            matches!(&ended, Ended::ConnectFailed(e)
                if matches!(**e, TransportError::Negotiation(NegotiationError::NoSubprotocol))),
            "{ended}"
        );
        assert_eq!(
            ended.to_string(),
            "could not connect: server did not select a WebSocket subprotocol (Part 4 §3.1.2)"
        );
        assert!(
            std::error::Error::source(&ended).is_some(),
            "the cause chains"
        );
    }

    /// FR-003 / SR-001: every failed attempt is an event with its typed cause, before the
    /// back-off — and nothing rendered contains the password.
    #[tokio::test]
    async fn edge_station_reports_every_failed_attempt_before_backing_off() {
        let (port, _csms) = pinning_csms(|b| b).await;
        let handle = builder(port, "CS-WRONGPW", &[Version::V2_1])
            .reconnect(true)
            .backoff(Backoff {
                wait_minimum: Duration::from_millis(50),
                random_range: Duration::ZERO,
                repeat_times: 0,
            })
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        let mut events = handle.events();
        let mut failed = None;
        let mut pairs = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while pairs < 2 {
                match events.recv().await.unwrap() {
                    Event::ConnectFailed {
                        attempt,
                        configuration_slot,
                        error,
                    } => {
                        assert!(matches!(*error, TransportError::Rejected { status: 401 }));
                        assert_eq!(configuration_slot, 0);
                        let text = format!("{error} {error:?}");
                        assert!(!text.contains(PASSWORD), "{text}");
                        failed = Some(attempt);
                    }
                    Event::Reconnecting { attempt, .. } if failed == Some(attempt) => {
                        failed = None;
                        pairs += 1;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("two failed attempts, each followed by its back-off");
        assert!(
            handle.ended().is_none(),
            "a reconnecting station has not ended"
        );
    }

    /// FR-001/FR-004: a session that ends with reconnect off ends the station with its
    /// reason, and the CSMS-side handle reports the same thing the fleet event does.
    #[tokio::test]
    async fn edge_both_ends_of_a_session_say_how_it_ended() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let station = builder(port, "CS-ENDS", &[Version::V2_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        assert_eq!(next_opened(&mut fleet).await, Version::V2_1);
        let identity = Identity::new("CS-ENDS").unwrap();
        let session = csms.session(&identity).await.unwrap();

        session.shutdown(Duration::from_secs(1)).await;
        let at_csms = tokio::time::timeout(Duration::from_secs(5), session.closed())
            .await
            .unwrap();
        assert!(at_csms.is_requested(), "{at_csms}");
        let fleet_reason = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let SessionEvent::Closed { reason, .. } = fleet.recv().await.unwrap() {
                    return reason;
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(&at_csms, Ended::Disconnected(r) if *r == fleet_reason));

        let at_station = tokio::time::timeout(Duration::from_secs(5), station.closed())
            .await
            .unwrap();
        assert!(
            matches!(
                &at_station,
                Ended::Disconnected(DisconnectReason::PeerClosed(_))
            ),
            "{at_station}"
        );
        assert!(!at_station.is_requested());
    }

    #[tokio::test]
    async fn edge_a_drained_station_ended_on_request() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let station = builder(port, "CS-DRAIN", &[Version::V2_1])
            .reconnect(true)
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        next_opened(&mut fleet).await;
        station.shutdown(Duration::from_secs(1)).await;
        let ended = tokio::time::timeout(Duration::from_secs(5), station.closed())
            .await
            .unwrap();
        assert!(ended.is_requested(), "{ended}");
    }

    // --- story 2: the CSMS decides the version -------------------------------------------

    #[tokio::test]
    async fn edge_pin_serves_the_pinned_version_over_the_station_preference() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let _station = builder(port, "CS-PIN201", &[Version::V2_1, Version::V2_0_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        assert_eq!(next_opened(&mut fleet).await, Version::V2_0_1);

        // Without a pin, the station's preference among served versions.
        let _other = builder(port, "CS-FREE", &[Version::V2_0_1, Version::V2_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        assert_eq!(next_opened(&mut fleet).await, Version::V2_0_1);
        let _newer = builder(port, "CS-FREE2", &[Version::V2_1, Version::V2_0_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        assert_eq!(next_opened(&mut fleet).await, Version::V2_1);
    }

    #[tokio::test]
    async fn edge_pin_refuses_a_station_that_did_not_offer_it() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let station = builder(port, "CS-PIN201", &[Version::V2_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        let (identity, reason) = next_refusal(&mut fleet).await;
        assert_eq!(identity.unwrap().as_str(), "CS-PIN201");
        assert!(
            matches!(&reason, Refusal::VersionNotOffered { required: Version::V2_0_1, offered }
                if offered == &[Version::V2_1]),
            "{reason}"
        );
        assert_eq!(
            reason.to_string(),
            "the station must use OCPP 2.0.1 but offered [2.1]"
        );
        // The station sees what any version mismatch looks like.
        let ended = station.closed().await;
        assert!(
            matches!(&ended, Ended::ConnectFailed(e)
                if matches!(**e, TransportError::Negotiation(NegotiationError::NoSubprotocol))),
            "{ended}"
        );
    }

    /// SR-004: a pin to a version the CSMS does not serve never falls back to another one.
    #[tokio::test]
    async fn edge_pin_to_an_unserved_version_never_falls_back() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let _station = builder(port, "CS-PIN16", &[Version::V2_1, Version::V1_6])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        let (_, reason) = next_refusal(&mut fleet).await;
        assert!(
            matches!(
                reason,
                Refusal::VersionNotOffered {
                    required: Version::V1_6,
                    ..
                }
            ),
            "{reason}"
        );
    }

    /// SR-003: a pin refusal and a version mismatch are byte-identical on the wire.
    #[tokio::test]
    async fn edge_pin_refusal_looks_like_no_common_version() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let pinned = raw(
            port,
            &upgrade("CS-PIN201", "ocpp2.1", "CS-PIN201", PASSWORD),
        )
        .await;
        assert!(matches!(
            next_refusal(&mut fleet).await.1,
            Refusal::VersionNotOffered { .. }
        ));
        let unserved = raw(port, &upgrade("CS-ANY", "ocpp1.6", "CS-ANY", PASSWORD)).await;
        assert!(matches!(
            next_refusal(&mut fleet).await.1,
            Refusal::NoCommonVersion { .. }
        ));
        assert!(
            pinned.starts_with(b"HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&pinned)
        );
        assert_eq!(pinned, unserved);
    }

    // --- story 3: every refusal says why -------------------------------------------------

    /// SR-003: both 401s are the same bytes to the peer, and different reasons to the operator.
    #[tokio::test]
    async fn edge_refusal_tells_a_username_mismatch_from_a_bad_password() {
        let (port, csms) = pinning_csms(|b| b).await;
        let mut fleet = csms.events();
        let mismatch = raw(port, &upgrade("CS-ANY", "ocpp2.1", "CS-OTHER", PASSWORD)).await;
        assert!(matches!(
            next_refusal(&mut fleet).await.1,
            Refusal::UsernameMismatch
        ));
        let wrong = raw(
            port,
            &upgrade("CS-WRONGPW", "ocpp2.1", "CS-WRONGPW", PASSWORD),
        )
        .await;
        assert!(matches!(
            next_refusal(&mut fleet).await.1,
            Refusal::Rejected
        ));
        assert!(mismatch.starts_with(b"HTTP/1.1 401"));
        assert_eq!(mismatch, wrong);
    }

    #[tokio::test]
    async fn edge_refusal_names_every_handshake_defect() {
        let (port, csms) = pinning_csms(|b| b.handshake_timeout(Duration::from_millis(200))).await;
        let mut fleet = csms.events();

        // No identity in the path.
        raw(
            port,
            &upgrade("", "ocpp2.1", "x", PASSWORD).replace("/ocpp/ ", "/ "),
        )
        .await;
        let (_, reason) = next_refusal(&mut fleet).await;
        assert!(matches!(reason, Refusal::NoIdentity), "{reason}");
        assert_eq!(reason.status(), Some(400));

        // RFC 6455 §4.4: an unsupported WebSocket version gets 426.
        raw(
            port,
            &upgrade("CS-ANY", "ocpp2.1", "CS-ANY", PASSWORD)
                .replace("Sec-WebSocket-Version: 13", "Sec-WebSocket-Version: 8"),
        )
        .await;
        let (_, reason) = next_refusal(&mut fleet).await;
        assert!(
            matches!(reason, Refusal::BadUpgrade { status: 426 }),
            "{reason}"
        );

        // Opened a socket and said nothing: timed out, with no response.
        let silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (identity, reason) = next_refusal(&mut fleet).await;
        assert!(matches!(reason, Refusal::TimedOut), "{reason}");
        assert_eq!((identity, reason.status()), (None, None));
        drop(silent);

        // A malformed request: failed, and the peer's text is escaped (SR-002).
        raw(port, "\x1b[31mPOST /ocpp/CS-ANY HTTP/1.1\r\n\r\n").await;
        let (_, reason) = next_refusal(&mut fleet).await;
        assert!(matches!(reason, Refusal::Failed(_)), "{reason}");
        let line = reason.to_string();
        assert!(!line.contains('\x1b') && !line.contains('\n'), "{line:?}");

        // A TCP health probe — connect, close — is not a refusal.
        drop(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        let quiet =
            tokio::time::timeout(Duration::from_millis(400), next_refusal(&mut fleet)).await;
        assert!(quiet.is_err(), "a probe must not be reported: {quiet:?}");
    }

    #[tokio::test]
    async fn edge_refusal_at_capacity() {
        let (port, csms) = pinning_csms(|b| b.max_connections(0)).await;
        let mut fleet = csms.events();
        let reply = raw(port, &upgrade("CS-ANY", "ocpp2.1", "CS-ANY", PASSWORD)).await;
        assert!(reply.starts_with(b"HTTP/1.1 503"));
        assert!(matches!(
            next_refusal(&mut fleet).await.1,
            Refusal::AtCapacity
        ));
    }

    /// With supersede off, the newer connection is closed with 1008 and reported.
    #[tokio::test]
    async fn edge_refusal_of_a_duplicate_closes_it_properly() {
        let (port, csms) = pinning_csms(|b| b.supersede_existing(false)).await;
        let mut fleet = csms.events();
        let _first = builder(port, "CS-TWICE", &[Version::V2_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        next_opened(&mut fleet).await;
        let second = builder(port, "CS-TWICE", &[Version::V2_1])
            .build()
            .unwrap()
            .spawn()
            .unwrap();
        let (identity, reason) = next_refusal(&mut fleet).await;
        assert_eq!(identity.unwrap().as_str(), "CS-TWICE");
        assert!(matches!(reason, Refusal::Duplicate), "{reason}");
        let ended = tokio::time::timeout(Duration::from_secs(5), second.closed())
            .await
            .unwrap();
        assert!(
            matches!(&ended, Ended::Disconnected(DisconnectReason::PeerClosed(Some(frame)))
                if frame.code == ocpp_kit::transport::CloseCode::POLICY
                    && frame.reason == "already connected"),
            "{ended}"
        );
    }

    /// SR-005: refusals ride the bounded fleet channel; a subscriber that does not read is told
    /// it lagged, and the stream keeps flowing.
    #[tokio::test]
    async fn edge_a_refusal_flood_lags_a_slow_subscriber_without_growing() {
        let (port, csms) = pinning_csms(|b| b.max_connections(0)).await;
        let mut idle = csms.events();
        for _ in 0..1_100 {
            raw(port, &upgrade("CS-ANY", "ocpp2.1", "CS-ANY", PASSWORD)).await;
        }
        match idle.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => assert!(missed > 0),
            other => panic!("expected a lag notice, got {other:?}"),
        }
        let mut fresh = csms.events();
        raw(port, &upgrade("CS-ANY", "ocpp2.1", "CS-ANY", PASSWORD)).await;
        assert!(matches!(
            next_refusal(&mut fresh).await.1,
            Refusal::AtCapacity
        ));
    }
}
