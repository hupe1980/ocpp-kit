//! A Charging Station that boots, sends a transaction, and answers what the CSMS asks.
//!
//! ```text
//! cargo run --features full --example minimal_station -- ws://127.0.0.1:9000/ocpp CS-0001
//! ```
//!
//! Pair it with `cargo run --features full --example minimal_csms`.

use std::time::Duration;

use ocpp_kit::RawValue;
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::station::transactions::{
    Conditions, RandomTransactionIds, TransactionMachine, TxEvent,
};
use ocpp_kit::transport::{BasicAuthPassword, BoxFuture, Ctx, Handler, SecurityProfile, Station};
use ocpp_kit::types::DateTime;
use ocpp_kit::{Version, v2_1};

/// Answers the requests a CSMS sends to a station.
struct MyStation;

impl Handler for MyStation {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            let action = v2_1::Action::from_wire(&request.action)
                .ok_or_else(|| CallError::not_implemented(&request.action))?;
            println!("<- {action}");
            match v2_1::CsmsRequest::decode(action, &request.payload, ctx.decode_options())? {
                v2_1::CsmsRequest::Reset(_) => {
                    ctx.reply(&v2_1::ResetResponse::new(v2_1::ResetStatus::Accepted))
                }
                v2_1::CsmsRequest::GetVariables(request) => {
                    let results = request
                        .get_variable_data
                        .iter()
                        .map(|item| {
                            v2_1::GetVariableResult::new(
                                v2_1::GetVariableStatus::UnknownVariable,
                                item.component.clone(),
                                item.variable.clone(),
                            )
                        })
                        .collect();
                    ctx.reply(&v2_1::GetVariablesResponse::new(results))
                }
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "ws://127.0.0.1:9000/ocpp".into());
    let identity = args.next().unwrap_or_else(|| "CS-0001".into());

    let station = Station::builder()
        .identity(&identity)?
        .url(url)
        // Part 4 §3.2 recommends offering 2.0.1 alongside 2.1.
        .versions([Version::V2_1, Version::V2_0_1])
        .security_profile(SecurityProfile::BasicAuth)
        .password(BasicAuthPassword::utf8("0123456789abcdef")?)
        .handler(MyStation)
        .build()?;
    let handle = station.spawn()?;

    // B01 — nothing else may be sent until the CSMS accepts the station, and the engine
    // enforces that for us: anything queued now goes out the moment `Accepted` arrives.
    let boot = handle
        .call(v2_1::BootNotificationRequest::new(
            v2_1::ChargingStation::new("Model-1", "ACME").with_serial_number("SN-42"),
            v2_1::BootReason::PowerUp,
        ))
        .await?;
    println!(
        "-> BootNotification: {:?} (heartbeat every {}s)",
        boot.status, boot.interval
    );
    handle.wait_ready().await;

    // A short transaction, driven by the same start/stop rules a real station uses.
    // `with_defaults` is Table 62's OCPP 1.6-compatible configuration: start once the driver
    // is authorized and the cable is in, stop when either goes away.
    let mut transaction = TransactionMachine::with_defaults(Box::new(RandomTransactionIds::new()));
    let plugged_in = Conditions {
        ev_connected: true,
        authorized: true,
        ..Conditions::default()
    };

    for event in transaction.observe(plugged_in, DateTime::now()) {
        send(&handle, &event).await?;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let unplugged = Conditions {
        ev_connected: false,
        ..plugged_in
    };
    for event in transaction.observe(unplugged, DateTime::now()) {
        send(&handle, &event).await?;
    }

    handle.shutdown(Duration::from_secs(5)).await;
    Ok(())
}

/// Turns a machine event into the wire message and sends it.
async fn send(
    handle: &ocpp_kit::transport::Handle,
    event: &TxEvent,
) -> Result<(), Box<dyn std::error::Error>> {
    let (kind, trigger, seq_no, timestamp) = match event {
        TxEvent::Started {
            trigger,
            seq_no,
            timestamp,
            ..
        } => (
            v2_1::TransactionEventEnum::Started,
            *trigger,
            *seq_no,
            *timestamp,
        ),
        TxEvent::Updated {
            trigger,
            seq_no,
            timestamp,
            ..
        } => (
            v2_1::TransactionEventEnum::Updated,
            *trigger,
            *seq_no,
            *timestamp,
        ),
        TxEvent::Ended {
            trigger,
            seq_no,
            timestamp,
            ..
        } => (
            v2_1::TransactionEventEnum::Ended,
            *trigger,
            *seq_no,
            *timestamp,
        ),
        // `TxEvent` is `#[non_exhaustive]`, so new event kinds cannot break this example.
        _ => return Ok(()),
    };
    let request = v2_1::TransactionEventRequest::new(
        kind.clone(),
        timestamp,
        v2_1::TriggerReason::from_wire(trigger),
        seq_no,
        v2_1::Transaction::new(event.transaction_id()),
    );
    handle.call(request).await?;
    println!("-> TransactionEvent({kind:?}) seqNo {seq_no}");
    Ok(())
}
