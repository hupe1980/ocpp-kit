//! Payload decoding must never panic, whatever policy is in force, and a payload that decodes
//! must re-serialize.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::v2_1;
use serde_json::value::RawValue;

fuzz_target!(|data: &[u8]| {
    // The first byte picks the action, so the corpus explores the whole catalogue.
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
            // Whatever the types produced must itself decode under the policy that produced
            // it, or the types are not closed under their own serialization. Not under
            // `strict()`: a lenient decode preserves an undefined enum value by design, and
            // strict rejects exactly that (tests/data/fuzz-regressions/payload/).
            v2_1::transcode_request(action, &normalized, &options).expect("our own output decodes");
        }
        if action.has_response() {
            let _ = v2_1::transcode_response(action, &payload, &options);
        }
    }
});
