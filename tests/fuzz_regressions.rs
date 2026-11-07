//! The fuzz targets' properties, checked on stable Rust over every committed input.
//!
//! Each `check_*` function below is the body of the `fuzz/fuzz_targets/<target>.rs` of the same
//! name, restated here because a libFuzzer binary cannot be linked into `cargo test`. Two sets
//! of inputs go through each one:
//!
//! - `tests/data/fuzz-regressions/<target>/` — every crash a fuzzer has found. A file lands
//!   there the moment it is found, so the suite fails until the fix does;
//! - `fuzz/corpus/<target>/` — the committed seed corpus, so a change that makes an ordinary,
//!   real-shaped input panic fails here before anyone runs a fuzzer.
//!
//! The two copies of each property can drift apart; `the_targets_are_the_ones_replicated`
//! at least notices a target being added or removed.

#![cfg(all(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use ocpp_kit::Version;
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Output, Role};
use ocpp_kit::rpc::Frame;
use ocpp_kit::v2_1;
use serde_json::value::RawValue;

const TARGETS: [&str; 4] = ["engine", "frame", "payload", "websocket"];

/// `fuzz/fuzz_targets/frame.rs`.
fn check_frame(data: &[u8]) {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    for version in [Version::V1_6, Version::V2_0_1, Version::V2_1] {
        match Frame::parse(text, version) {
            Ok(frame) => {
                let json = frame.to_json(version).expect("a parsed frame serializes");
                let reparsed = Frame::parse(&json, version).expect("round trip");
                assert_eq!(frame, reparsed);
            }
            Err(error) => {
                let _ = error.error_code();
                let _ = error.reply_id();
                let _ = error.is_ignorable(version);
            }
        }
    }
}

/// `fuzz/fuzz_targets/payload.rs`.
fn check_payload(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = core::str::from_utf8(rest) else {
        return;
    };
    let Ok(payload) = RawValue::from_string(text.to_owned()) else {
        return;
    };

    let actions = v2_1::Action::ALL;
    let action = actions[usize::from(selector) % actions.len()];

    for options in [
        DecodeOptions::strict(),
        DecodeOptions::pedantic(),
        DecodeOptions::lenient(),
    ] {
        if let Ok(normalized) = v2_1::transcode_request(action, &payload, &options) {
            v2_1::transcode_request(action, &normalized, &options).expect("our own output decodes");
        }
        if action.has_response() {
            let _ = v2_1::transcode_response(action, &payload, &options);
        }
    }
}

/// `fuzz/fuzz_targets/engine.rs`.
fn check_engine(data: &[u8]) {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    for role in [Role::ChargingStation, Role::Csms] {
        let mut engine = Engine::new(EngineConfig::new(role, Version::V2_1));
        let mut clock = 0u64;
        engine.handle(
            Instant::from_millis(clock),
            Input::Connected {
                version: Version::V2_1,
            },
        );
        for line in text.lines() {
            engine.handle(Instant::from_millis(clock), Input::Received(line));
            clock = clock.wrapping_add(1_000);
            engine.handle(Instant::from_millis(clock), Input::Timeout);
            for output in engine.drain() {
                if let Output::Transmit(frame) = output {
                    Frame::parse(&frame, Version::V2_1)
                        .expect("the engine emits well-formed frames");
                }
            }
        }
        engine.handle(Instant::from_millis(clock), Input::Disconnected);
        let _ = engine.drain();
    }
}

/// `fuzz/fuzz_targets/websocket.rs`. The codec is behind the `tokio` feature.
#[cfg(feature = "tokio")]
fn check_websocket(data: &[u8]) {
    use bytes::BytesMut;
    use ocpp_kit::transport::ws_fuzz::{Config, Role, WsCodec, decode, encode};

    for role in [Role::Client, Role::Server] {
        let mut codec = WsCodec::new(role, Config::default());
        let mut buffer = BytesMut::from(data);
        for _ in 0..1024 {
            match decode(&mut codec, &mut buffer) {
                Ok(Some(message)) => {
                    let mut out = BytesMut::new();
                    let peer_role = match role {
                        Role::Client => Role::Server,
                        Role::Server => Role::Client,
                    };
                    let mut peer = WsCodec::new(peer_role, Config::default());
                    if encode(&mut peer, message.clone(), &mut out).is_ok() {
                        let mut back = WsCodec::new(role, Config::default());
                        if let Ok(Some(again)) = decode(&mut back, &mut out) {
                            assert_eq!(message, again, "a message did not survive a round trip");
                        }
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every regular file under `dir`, sorted; an absent directory is an empty set.
fn inputs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.is_file())
        .filter(|path| {
            // `.gitkeep` keeps an empty regression directory in git; it is not an input.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.starts_with('.')
        })
        .collect();
    files.sort();
    files
}

/// Runs `check` over every input for `target`, and fails naming each one that panicked.
fn replay(target: &str, check: fn(&[u8]), require_corpus: bool) {
    let corpus = inputs(&root().join("fuzz/corpus").join(target));
    let regressions = inputs(&root().join("tests/data/fuzz-regressions").join(target));
    if require_corpus {
        assert!(
            !corpus.is_empty(),
            "fuzz/corpus/{target}/ is empty — the seed corpus is part of the suite"
        );
    }

    let mut failures = Vec::new();
    for path in corpus.iter().chain(&regressions) {
        let data = std::fs::read(path).expect("readable input");
        if catch_unwind(AssertUnwindSafe(|| check(&data))).is_err() {
            failures.push(
                path.strip_prefix(root())
                    .unwrap_or(path)
                    .display()
                    .to_string(),
            );
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} `{target}` input(s) panicked:\n  {}",
        failures.len(),
        corpus.len() + regressions.len(),
        failures.join("\n  ")
    );
}

#[test]
fn frame_inputs_hold_the_frame_property() {
    replay("frame", check_frame, true);
}

#[test]
fn payload_inputs_hold_the_payload_property() {
    replay("payload", check_payload, true);
}

#[test]
fn engine_inputs_hold_the_engine_property() {
    replay("engine", check_engine, true);
}

#[cfg(feature = "tokio")]
#[test]
fn websocket_inputs_hold_the_websocket_property() {
    replay("websocket", check_websocket, true);
}

#[test]
fn the_targets_are_the_ones_replicated() {
    let mut found: Vec<String> = inputs(&root().join("fuzz/fuzz_targets"))
        .iter()
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_owned))
        .collect();
    found.sort();
    assert_eq!(
        found, TARGETS,
        "a fuzz target was added or removed: replicate its body here"
    );
    for target in TARGETS {
        assert!(
            root()
                .join("tests/data/fuzz-regressions")
                .join(target)
                .is_dir(),
            "tests/data/fuzz-regressions/{target}/ is missing"
        );
    }
}

/// The replay itself must be able to fail, or the corpus tests above prove nothing.
#[test]
fn a_panicking_input_is_reported() {
    fn always_panics(_: &[u8]) {
        panic!("planted");
    }
    let outcome = catch_unwind(|| replay("frame", always_panics, true));
    assert!(outcome.is_err());
}
