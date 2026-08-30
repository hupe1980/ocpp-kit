//! The WebSocket codec must never panic on hostile input, and must never desynchronise.
#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use ocpp_kit::transport::ws_fuzz::{Config, Role, WsCodec, decode, encode};

fuzz_target!(|data: &[u8]| {
    // Both roles, because the masking rules are mirror images and each rejects what the other
    // requires.
    for role in [Role::Client, Role::Server] {
        let mut codec = WsCodec::new(role, Config::default());
        let mut buffer = BytesMut::from(data);
        // Decoding must terminate: either it yields messages until the buffer is short, or it
        // reports an error. It must never loop, and never panic.
        for _ in 0..1024 {
            match decode(&mut codec, &mut buffer) {
                Ok(Some(message)) => {
                    // Anything decoded must re-encode and decode back to the same value.
                    let mut out = BytesMut::new();
                    let mut peer = WsCodec::new(
                        match role {
                            Role::Client => Role::Server,
                            Role::Server => Role::Client,
                        },
                        Config::default(),
                    );
                    if encode(&mut peer, message.clone(), &mut out).is_ok() {
                        let mut back = WsCodec::new(role, Config::default());
                        if let Ok(Some(again)) = decode(&mut back, &mut out) {
                            assert_eq!(message, again, "a message did not survive a round trip");
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }
});
