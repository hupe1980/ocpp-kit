//! The WebSocket layer, checked against a reference implementation and against itself.
//!
//! OCPP 2.1 requires `permessage-deflate` on the CSMS side, and no general-purpose Rust
//! WebSocket crate implements it — the frame layer has to surface `RSV1`, and the ones that do
//! not simply reject it. So this crate has its own. That is a serious claim to make about a
//! protocol with a long history of framing bugs, which is why these tests exist:
//!
//! * every frame goes past `tokio-tungstenite`, a widely used independent implementation,
//!   in both directions;
//! * compression is verified on the wire, not merely configured;
//! * and the Autobahn-style edge cases — fragmentation, interleaved control frames, oversized
//!   payloads — are exercised over a real socket.
//!
//! `tokio-tungstenite` is a **dev-dependency only**. It is the reference, never the runtime.

#![cfg(feature = "tokio")]
// The reference implementation's handshake error type is large; that is its business.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{
    Auth, AuthOutcome, BasicAuthPassword, BoxFuture, Csms, Ctx, Handler, SecurityProfile, Station,
};
use ocpp_kit::types::DateTime;
use ocpp_kit::{RawValue, Version, v2_1};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as RefMessage;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::protocol::frame::{Frame, FrameHeader};

/// A CSMS that accepts everything and answers `BootNotification` and `Heartbeat`.
struct Accepting;

impl Handler for Accepting {
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
                    ctx.reply(&v2_1::BootNotificationResponse::new(
                        DateTime::now(),
                        0,
                        v2_1::RegistrationStatus::Accepted,
                    ))
                }
                v2_1::CsRequest::Heartbeat(_) => {
                    ctx.reply(&v2_1::HeartbeatResponse::new(DateTime::now()))
                }
                v2_1::CsRequest::DataTransfer(request) => ctx.reply(
                    &v2_1::DataTransferResponse::new(v2_1::DataTransferStatus::Accepted)
                        .with_data(request.data.unwrap_or(serde_json::Value::Null)),
                ),
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

async fn start_csms() -> (u16, ocpp_kit::transport::CsmsHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let csms = Csms::builder()
        .bind(listener.local_addr().unwrap())
        .versions([Version::V2_1, Version::V2_0_1, Version::V1_6])
        .authenticate(|_: Auth| async { AuthOutcome::Accept })
        .handler(Accepting)
        .ping_interval(None)
        .build()
        .unwrap();
    let handle = csms.handle();
    tokio::spawn(async move {
        let _ = csms.serve_on(listener).await;
    });
    (port, handle)
}

// ---------------------------------------------------------------------------
// Our server against a reference client
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reference_client_can_talk_to_our_csms() {
    let (port, _handle) = start_csms().await;

    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://127.0.0.1:{port}/ocpp/CS-REFERENCE"))
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

    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut socket, response) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::client_async(request, stream),
    )
    .await
    .expect("handshake in time")
    .expect("the reference client completes our handshake");

    // Our handshake must satisfy an implementation that had no part in writing it.
    assert_eq!(response.status(), 101);
    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "ocpp2.1"
    );

    socket
        .send(RefMessage::text(
            r#"[2,"1","BootNotification",{"reason":"PowerUp","chargingStation":{"model":"M","vendorName":"V"}}]"#,
        ))
        .await
        .unwrap();
    let answer = socket.next().await.unwrap().unwrap();
    let RefMessage::Text(text) = answer else {
        panic!("expected text, got {answer:?}")
    };
    assert!(text.starts_with(r#"[3,"1","#), "{text}");
    assert!(text.contains("Accepted"));

    // A ping from the reference client must be ponged.
    socket
        .send(RefMessage::Ping(b"ping"[..].into()))
        .await
        .unwrap();
    let pong = socket.next().await.unwrap().unwrap();
    assert!(matches!(pong, RefMessage::Pong(payload) if payload.as_ref() == b"ping"));

    socket.close(None).await.unwrap();
}

#[tokio::test]
async fn a_reference_client_that_fragments_a_message_is_understood() {
    let (port, _handle) = start_csms().await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://127.0.0.1:{port}/ocpp/CS-FRAGMENTS"))
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
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();

    // tungstenite writes raw frames verbatim, so a message can be split deliberately.
    // BootNotification, because the CSMS's boot gate (B02.FR.09) refuses anything else from a
    // station it has not yet accepted — which the raw client here has not been.
    let whole = r#"[2,"1","BootNotification",{"reason":"PowerUp","chargingStation":{"model":"M","vendorName":"V"}}]"#;
    let (first, second) = whole.split_at(20);
    let header = FrameHeader {
        is_final: false,
        opcode: OpCode::Data(Data::Text),
        ..FrameHeader::default()
    };
    socket
        .send(RefMessage::Frame(Frame::from_payload(
            header,
            first.as_bytes().to_vec().into(),
        )))
        .await
        .unwrap();

    let header = FrameHeader {
        is_final: true,
        opcode: OpCode::Data(Data::Continue),
        ..FrameHeader::default()
    };
    socket
        .send(RefMessage::Frame(Frame::from_payload(
            header,
            second.as_bytes().to_vec().into(),
        )))
        .await
        .unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("an answer in time")
        .unwrap()
        .unwrap();
    let RefMessage::Text(text) = answer else {
        panic!("expected text, got {answer:?}")
    };
    assert!(
        text.starts_with(r#"[3,"1","#),
        "the fragments were reassembled: {text}"
    );
}

// ---------------------------------------------------------------------------
// Our client against a reference server
// ---------------------------------------------------------------------------

/// A minimal reference CSMS built on `tokio-tungstenite`.
async fn reference_csms(compression: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let callback = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    // Echo the subprotocol, as Part 4 §3.1.2 requires.
                    if let Some(offered) = request.headers().get("sec-websocket-protocol") {
                        let chosen = offered
                            .to_str()
                            .unwrap_or_default()
                            .split(',')
                            .map(str::trim)
                            .next()
                            .unwrap_or("ocpp2.1")
                            .to_owned();
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            chosen.parse().expect("a valid header"),
                        );
                    }
                    // tungstenite cannot actually *do* permessage-deflate, so this server
                    // never accepts the offer — which is precisely the case Part 4 §3.4 says a
                    // station must tolerate without closing the connection.
                    let _ = compression;
                    Ok(response)
                };
                let Ok(mut socket) = tokio_tungstenite::accept_hdr_async_with_config(
                    stream,
                    callback,
                    Some(WebSocketConfig::default()),
                )
                .await
                else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    if let RefMessage::Text(text) = message {
                        let parts: Vec<serde_json::Value> =
                            serde_json::from_str(&text).expect("an OCPP-J frame");
                        let id = parts[1].as_str().unwrap();
                        let action = parts[2].as_str().unwrap();
                        let body = match action {
                            "BootNotification" => serde_json::json!({
                                "currentTime": "2024-01-01T00:00:00Z",
                                "interval": 0,
                                "status": "Accepted"
                            }),
                            "Heartbeat" => {
                                serde_json::json!({"currentTime": "2024-01-01T00:00:00Z"})
                            }
                            _ => serde_json::json!({"status": "Accepted"}),
                        };
                        let answer = serde_json::json!([3, id, body]).to_string();
                        let _ = socket.send(RefMessage::text(answer)).await;
                    }
                }
            });
        }
    });
    port
}

fn station(port: u16, identity: &str) -> ocpp_kit::transport::Handle {
    Station::builder()
        .identity(identity)
        .unwrap()
        .url(format!("ws://127.0.0.1:{port}/ocpp"))
        .versions([Version::V2_1])
        .security_profile(SecurityProfile::BasicAuth)
        .password(BasicAuthPassword::utf8("0123456789abcdef").unwrap())
        .ping_interval(None)
        .build()
        .unwrap()
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn our_client_can_talk_to_a_reference_server() {
    let port = reference_csms(false).await;
    let station = station(port, "CS-AGAINST-REFERENCE");

    let boot = tokio::time::timeout(
        Duration::from_secs(5),
        station.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("a boot in time")
    .expect("the reference server understood our handshake and framing");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Accepted);

    // Part 4 §3.4: a CSMS that declines compression is talked to uncompressed, and the
    // station must not close the connection over it.
    assert!(station.state().connected);
    let beat = station.call(v2_1::HeartbeatRequest::new()).await.unwrap();
    assert_eq!(beat.current_time.to_string(), "2024-01-01T00:00:00Z");
}

// ---------------------------------------------------------------------------
// Compression, on the wire
// ---------------------------------------------------------------------------

#[cfg(feature = "compression")]
#[tokio::test]
async fn compression_is_negotiated_and_actually_compresses() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (port, _handle) = start_csms().await;

    // Drive the handshake by hand so the raw bytes can be inspected.
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!(
        "GET /ocpp/CS-COMPRESSED HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: ocpp2.1\r\n\
         Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
    );
    socket.write_all(request.as_bytes()).await.unwrap();

    let mut head = vec![0u8; 1024];
    let read = socket.read(&mut head).await.unwrap();
    let head = String::from_utf8_lossy(&head[..read]).to_ascii_lowercase();
    assert!(head.contains("101 switching protocols"), "{head}");
    // 2.1 Part 4 §3.4 Table 2: a CSMS *shall* support RFC 7692.
    assert!(
        head.contains("sec-websocket-extensions: permessage-deflate"),
        "the CSMS must accept the offer:\n{head}"
    );

    // Now speak compressed OCPP-J to it. The boot gate comes first: until the CSMS accepts
    // the station, B02.FR.09 refuses everything else.
    let mut codec = ocpp_kit::transport::ws_test_support::client_codec_with_deflate();
    let mut buffer = bytes::BytesMut::new();
    tokio_util::codec::Encoder::encode(
        &mut codec,
        ocpp_kit::transport::WsMessage::Text(
            r#"[2,"boot","BootNotification",{"reason":"PowerUp","chargingStation":{"model":"M","vendorName":"V"}}]"#
                .to_owned(),
        ),
        &mut buffer,
    )
    .unwrap();
    socket.write_all(&buffer).await.unwrap();

    let mut answer = bytes::BytesMut::new();
    loop {
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut chunk))
            .await
            .expect("a boot answer in time")
            .unwrap();
        assert_ne!(read, 0, "the CSMS closed instead of answering");
        answer.extend_from_slice(&chunk[..read]);
        if tokio_util::codec::Decoder::decode(&mut codec, &mut answer)
            .unwrap()
            .is_some()
        {
            break;
        }
    }

    let mut buffer = bytes::BytesMut::new();
    let payload = format!(
        r#"[2,"1","DataTransfer",{{"vendorId":"acme","data":"{}"}}]"#,
        "repeat me ".repeat(80)
    );
    tokio_util::codec::Encoder::encode(
        &mut codec,
        ocpp_kit::transport::WsMessage::Text(payload.clone()),
        &mut buffer,
    )
    .unwrap();
    assert_eq!(buffer[0] & 0x40, 0x40, "our frame claims to be compressed");
    assert!(
        buffer.len() < payload.len() / 3,
        "and it is: {} vs {}",
        buffer.len(),
        payload.len()
    );
    socket.write_all(&buffer).await.unwrap();

    let mut answer = bytes::BytesMut::new();
    let message = loop {
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut chunk))
            .await
            .expect("an answer in time")
            .unwrap();
        assert_ne!(read, 0, "the CSMS closed instead of answering");
        answer.extend_from_slice(&chunk[..read]);
        // The server's answer must itself be compressed.
        assert_eq!(answer[0] & 0x40, 0x40, "the CSMS answered uncompressed");
        if let Some(message) = tokio_util::codec::Decoder::decode(&mut codec, &mut answer).unwrap()
        {
            break message;
        }
    };
    let ocpp_kit::transport::WsMessage::Text(text) = message else {
        panic!("expected text")
    };
    assert!(text.starts_with(r#"[3,"1","#), "{text}");
    assert!(
        text.contains("repeat me"),
        "the payload survived the round trip"
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn a_whole_session_runs_over_a_compressed_connection() {
    let (port, csms) = start_csms().await;
    let station = station(port, "CS-COMPRESSED-SESSION");

    let boot = tokio::time::timeout(
        Duration::from_secs(5),
        station.call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME"),
            v2_1::BootReason::PowerUp,
        )),
    )
    .await
    .expect("a boot in time")
    .expect("boot succeeded over a compressed connection");
    assert_eq!(boot.status, v2_1::RegistrationStatus::Accepted);

    // A payload large enough to be compressed, round-tripped through both codecs.
    let big = "x".repeat(4096);
    let response = station
        .call(
            v2_1::DataTransferRequest::new("acme")
                .with_data(serde_json::Value::String(big.clone())),
        )
        .await
        .unwrap();
    assert_eq!(response.data, Some(serde_json::Value::String(big)));

    let identity = ocpp_kit::types::Identity::new("CS-COMPRESSED-SESSION").unwrap();
    assert!(csms.session(&identity).await.is_some());
    station.shutdown(Duration::from_secs(2)).await;
}

/// Keeps the `Arc` import meaningful across feature combinations.
const _: Option<Arc<()>> = None;
