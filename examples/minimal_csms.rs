//! A CSMS that accepts stations, keeps an idempotent transaction ledger, and reduces every
//! version to one set of domain events.
//!
//! ```text
//! cargo run --features full --example minimal_csms
//! ```

use std::sync::Arc;
use std::sync::Mutex;

use ocpp_kit::RawValue;
use ocpp_kit::csms::events::{Observed, observe_v16, observe_v21, observe_v201};
use ocpp_kit::csms::ledger::{Ingested, Ledger};
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{Auth, AuthOutcome, BoxFuture, Csms, Ctx, Handler, SessionEvent};
use ocpp_kit::types::DateTime;
use ocpp_kit::{Version, v1_6, v2_0_1, v2_1};

#[derive(Default)]
struct MyCsms {
    ledger: Mutex<Ledger>,
}

impl Handler for MyCsms {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            // One code path for 1.6, 2.0.1 and 2.1: decode with the version's own types, then
            // look at the version-neutral view of what happened.
            let (observed, reply) = match ctx.version() {
                Version::V1_6 => {
                    let action = v1_6::Action::from_wire(&request.action)
                        .ok_or_else(|| CallError::not_implemented(&request.action))?;
                    let typed =
                        v1_6::CsRequest::decode(action, &request.payload, ctx.decode_options())?;
                    (observe_v16(&typed), answer_v16(&ctx, &typed))
                }
                Version::V2_0_1 => {
                    let action = v2_0_1::Action::from_wire(&request.action)
                        .ok_or_else(|| CallError::not_implemented(&request.action))?;
                    let typed =
                        v2_0_1::CsRequest::decode(action, &request.payload, ctx.decode_options())?;
                    (observe_v201(&typed), answer_v201(&ctx, &typed))
                }
                _ => {
                    let action = v2_1::Action::from_wire(&request.action)
                        .ok_or_else(|| CallError::not_implemented(&request.action))?;
                    let typed =
                        v2_1::CsRequest::decode(action, &request.payload, ctx.decode_options())?;
                    (observe_v21(&typed), answer_v21(&ctx, &typed))
                }
            };

            self.record(ctx.identity(), &observed);
            reply
        })
    }
}

impl MyCsms {
    fn record(&self, identity: &ocpp_kit::types::Identity, observed: &Observed) {
        println!("[{identity}] {:?}", observed.event);
        // Rare, and worth a log line every time: each one is a station saying something
        // malformed about a value that decides money, and each one otherwise fails silently.
        for warning in &observed.warnings {
            println!("  !! {warning}");
        }
        let Some(event) = ocpp_kit::csms::events::to_ledger_event(identity, observed) else {
            return;
        };
        let mut ledger = self.ledger.lock().expect("ledger");
        // 1.6 has no seqNo, so let the ledger assign one there.
        let outcome = if observed.version == Version::V1_6 {
            ledger.ingest_unsequenced(&event)
        } else {
            ledger.ingest(&event)
        };
        match outcome {
            // A station that retried after a timeout must not be billed twice.
            Ingested::Duplicate => println!("  (already recorded — not billed again)"),
            Ingested::AppliedWithGap { missing } => println!("  (missing seqNo {missing:?})"),
            other => println!("  ({other:?})"),
        }
        let Some(record) = ledger.transaction(identity, &event.transaction_id) else {
            return;
        };
        // The station's own registers, subtracted exactly: two decimals in, one decimal out,
        // no f64 anywhere. That makes it a reliable *operational* figure — it is not
        // automatically a billable one. The registers are whatever the meter chose to report,
        // and in the OCA's own example message `meterStop` is the meter's lifetime total
        // while the signed record beside it reports the session.
        if let Some(energy) = record.energy_wh() {
            println!("  ({energy} Wh between the reported registers)");
        }
        // Where calibration law applies, this is the value a customer may be billed for. It
        // is carried through untouched; verifying it needs the meter's public key from
        // somewhere other than this socket.
        for signed in &record.signed {
            let context = signed.context.as_deref().unwrap_or("no context");
            let method = signed
                .value
                .encoding_method
                .as_deref()
                .unwrap_or("unspecified");
            println!(
                "  (signed {method} record, {context}, {} bytes)",
                signed.value.signed_meter_data.len()
            );
        }
    }
}

fn answer_v21(ctx: &Ctx, request: &v2_1::CsRequest) -> Result<Box<RawValue>, CallError> {
    match request {
        v2_1::CsRequest::BootNotification(_) => ctx.reply(&v2_1::BootNotificationResponse::new(
            DateTime::now(),
            300,
            v2_1::RegistrationStatus::Accepted,
        )),
        v2_1::CsRequest::Heartbeat(_) => ctx.reply(&v2_1::HeartbeatResponse::new(DateTime::now())),
        v2_1::CsRequest::StatusNotification(_) => {
            ctx.reply(&v2_1::StatusNotificationResponse::new())
        }
        v2_1::CsRequest::TransactionEvent(_) => ctx.reply(&v2_1::TransactionEventResponse::new()),
        v2_1::CsRequest::Authorize(_) => ctx.reply(&v2_1::AuthorizeResponse::new(
            v2_1::IdTokenInfo::new(v2_1::AuthorizationStatus::Accepted),
        )),
        other => Err(CallError::not_supported(other.action().as_str())),
    }
}

fn answer_v201(ctx: &Ctx, request: &v2_0_1::CsRequest) -> Result<Box<RawValue>, CallError> {
    match request {
        v2_0_1::CsRequest::BootNotification(_) => {
            ctx.reply(&v2_0_1::BootNotificationResponse::new(
                DateTime::now(),
                300,
                v2_0_1::RegistrationStatus::Accepted,
            ))
        }
        v2_0_1::CsRequest::Heartbeat(_) => {
            ctx.reply(&v2_0_1::HeartbeatResponse::new(DateTime::now()))
        }
        v2_0_1::CsRequest::TransactionEvent(_) => {
            ctx.reply(&v2_0_1::TransactionEventResponse::new())
        }
        other => Err(CallError::not_supported(other.action().as_str())),
    }
}

fn answer_v16(ctx: &Ctx, request: &v1_6::CsRequest) -> Result<Box<RawValue>, CallError> {
    match request {
        v1_6::CsRequest::BootNotification(_) => ctx.reply(&v1_6::BootNotificationResponse::new(
            v1_6::RegistrationStatus::Accepted,
            DateTime::now(),
            300,
        )),
        v1_6::CsRequest::Heartbeat(_) => ctx.reply(&v1_6::HeartbeatResponse::new(DateTime::now())),
        v1_6::CsRequest::StatusNotification(_) => {
            ctx.reply(&v1_6::StatusNotificationResponse::new())
        }
        other => Err(CallError::not_supported(other.action().as_str())),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let handler = Arc::new(MyCsms::default());
    let csms = Csms::builder()
        .bind("127.0.0.1:9000".parse()?)
        .versions([Version::V2_1, Version::V2_0_1, Version::V1_6])
        .authenticate(|auth: Auth| async move {
            // A real CSMS looks the identity up and compares the password in constant time;
            // `BasicAuthPassword::verify` does the comparison for you.
            println!(
                "handshake from {} as {} ({})",
                auth.remote, auth.identity, auth.profile
            );
            AuthOutcome::accept()
        })
        .handler(SharedHandler(handler.clone()))
        .build()?;

    let mut events = csms.handle().events();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                SessionEvent::Opened {
                    identity, version, ..
                } => {
                    println!("+ {identity} speaks OCPP {version}");
                }
                SessionEvent::Closed { identity, reason } => println!("- {identity}: {reason}"),
                other => println!("  {other:?}"),
            }
        }
    });

    println!("CSMS listening on 127.0.0.1:9000");
    csms.serve().await?;
    Ok(())
}

/// Lets `main` keep a reference to the handler it installed.
struct SharedHandler(Arc<MyCsms>);

impl Handler for SharedHandler {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        self.0.on_request(ctx, request)
    }
}
