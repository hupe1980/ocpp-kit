//! A synthesised corpus of field quirks, replayed the way `ocpp-cli replay` replays a capture.
//!
//! Every `tests/data/captures/<name>.ocppcap` has a `<name>.expect` beside it naming the OCPP
//! version and, for strict and for lenient decoding, either `pass` or the line numbers that
//! must report a problem. The captures are synthetic — made-up vendors, identities and tokens —
//! and each one's header comment names the quirk it reproduces.
//!
//! `verdicts` restates what `replay` in `src/bin/ocpp-cli.rs` does per line: parse the frame,
//! and for a CALL or SEND check the payload against the version's types. When the `cli`
//! feature builds the binary, the binary itself is run as well and its per-line report must
//! agree.

#![cfg(all(feature = "v1_6", feature = "v2_0_1", feature = "v2_1"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ocpp_kit::decode::{DecodeError, DecodeErrorKind, DecodeOptions};
use ocpp_kit::rpc::Frame;
use ocpp_kit::{RawValue, Version, v1_6, v2_0_1, v2_1};

#[derive(Debug)]
struct Expectation {
    version: Version,
    strict: BTreeSet<usize>,
    lenient: BTreeSet<usize>,
}

fn captures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/captures");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/data/captures exists")
        .map(|entry| entry.expect("readable entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ocppcap"))
        .collect();
    files.sort();
    files
}

fn expectation(capture: &Path) -> Expectation {
    let path = capture.with_extension("expect");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error} — every capture needs one", path.display()));
    let (mut version, mut strict, mut lenient) = (None, None, None);
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{}: `{line}` is not `key = value`", path.display()));
        let value = value.trim();
        let lines = || -> BTreeSet<usize> {
            if value == "pass" {
                return BTreeSet::new();
            }
            value
                .split_whitespace()
                .map(|n| n.parse().expect("a line number"))
                .collect()
        };
        match key.trim() {
            "version" => version = Some(value.parse().expect("an OCPP version")),
            "strict" => strict = Some(lines()),
            "lenient" => lenient = Some(lines()),
            other => panic!("{}: unknown key {other:?}", path.display()),
        }
    }
    Expectation {
        version: version.expect("version ="),
        strict: strict.expect("strict ="),
        lenient: lenient.expect("lenient ="),
    }
}

/// `transcode` in `src/bin/ocpp-cli.rs`, for requests.
fn check_request(
    version: Version,
    action: &str,
    payload: &RawValue,
    options: &DecodeOptions,
) -> Result<(), DecodeError> {
    macro_rules! dispatch {
        ($module:ident) => {{
            let action = $module::Action::from_wire(action).ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::UnsupportedAction, "", action.to_owned())
            })?;
            $module::transcode_request(action, payload, options).map(drop)
        }};
    }
    match version {
        Version::V1_6 => dispatch!(v1_6),
        Version::V2_0_1 => dispatch!(v2_0_1),
        _ => dispatch!(v2_1),
    }
}

/// `replay` and `describe_frame` in `src/bin/ocpp-cli.rs`: the 1-based numbers of the lines
/// that report a problem — a frame that does not parse, or a request payload that does not
/// decode — each with the reason.
fn verdicts(text: &str, version: Version, options: &DecodeOptions) -> Vec<(usize, String)> {
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        let line = line
            .strip_prefix('>')
            .or_else(|| line.strip_prefix('<'))
            .unwrap_or(line)
            .trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let problem = match Frame::parse(line, version) {
            Err(error) => Some(error.to_string()),
            Ok(
                Frame::Call {
                    action, payload, ..
                }
                | Frame::Send {
                    action, payload, ..
                },
            ) => check_request(version, &action, &payload, options)
                .err()
                .map(|error| format!("INVALID: {error}")),
            Ok(_) => None,
        };
        if let Some(problem) = problem {
            problems.push((index + 1, problem));
        }
    }
    problems
}

fn lines_of(problems: &[(usize, String)]) -> BTreeSet<usize> {
    problems.iter().map(|(line, _)| *line).collect()
}

#[test]
fn every_capture_replays_to_its_recorded_outcome() {
    let captures = captures();
    assert!(captures.len() >= 8, "the capture corpus went missing");

    let mut mismatches = Vec::new();
    for capture in &captures {
        let expected = expectation(capture);
        let text = std::fs::read_to_string(capture).expect("readable capture");
        for (mode, options, want) in [
            ("strict", DecodeOptions::strict(), &expected.strict),
            ("lenient", DecodeOptions::lenient(), &expected.lenient),
        ] {
            let problems = verdicts(&text, expected.version, &options);
            if &lines_of(&problems) != want {
                mismatches.push(format!(
                    "{} ({mode}): expected problems on {want:?}, got {problems:#?}",
                    capture.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// The corpus exists to show leniency doing something: a capture whose strict and lenient
/// results agree everywhere must be a baseline that passes both, or a frame-level rule
/// (frame parsing has no lenient mode).
#[test]
fn lenient_never_reports_more_than_strict() {
    for capture in captures() {
        let expected = expectation(&capture);
        assert!(
            expected.lenient.is_subset(&expected.strict),
            "{}: lenient must accept everything strict accepts",
            capture.display()
        );
    }
}

/// No real identities: everything that names a station, a token or a vendor is synthetic.
#[test]
fn captures_carry_only_synthetic_identities() {
    for capture in captures() {
        let text = std::fs::read_to_string(&capture).unwrap();
        for needle in [
            "\"idTag\":\"",
            "\"vendorName\":\"",
            "\"chargePointVendor\":\"",
        ] {
            for (at, _) in text.match_indices(needle) {
                let value = &text[at + needle.len()..];
                assert!(
                    value.starts_with("SYNTH") || value.starts_with("Synth"),
                    "{}: a `{needle}` value is not synthetic",
                    capture.display()
                );
            }
        }
    }
}

/// The binary agrees with the restatement above, line for line. Only built with `cli`.
#[test]
fn the_cli_reports_the_same_lines() {
    let Some(binary) = option_env!("CARGO_BIN_EXE_ocpp-cli") else {
        eprintln!("ocpp-cli not built (needs the `cli` feature); skipping");
        return;
    };
    for capture in captures() {
        let expected = expectation(&capture);
        for (lenient, want) in [(false, &expected.strict), (true, &expected.lenient)] {
            let mut command = std::process::Command::new(binary);
            command
                .arg("replay")
                .arg("--version")
                .arg(expected.version.to_string());
            if lenient {
                command.arg("--lenient");
            }
            let output = command.arg(&capture).output().expect("ocpp-cli runs");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let reported: BTreeSet<usize> = stdout
                .lines()
                .filter_map(|line| {
                    let (number, summary) = line.split_once(": ")?;
                    let number: usize = number.trim().parse().ok()?;
                    // A line that checked out reads `CALL <id> <action> [...] valid` or
                    // describes a CALLRESULT / CALLERROR; anything else is a problem.
                    let fine = summary.ends_with(" valid")
                        || summary.starts_with("CALLRESULT ")
                        || summary.starts_with("CALLERROR ")
                        || summary.starts_with("CALLRESULTERROR ");
                    (!fine).then_some(number)
                })
                .collect();
            assert_eq!(
                &reported,
                want,
                "{} (lenient: {lenient}):\n{stdout}",
                capture.display()
            );
        }
    }
}

/// `ocpp-cli replay` exits non-zero when any line reports a problem — a frame that does not
/// parse or a payload that does not decode.
#[test]
fn the_cli_exit_status_follows_the_expectation() {
    let Some(binary) = option_env!("CARGO_BIN_EXE_ocpp-cli") else {
        return;
    };
    for capture in captures() {
        let expected = expectation(&capture);
        for (lenient, want) in [(false, &expected.strict), (true, &expected.lenient)] {
            let mut command = std::process::Command::new(binary);
            command
                .arg("replay")
                .arg("--version")
                .arg(expected.version.to_string());
            if lenient {
                command.arg("--lenient");
            }
            let status = command
                .arg(&capture)
                .output()
                .expect("ocpp-cli runs")
                .status;
            assert_eq!(
                status.success(),
                want.is_empty(),
                "{} (lenient: {lenient})",
                capture.display()
            );
        }
    }
}
