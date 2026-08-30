//! The engine must survive arbitrary input from the peer without panicking, must never
//! answer a `SEND`, and must never emit a frame it cannot itself parse.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ocpp_kit::Version;
use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Output, Role};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else { return };

    for role in [Role::ChargingStation, Role::Csms] {
        let mut engine = Engine::new(EngineConfig::new(role, Version::V2_1));
        let mut clock = 0u64;
        engine.handle(Instant::from_millis(clock), Input::Connected { version: Version::V2_1 });

        for line in text.lines() {
            engine.handle(Instant::from_millis(clock), Input::Received(line));
            clock = clock.wrapping_add(1_000);
            engine.handle(Instant::from_millis(clock), Input::Timeout);

            for output in engine.drain() {
                if let Output::Transmit(frame) = output {
                    // Whatever the engine emits must be a frame it can parse itself.
                    ocpp_kit::rpc::Frame::parse(&frame, Version::V2_1)
                        .expect("the engine emits well-formed frames");
                }
            }
        }
        engine.handle(Instant::from_millis(clock), Input::Disconnected);
        let _ = engine.drain();
    }
});
