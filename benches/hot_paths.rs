//! The paths a CSMS runs millions of times a day.
//!
//! ```console
//! cargo bench --features full
//! ```

// Benchmark payloads are a few hundred bytes; the ratio arithmetic cannot lose anything.
#![allow(clippy::cast_precision_loss)]

use criterion::{Criterion, criterion_main};
use ocpp_kit::Version;
use ocpp_kit::decode::{DecodeOptions, decode_payload};
use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Role};
use ocpp_kit::rpc::Frame;
use ocpp_kit::v2_1;
use ocpp_kit::validate::Validate;
use serde_json::value::RawValue;
use std::hint::black_box;

const TRANSACTION_EVENT: &str = r#"{
  "eventType": "Updated",
  "timestamp": "2024-01-01T12:00:00Z",
  "triggerReason": "MeterValuePeriodic",
  "seqNo": 42,
  "transactionInfo": { "transactionId": "tx-0001", "chargingState": "Charging" },
  "evse": { "id": 1, "connectorId": 1 },
  "meterValue": [
    {
      "timestamp": "2024-01-01T12:00:00Z",
      "sampledValue": [
        { "value": 12345.6, "measurand": "Energy.Active.Import.Register" },
        { "value": 16.0, "measurand": "Current.Import", "phase": "L1" }
      ]
    }
  ]
}"#;

fn frame_text() -> String {
    format!(r#"[2,"19223201","TransactionEvent",{TRANSACTION_EVENT}]"#)
}

fn framing(c: &mut Criterion) {
    let text = frame_text();
    c.bench_function("frame/parse", |b| {
        b.iter(|| {
            let frame = Frame::parse(black_box(&text), Version::V2_1).unwrap();
            black_box(frame.action());
        });
    });

    let frame = Frame::parse(&text, Version::V2_1).unwrap();
    c.bench_function("frame/serialize", |b| {
        b.iter(|| black_box(frame.to_json(Version::V2_1).unwrap()));
    });
}

fn payloads(c: &mut Criterion) {
    let payload = RawValue::from_string(TRANSACTION_EVENT.to_owned()).unwrap();
    let strict = DecodeOptions::strict();

    c.bench_function("payload/decode+validate", |b| {
        b.iter(|| {
            let request: v2_1::TransactionEventRequest =
                decode_payload(black_box(&payload), &strict).unwrap();
            black_box(request.seq_no)
        });
    });

    let request: v2_1::TransactionEventRequest = decode_payload(&payload, &strict).unwrap();
    c.bench_function("payload/validate", |b| {
        b.iter(|| black_box(request.validate().is_ok()));
    });
    c.bench_function("payload/serialize", |b| {
        b.iter(|| black_box(serde_json::to_string(&request).unwrap()));
    });

    // The lenient path must cost nothing extra when the payload is already valid: the repair
    // loop only runs after a strict parse has failed.
    let lenient = DecodeOptions::lenient();
    c.bench_function(
        "payload/decode+validate (lenient policy, valid input)",
        |b| {
            b.iter(|| {
                let request: v2_1::TransactionEventRequest =
                    decode_payload(black_box(&payload), &lenient).unwrap();
                black_box(request.seq_no)
            });
        },
    );
}

fn engine(c: &mut Criterion) {
    let text = frame_text();
    c.bench_function("engine/receive+respond", |b| {
        b.iter(|| {
            let mut engine = Engine::new(EngineConfig::new(Role::Csms, Version::V2_1));
            engine.handle(
                NOW,
                Input::Connected {
                    version: Version::V2_1,
                },
            );
            engine.handle(NOW, Input::Received(black_box(&text)));
            black_box(engine.drain().len())
        });
    });
}

/// The WebSocket layer, which every byte passes through twice.
#[cfg(feature = "tokio")]
fn websocket(c: &mut Criterion) {
    use bytes::BytesMut;
    use ocpp_kit::transport::WsMessage;
    use ocpp_kit::transport::ws_fuzz::{Config, Role, WsCodec, encode};
    use tokio_util::codec::Decoder as _;

    let text = frame_text();

    let mut client = WsCodec::new(Role::Client, Config::default());
    c.bench_function("ws/encode (masked)", |b| {
        b.iter(|| {
            let mut out = BytesMut::new();
            encode(
                &mut client,
                WsMessage::Text(black_box(text.clone())),
                &mut out,
            )
            .unwrap();
            black_box(out.len())
        });
    });

    let mut writer = WsCodec::new(Role::Client, Config::default());
    let mut framed = BytesMut::new();
    encode(&mut writer, WsMessage::Text(text.clone()), &mut framed).unwrap();
    c.bench_function("ws/decode (unmask)", |b| {
        b.iter(|| {
            let mut server = WsCodec::new(Role::Server, Config::default());
            let mut buffer = framed.clone();
            black_box(server.decode(&mut buffer).unwrap().is_some())
        });
    });

    #[cfg(feature = "compression")]
    {
        use ocpp_kit::transport::ws_test_support::client_codec_with_deflate;
        let mut deflating = client_codec_with_deflate();
        c.bench_function("ws/encode (permessage-deflate)", |b| {
            b.iter(|| {
                let mut out = BytesMut::new();
                encode(
                    &mut deflating,
                    WsMessage::Text(black_box(text.clone())),
                    &mut out,
                )
                .unwrap();
                black_box(out.len())
            });
        });

        // How much smaller the wire actually gets, reported once.
        let mut sizer = client_codec_with_deflate();
        let mut compressed = BytesMut::new();
        for _ in 0..8 {
            compressed.clear();
            encode(&mut sizer, WsMessage::Text(text.clone()), &mut compressed).unwrap();
        }
        println!(
            "permessage-deflate: {} bytes -> {} bytes ({:.0}% of the original)",
            text.len(),
            compressed.len(),
            100.0 * compressed.len() as f64 / text.len() as f64,
        );
    }
}

#[cfg(not(feature = "tokio"))]
fn websocket(_: &mut Criterion) {}

#[allow(missing_docs)]
mod group {
    use super::{engine, framing, payloads, websocket};
    criterion::criterion_group!(benches, framing, payloads, engine, websocket);
}

use group::benches;

/// The engine takes the driver's clock on every entry point. A test that is not
/// exercising a timer supplies the origin and moves on.
const NOW: Instant = Instant::ZERO;
criterion_main!(benches);
