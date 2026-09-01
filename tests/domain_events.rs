//! The version-agnostic funnel, checked against every action the schemas define.
//!
//! [`csms::events::COVERED_ACTIONS`] is a public list of what the funnel understands, and a
//! hand-maintained list is a claim waiting to drift: a version's schemas gain an action, the
//! code generator emits it, and the list still says it is not covered — or, worse, says it is.
//!
//! So the list is checked rather than trusted. For every action of every version this
//! generates a schema-valid request, runs it through `observe_*`, and asserts that it maps to
//! something other than `DomainEvent::Other` **exactly** when the list says it does.

mod support;

use ocpp_kit::RawValue;
use ocpp_kit::Version;
use ocpp_kit::csms::events::{COVERED_ACTIONS, DomainEvent, Observed};
use ocpp_kit::decode::{DecodeErrorKind, DecodeOptions};
use serde_json::Value;

use support::{Generator, schema_path};

/// A deterministic seed per action, so a failure reproduces exactly.
fn payload(version: Version, action: &str) -> Option<Box<RawValue>> {
    let path = schema_path(version, action, false)?;
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    let mut generator = Generator::new(&schema, 0x9e37_79b9);
    RawValue::from_string(generator.generate().to_string()).ok()
}

/// Runs the check for one version.
fn check(
    version: Version,
    actions: &[&'static str],
    observe: fn(&RawValue, &str) -> Result<Observed, DecodeErrorKind>,
) {
    let covered = COVERED_ACTIONS;
    let mut checked = 0usize;
    let mut station_originated = 0usize;

    for action in actions {
        let Some(payload) = payload(version, action) else {
            continue;
        };
        let observed = match observe(&payload, action) {
            Ok(observed) => observed,
            // Not a message a Charging Station originates, so the funnel has nothing to say
            // about it: `observe_*` only takes `CsRequest`.
            Err(DecodeErrorKind::UnsupportedAction) => continue,
            // Anything else means the generator produced something the types reject, which is
            // a real failure and not something to skip past.
            Err(kind) => panic!("OCPP {version} {action} failed to decode: {kind:?}"),
        };
        station_originated += 1;

        let mapped = !matches!(observed.event, DomainEvent::Other { .. });
        assert_eq!(
            mapped,
            covered.contains(action),
            "OCPP {version} {action}: COVERED_ACTIONS says {}, the funnel says {}",
            covered.contains(action),
            mapped
        );
        // Warnings are deliberately *not* asserted to be empty here, and the reason is worth
        // recording: a schema-valid message can still be one no CSMS can bill. 1.6 types a
        // sampled value as a plain `string` with no numeric constraint, so `"L"` is a
        // conforming reading; 2.x puts no bound on `unitOfMeasure.multiplier`, so `10^109`
        // conforms too. The schema cannot catch either, which is why the funnel says so.
        assert_eq!(observed.version, version);
        checked += 1;
    }

    assert!(checked > 0, "no actions were checked for OCPP {version}");
    println!("OCPP {version}: {station_originated} station-originated action(s) checked");
}

macro_rules! version_suite {
    ($name:ident, $version:expr, $module:ident, $observe:ident) => {
        #[test]
        fn $name() {
            use ocpp_kit::$module::{Action, CsRequest};
            let actions: Vec<&'static str> =
                Action::ALL.iter().map(|action| action.as_str()).collect();
            check($version, &actions, |payload, action| {
                let action = Action::from_wire(action).expect("known action");
                let request = CsRequest::decode(action, payload, &DecodeOptions::pedantic())
                    .map_err(|error| error.kind)?;
                Ok(ocpp_kit::csms::events::$observe(&request))
            });
        }
    };
}

version_suite!(
    the_16_funnel_covers_what_it_claims,
    Version::V1_6,
    v1_6,
    observe_v16
);
version_suite!(
    the_201_funnel_covers_what_it_claims,
    Version::V2_0_1,
    v2_0_1,
    observe_v201
);
version_suite!(
    the_21_funnel_covers_what_it_claims,
    Version::V2_1,
    v2_1,
    observe_v21
);

/// Every name the list carries has to be an action *somewhere*, or it is a typo that the
/// per-version checks above cannot see — they only ever look at names the schemas define.
#[test]
fn the_list_names_no_action_that_does_not_exist() {
    let known: Vec<&'static str> = ocpp_kit::v1_6::Action::ALL
        .iter()
        .map(|action| action.as_str())
        .chain(
            ocpp_kit::v2_1::Action::ALL
                .iter()
                .map(|action| action.as_str()),
        )
        .collect();
    for action in COVERED_ACTIONS {
        assert!(
            known.contains(action),
            "COVERED_ACTIONS names {action:?}, which no version defines"
        );
    }
}
