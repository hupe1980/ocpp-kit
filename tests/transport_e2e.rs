//! End-to-end tests over real sockets: a Charging Station, a CSMS, and a Local Controller
//! between them.

#![cfg(feature = "tokio")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ocpp_kit::RawValue;
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{
    Auth, AuthOutcome, BoxFuture, Csms, CsmsHandle, Ctx, Direction, Handle, Handler,
    LocalController, NetworkProfile, NetworkProfiles, RelayDecision, SecurityProfile, SessionEvent,
    Station,
};
use ocpp_kit::types::Identity;
use ocpp_kit::{Version, v2_1};
use tokio::net::TcpListener;

/// A CSMS that accepts boot notifications and counts what it saw.
struct CsmsSide {
    boots: AtomicUsize,
    boot_status: v2_1::RegistrationStatus,
}

impl CsmsSide {
    fn new(boot_status: v2_1::RegistrationStatus) -> Self {
        Self {
            boots: AtomicUsize::new(0),
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
            let action = v2_1::Action::from_wire(&request.action)
                .ok_or_else(|| CallError::not_implemented(&request.action))?;
            match v2_1::CsRequest::decode(action, &request.payload, ctx.decode_options())? {
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
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

/// A Charging Station that answers `Reset` and nothing else.
struct StationSide;

impl Handler for StationSide {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            let action = v2_1::Action::from_wire(&request.action)
                .ok_or_else(|| CallError::not_implemented(&request.action))?;
            match v2_1::CsmsRequest::decode(action, &request.payload, ctx.decode_options())? {
                v2_1::CsmsRequest::Reset(_) => {
                    ctx.reply(&v2_1::ResetResponse::new(v2_1::ResetStatus::Accepted))
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
    AuthOutcome::Accept
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
    assert!(station.wait_ready().await);
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

#[tokio::test]
async fn bad_credentials_get_a_401_and_an_unknown_identity_a_404() {
    let handler = Arc::new(CsmsSide::new(v2_1::RegistrationStatus::Accepted));
    let (port, csms) = start_csms(handler, |auth: Auth| async move {
        match auth.identity.as_str() {
            "CS-KNOWN" => AuthOutcome::Accept,
            "CS-WRONGPW" => AuthOutcome::Reject,
            _ => AuthOutcome::Unknown,
        }
    })
    .await;
    let mut events = csms.events();

    // The station's reconnect loop keeps retrying, so build it without reconnect.
    for (identity, expected) in [("CS-WRONGPW", 401u16), ("CS-MISSING", 404)] {
        let error = Station::builder()
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
            .unwrap_err();
        assert!(error.to_string().contains("websocket") || error.to_string().contains("HTTP"));

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("an event arrived")
            .unwrap();
        match event {
            SessionEvent::Refused { status, .. } => assert_eq!(status, expected),
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

    // The station speaks only 1.6. Part 4 §3.1.1: the handshake succeeds *without* a
    // subprotocol header, and the connection is closed immediately — so negotiation fails
    // rather than the two silently talking past each other.
    let outcome = Station::builder()
        .identity("CS-OLD")
        .unwrap()
        .url(format!("ws://127.0.0.1:{port}/ocpp"))
        .versions([Version::V1_6])
        .password(ocpp_kit::transport::BasicAuthPassword::hex("00112233").unwrap())
        .reconnect(false)
        .ping_interval(None)
        .build()
        .unwrap()
        .run()
        .await;
    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("did not select"),
        "expected a negotiation failure, got {error}"
    );
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
    assert!(closed.1.contains("pong"), "{}", closed.1);
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
                AuthOutcome::Accept
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
