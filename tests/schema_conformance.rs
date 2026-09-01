//! Every generated type is checked against the official OCA JSON schema it came from.
//!
//! For each action and version this generates pseudo-random *schema-valid* payloads, feeds
//! them through the Rust types, and then asks two questions of the result:
//!
//! 1. does it still carry every member and value the schema put there — i.e. do the types
//!    model the payload **completely**, and
//! 2. is it itself valid against the schema — i.e. do the types produce **only** what the
//!    schema allows?
//!
//! Together those catch the mistakes that hand-written OCPP libraries actually make: a
//! member typed as optional that is required, a missing enum value, a dropped field, a
//! constraint the type widens.

mod support;

use ocpp_kit::RawValue;
use ocpp_kit::Version;
use ocpp_kit::decode::DecodeOptions;
use serde_json::Value;

use support::{Generator, differences, schema_path, validate};

/// How many random instances per schema. Raise it locally to hunt for a rare case; the
/// generator is deterministic, so a failure always reproduces.
const INSTANCES_PER_SCHEMA: u64 = 24;

type Transcode = fn(&str, bool, &RawValue, &DecodeOptions) -> Result<Box<RawValue>, String>;

fn check_version(version: Version, actions: &[&'static str], transcode: Transcode) {
    let options = DecodeOptions::pedantic();
    let mut checked = 0usize;

    for action in actions {
        for response in [false, true] {
            let Some(path) = schema_path(version, action, response) else {
                continue;
            };
            let schema: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

            for seed in 0..INSTANCES_PER_SCHEMA {
                let mut generator = Generator::new(&schema, seed.wrapping_mul(0x9e37_79b9) + 1);
                let instance = generator.generate();

                // Sanity: the generator must itself produce schema-valid input.
                let problems = validate(&schema, &instance);
                assert!(
                    problems.is_empty(),
                    "generator produced an invalid instance for {}: {problems:?}\n{instance:#}",
                    path.display()
                );

                let raw = RawValue::from_string(instance.to_string()).unwrap();
                let kind = if response { "Response" } else { "Request" };
                let produced = transcode(action, response, &raw, &options).unwrap_or_else(|error| {
                    panic!(
                        "OCPP {version} {action}{kind} (seed {seed}) failed to decode: {error}\n{instance:#}"
                    )
                });
                let produced: Value = serde_json::from_str(produced.get()).unwrap();

                let mut lost = Vec::new();
                differences(&instance, &produced, "", &mut lost);
                assert!(
                    lost.is_empty(),
                    "OCPP {version} {action}{kind} (seed {seed}) does not round-trip: {lost:?}"
                );

                let problems = validate(&schema, &produced);
                assert!(
                    problems.is_empty(),
                    "OCPP {version} {action}{kind} (seed {seed}) produced schema-invalid JSON: {problems:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no schemas were checked for OCPP {version}");
    println!("OCPP {version}: {checked} payload round trips checked");
}

macro_rules! version_suite {
    ($name:ident, $version:expr, $module:ident) => {
        #[test]
        fn $name() {
            use ocpp_kit::$module::{Action, transcode_request, transcode_response};
            let actions: Vec<&'static str> =
                Action::ALL.iter().map(|action| action.as_str()).collect();
            check_version($version, &actions, |action, response, payload, options| {
                let action = Action::from_wire(action).expect("known action");
                let result = if response {
                    transcode_response(action, payload, options)
                } else {
                    transcode_request(action, payload, options)
                };
                result.map_err(|error| error.to_string())
            });
        }
    };
}

#[cfg(feature = "v1_6")]
version_suite!(ocpp_16_types_match_the_schemas, Version::V1_6, v1_6);
#[cfg(feature = "v2_0_1")]
version_suite!(ocpp_201_types_match_the_schemas, Version::V2_0_1, v2_0_1);
#[cfg(feature = "v2_1")]
version_suite!(ocpp_21_types_match_the_schemas, Version::V2_1, v2_1);

/// A meter's resolution is a claim about accuracy, and the JSON number that carries it is the
/// only place the claim lives: `2935.600` says three decimals, `2935.6` says one, and an
/// `f64` cannot tell them apart. So the round trip has to preserve the *token*, not merely
/// the value — this checks the whole decode-and-re-encode path, not just the number type.
#[test]
fn a_number_goes_back_out_spelled_exactly_as_it_arrived() {
    use ocpp_kit::v2_1::{Action, transcode_request};

    let payload = RawValue::from_string(
        r#"{"eventType":"Ended","timestamp":"2024-01-01T00:00:00Z","triggerReason":"StopAuthorized",
            "seqNo":7,"transactionInfo":{"transactionId":"tx-1"},
            "meterValue":[{"timestamp":"2024-01-01T00:00:00Z","sampledValue":[
                {"value":2935.600,"measurand":"Energy.Active.Import.Register",
                 "unitOfMeasure":{"unit":"kWh"}},
                {"value":7,"measurand":"Power.Active.Import"},
                {"value":-0.250,"measurand":"Current.Import"}]}]}"#
            .to_string(),
    )
    .unwrap();

    let produced = transcode_request(
        Action::TransactionEvent,
        &payload,
        &DecodeOptions::pedantic(),
    )
    .expect("decodes");
    let text = produced.get();
    for number in ["2935.600", "7", "-0.250"] {
        assert!(
            text.contains(number),
            "{number} did not survive the round trip: {text}"
        );
    }
}
