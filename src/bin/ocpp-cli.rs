//! `ocpp-cli` — a small command-line tool for working with OCPP traffic.
//!
//! ```text
//! ocpp-cli actions  [--version 2.1] [--block K]      list the action set
//! ocpp-cli validate [--version 2.1] --action X [--response] [FILE]
//! ocpp-cli frame    [--version 2.1] [FILE]           explain an OCPP-J frame
//! ocpp-cli replay   [--version 2.1] [--lenient] FILE  check a whole capture
//! ocpp-cli csms     [--bind 127.0.0.1:9000]          run a mock CSMS
//! ocpp-cli station  --url ws://… --identity CS-0001  run a mock Charging Station
//! ```
//!
//! `FILE` may be `-`, which reads standard input.

use std::io::Read as _;
use std::process::ExitCode;
use std::time::Duration;

use ocpp_kit::RawValue;
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::message::ActionName;
use ocpp_kit::rpc::{CallError, Frame};
use ocpp_kit::transport::{
    Auth, AuthOutcome, BasicAuthPassword, BoxFuture, Csms, Ctx, Handler, SecurityProfile,
    SessionEvent, Station,
};
use ocpp_kit::{Version, v1_6, v2_0_1, v2_1};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args::new(args);
    let command = args.next_positional().unwrap_or_else(|| "help".to_string());

    let result = match command.as_str() {
        "actions" => actions(&mut args),
        "validate" => validate(&mut args),
        "frame" => frame(&mut args),
        "replay" => replay(&mut args),
        "csms" => run_csms(&mut args),
        "station" => run_station(&mut args),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => Err(format!("unknown command {other:?}; try `ocpp-cli help`")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!(
        "\
ocpp-cli {version} — Open Charge Point Protocol tooling

USAGE:
  ocpp-cli actions  [--version 1.6|2.0.1|2.1] [--block <B>]
  ocpp-cli validate [--version <V>] --action <A> [--response] [--lenient|--pedantic] [FILE]
  ocpp-cli frame    [--version <V>] [FILE]
  ocpp-cli replay   [--version <V>] [--lenient] <FILE>
  ocpp-cli csms     [--bind <ADDR>] [--version <V>]...
  ocpp-cli station  --url <URL> --identity <ID> [--password <PW>] [--version <V>]...

FILE defaults to `-`, which reads standard input.
A capture for `replay` is one OCPP-J frame per line; a line may be prefixed with
`>` (to the peer) or `<` (from the peer), which is ignored.",
        version = ocpp_kit::VERSION
    );
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn actions(args: &mut Args) -> Result<(), String> {
    let version = args.version()?;
    let block = args.value("--block")?;
    args.finish()?;
    println!("{:<36} {:<24} {:<7} DIRECTION", "ACTION", "BLOCK", "KIND");
    let rows = match version {
        Version::V1_6 => describe(v1_6::Action::ALL),
        Version::V2_0_1 => describe(v2_0_1::Action::ALL),
        Version::V2_1 => describe(v2_1::Action::ALL),
        _ => Vec::new(),
    };
    let mut shown = 0;
    for (name, block_id, kind, direction) in rows {
        if block.as_deref().is_some_and(|filter| filter != block_id) {
            continue;
        }
        println!("{name:<36} {block_id:<24} {kind:<7} {direction}");
        shown += 1;
    }
    println!("\n{shown} action(s) in OCPP {version}");
    Ok(())
}

fn describe<A: ActionName>(
    actions: &'static [A],
) -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    actions
        .iter()
        .map(|action| {
            let kind = match action.kind() {
                ocpp_kit::message::MessageKind::Call => "CALL",
                ocpp_kit::message::MessageKind::Send => "SEND",
            };
            let direction = match action.origin() {
                ocpp_kit::message::Origin::ChargingStation => "CS  -> CSMS",
                ocpp_kit::message::Origin::Csms => "CSMS -> CS",
                ocpp_kit::message::Origin::Both => "either",
            };
            (action.as_str(), action.block(), kind, direction)
        })
        .collect()
}

fn validate(args: &mut Args) -> Result<(), String> {
    let version = args.version()?;
    let action = args.value("--action")?.ok_or("--action is required")?;
    let response = args.flag("--response");
    let options = args.decode_options();
    let text = read_input(args)?;
    let payload = RawValue::from_string(text).map_err(|error| format!("not JSON: {error}"))?;

    match transcode(version, &action, response, &payload, &options) {
        Ok(normalized) => {
            let pretty: serde_json::Value =
                serde_json::from_str(normalized.get()).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&pretty).map_err(|e| e.to_string())?
            );
            eprintln!(
                "ok: valid {action}{} for OCPP {version}",
                if response { "Response" } else { "Request" }
            );
            Ok(())
        }
        Err(error) => {
            let call_error = CallError::from(error.clone());
            Err(format!(
                "{error}\n  OCPP error code: {}\n  path: {}",
                call_error.code.as_wire(version),
                if error.path.is_empty() {
                    "<root>"
                } else {
                    &error.path
                }
            ))
        }
    }
}

fn transcode(
    version: Version,
    action: &str,
    response: bool,
    payload: &RawValue,
    options: &DecodeOptions,
) -> Result<Box<RawValue>, ocpp_kit::decode::DecodeError> {
    macro_rules! dispatch {
        ($module:ident) => {{
            let action = $module::Action::from_wire(action).ok_or_else(|| {
                ocpp_kit::decode::DecodeError::new(
                    ocpp_kit::decode::DecodeErrorKind::UnsupportedAction,
                    "",
                    format!("OCPP {version} has no action {action:?}"),
                )
            })?;
            if response {
                $module::transcode_response(action, payload, options)
            } else {
                $module::transcode_request(action, payload, options)
            }
        }};
    }
    match version {
        Version::V1_6 => dispatch!(v1_6),
        Version::V2_0_1 => dispatch!(v2_0_1),
        _ => dispatch!(v2_1),
    }
}

fn frame(args: &mut Args) -> Result<(), String> {
    let version = args.version()?;
    let text = read_input(args)?;
    describe_frame(version, text.trim(), &args.decode_options()).map(|line| println!("{line}"))
}

fn describe_frame(version: Version, text: &str, options: &DecodeOptions) -> Result<String, String> {
    let frame = Frame::parse(text, version).map_err(|error| {
        format!(
            "{error} (would answer {} with id {})",
            error.error_code().as_wire(version),
            error.reply_id()
        )
    })?;
    let summary = match &frame {
        Frame::Call {
            id,
            action,
            payload,
        }
        | Frame::Send {
            id,
            action,
            payload,
        } => {
            // Check the payload against the types, so a capture is validated and not just parsed.
            let direction = match ocpp_kit::actions::origin(version, action) {
                Some(origin) => format!("{origin:?}"),
                None => "unknown action".to_string(),
            };
            let status = match transcode(version, action, false, payload, options) {
                Ok(_) => "valid".to_string(),
                Err(error) => format!("INVALID: {error}"),
            };
            format!(
                "{} {id} {action} [{direction}] {status}",
                frame.message_type()
            )
        }
        Frame::CallResult { id, payload } => {
            format!(
                "{} {id} ({} bytes) — the action is only known from the matching CALL",
                frame.message_type(),
                payload.get().len()
            )
        }
        Frame::CallError { id, error } | Frame::CallResultError { id, error } => {
            format!(
                "{} {id} {} {:?}",
                frame.message_type(),
                error.code.as_wire(version),
                error.description
            )
        }
    };
    Ok(summary)
}

fn replay(args: &mut Args) -> Result<(), String> {
    let version = args.version()?;
    let options = args.decode_options();
    let text = read_input(args)?;

    let mut checked = 0usize;
    let mut failed = 0usize;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        // Captures often mark direction with `>` / `<`; strip it.
        let line = line
            .strip_prefix('>')
            .or_else(|| line.strip_prefix('<'))
            .unwrap_or(line)
            .trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        checked += 1;
        match describe_frame(version, line, &options) {
            Ok(summary) => println!("{:>5}: {summary}", number + 1),
            Err(error) => {
                failed += 1;
                println!("{:>5}: {error}", number + 1);
            }
        }
    }
    println!("\n{checked} frame(s), {failed} problem(s)");
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} frame(s) did not check out"))
    }
}

fn run_csms(args: &mut Args) -> Result<(), String> {
    let bind = args
        .value("--bind")?
        .unwrap_or_else(|| "127.0.0.1:9000".to_string());
    let addr = bind
        .parse()
        .map_err(|_| format!("{bind} is not an address"))?;
    let versions = args.versions()?;
    args.finish()?;

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let csms = Csms::builder()
            .bind(addr)
            .versions(versions)
            .authenticate(|auth: Auth| async move {
                eprintln!(
                    "station {} connecting from {} ({})",
                    auth.identity, auth.remote, auth.profile
                );
                AuthOutcome::Accept
            })
            .handler(Echo)
            .build()
            .map_err(|error| error.to_string())?;

        let mut events = csms.handle().events();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    SessionEvent::Opened {
                        identity, version, ..
                    } => {
                        eprintln!("+ {identity} (OCPP {version})");
                    }
                    SessionEvent::Closed { identity, reason } => {
                        eprintln!("- {identity}: {reason}");
                    }
                    other => eprintln!("  {other:?}"),
                }
            }
        });

        eprintln!("mock CSMS listening on {addr}");
        csms.serve().await.map_err(|error| error.to_string())
    })
}

fn run_station(args: &mut Args) -> Result<(), String> {
    let url = args.value("--url")?.ok_or("--url is required")?;
    let identity = args.value("--identity")?.ok_or("--identity is required")?;
    let password = args.value("--password")?;
    let versions = args.versions()?;
    args.finish()?;
    let preferred = *versions
        .first()
        .ok_or("at least one --version is required")?;

    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let mut builder = Station::builder()
            .identity(&identity)
            .map_err(|error| error.to_string())?
            .url(url)
            .versions(versions)
            .handler(Echo);
        builder = match password {
            Some(password) => builder
                .security_profile(SecurityProfile::BasicAuth)
                .password(
                    BasicAuthPassword::for_version(preferred, &password)
                        .map_err(|error| error.to_string())?,
                ),
            None => builder
                .security_profile(SecurityProfile::BasicAuth)
                .password(BasicAuthPassword::raw(Vec::new())),
        };
        let station = builder.build().map_err(|error| error.to_string())?;
        let handle = station.spawn().map_err(|error| error.to_string())?;

        let mut events = handle.events();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                eprintln!("  {event:?}");
            }
        });

        // A mock station does the one thing every station must do first.
        let boot = handle
            .call(v2_1::BootNotificationRequest::new(
                v2_1::ChargingStation::new("ocpp-cli", "ocpp-kit"),
                v2_1::BootReason::PowerUp,
            ))
            .await
            .map_err(|error| error.to_string())?;
        println!("boot: {:?}, interval {}s", boot.status, boot.interval);

        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    })
}

/// Answers `DataTransfer` and refuses everything else, so the mock peers are useful for
/// checking connectivity without pretending to implement the protocol.
struct Echo;

impl Handler for Echo {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            eprintln!("  <- {} {}", request.action, request.payload.get());
            let _ = ctx;
            Err(CallError::not_supported(&request.action))
        })
    }
}

// ---------------------------------------------------------------------------
// a very small argument parser
// ---------------------------------------------------------------------------

struct Args {
    items: Vec<String>,
}

impl Args {
    fn new(items: Vec<String>) -> Self {
        Self { items }
    }

    fn next_positional(&mut self) -> Option<String> {
        let index = self.items.iter().position(|item| !item.starts_with("--"))?;
        Some(self.items.remove(index))
    }

    fn flag(&mut self, name: &str) -> bool {
        match self.items.iter().position(|item| item == name) {
            Some(index) => {
                self.items.remove(index);
                true
            }
            None => false,
        }
    }

    /// Takes `--name VALUE`.
    ///
    /// `Ok(None)` means the flag was not given at all; a flag with nothing after it is an
    /// error, not an absent one, because silently treating `--action` as unset produces a
    /// message about the wrong thing.
    fn value(&mut self, name: &str) -> Result<Option<String>, String> {
        let Some(index) = self.items.iter().position(|item| item == name) else {
            return Ok(None);
        };
        self.items.remove(index);
        if index < self.items.len() {
            Ok(Some(self.items.remove(index)))
        } else {
            Err(format!("{name} needs a value"))
        }
    }

    /// Fails if any option was not consumed.
    ///
    /// A mistyped flag that is silently ignored is worse here than almost anywhere else: `
    /// --pedntic` would report a payload as valid under rules the user asked not to use, and
    /// nothing in the output would say so.
    fn finish(&self) -> Result<(), String> {
        match self.items.iter().find(|item| item.starts_with('-')) {
            Some(unknown) => Err(format!("unknown option {unknown:?}; try `ocpp-cli help`")),
            None => Ok(()),
        }
    }

    fn values(&mut self, name: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        while let Some(value) = self.value(name)? {
            out.push(value);
        }
        Ok(out)
    }

    fn version(&mut self) -> Result<Version, String> {
        match self.value("--version")? {
            Some(value) => value
                .parse()
                .map_err(|_| format!("unknown OCPP version {value:?}")),
            None => Ok(Version::V2_1),
        }
    }

    fn versions(&mut self) -> Result<Vec<Version>, String> {
        let values = self.values("--version")?;
        if values.is_empty() {
            return Ok(alloc_default());
        }
        values
            .iter()
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| format!("unknown OCPP version {value:?}"))
            })
            .collect()
    }

    fn decode_options(&mut self) -> DecodeOptions {
        if self.flag("--lenient") {
            DecodeOptions::lenient()
        } else if self.flag("--pedantic") {
            DecodeOptions::pedantic()
        } else {
            DecodeOptions::strict()
        }
    }
}

/// The versions a mock peer offers when none is named: everything, newest first.
fn alloc_default() -> Vec<Version> {
    vec![Version::V2_1, Version::V2_0_1, Version::V1_6]
}

fn read_input(args: &mut Args) -> Result<String, String> {
    let path = args.next_positional().unwrap_or_else(|| "-".to_string());
    // Every option a file-taking command understands has been consumed by now, so anything
    // left over is a typo — and a typo in `--pedantic` would otherwise report a payload as
    // valid under rules the caller asked not to use.
    args.finish()?;
    if path == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        Ok(text)
    } else {
        std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))
    }
}
