//! The OCPP-J frame parser must never panic, and whatever it parses must re-serialize.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ocpp_kit::Version;
use ocpp_kit::rpc::Frame;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else { return };
    for version in [Version::V1_6, Version::V2_0_1, Version::V2_1] {
        match Frame::parse(text, version) {
            Ok(frame) => {
                // Anything that parsed must serialize, and the result must parse back to the
                // same frame — that is what makes relaying safe.
                let json = frame.to_json(version).expect("a parsed frame serializes");
                let reparsed = Frame::parse(&json, version).expect("round trip");
                assert_eq!(frame, reparsed);
            }
            Err(error) => {
                // The error must always be able to say how to answer it.
                let _ = error.error_code();
                let _ = error.reply_id();
                let _ = error.is_ignorable(version);
            }
        }
    }
});
