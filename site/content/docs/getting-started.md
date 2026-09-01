+++
title = "Getting started"
description = "Install ocpp-kit and connect a charging station to a CSMS: subprotocol negotiation, security profiles, the boot handshake and the offline queue."
weight = 10
+++

```console
$ cargo add ocpp-kit --features tokio,rustls
```

| You want | Features |
|---|---|
| Just the message types | *(default)* |
| Types and framing, on firmware | `v2_1`, with `--no-default-features` |
| A charging station or a CSMS | `tokio`, plus `rustls` for TLS |
| Domain building blocks | `station` and/or `csms` |
| Everything, including `ocpp-cli` | `full` |

## A charging station

```rust,no_run
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{BasicAuthPassword, BoxFuture, Ctx, Handler, SecurityProfile, Station};
use ocpp_kit::{RawValue, Version, v2_1};

/// Answers what the CSMS asks of the station.
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
            match v2_1::CsmsRequest::decode(action, &request.payload, ctx.decode_options())? {
                v2_1::CsmsRequest::Reset(_) => {
                    ctx.reply(&v2_1::ResetResponse::new(v2_1::ResetStatus::Accepted))
                }
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let handle = Station::builder()
    .identity("CS-0001")?
    .url("wss://csms.example.com/ocpp")
    .versions([Version::V2_1, Version::V2_0_1])
    .security_profile(SecurityProfile::TlsBasicAuth)
    .password(BasicAuthPassword::utf8("a-sixteen-plus-character-secret")?)
    .tls(ocpp_kit::transport::ClientTls::with_root_file("csms-root.pem")?)
    .handler(MyStation)
    .build()?
    .spawn()?;

let boot = handle
    .call(v2_1::BootNotificationRequest::new(
        v2_1::ChargingStation::new("Model-1", "ACME"),
        v2_1::BootReason::PowerUp,
    ))
    .await?;
println!("{:?}, heartbeat every {}s", boot.status, boot.interval);
# Ok(()) }
```

Three things happen without asking:

* the URL becomes `wss://csms.example.com/ocpp/CS-0001` — the identity is the last path segment
  (Part 4 §3.1);
* anything sent before `BootNotification` is accepted is **queued**, because B02.FR.02 forbids
  sending it, and released the moment the CSMS says `Accepted`;
* reconnects follow Part 4 §5.4 and keep the queue and the boot state, so a reconnect does not
  repeat `BootNotification`.

## A CSMS

```rust,no_run
use ocpp_kit::transport::{Auth, AuthOutcome, Csms};
use ocpp_kit::{Version, v2_1};

# async fn run(my_handler: impl ocpp_kit::transport::Handler) -> Result<(), Box<dyn std::error::Error>> {
let csms = Csms::builder()
    .bind("0.0.0.0:9000".parse()?)
    .versions([Version::V2_1, Version::V2_0_1, Version::V1_6])
    .authenticate(|auth: Auth| async move {
        match lookup(&auth) {
            Some(true) => AuthOutcome::accept(),
            Some(false) => AuthOutcome::Reject,   // HTTP 401
            None => AuthOutcome::Unknown,         // HTTP 404
        }
    })
    .handler(my_handler)
    .build()?;

let handle = csms.handle();
tokio::spawn(async move { csms.serve().await });

// Reach any connected station by identity.
let identity = "CS-0001".parse()?;
let response = handle.call(&identity, v2_1::ResetRequest::new(v2_1::ResetEnum::Immediate)).await?;
# Ok(()) }
# fn lookup(_: &Auth) -> Option<bool> { Some(true) }
```

Compare the password with `BasicAuthPassword::verify`, which is constant-time. The username
needs no check: A00.FR.207 makes that the CSMS's duty, so a request whose Basic username is not
the identity in the URL is answered 401 before your closure runs.

`authenticate` is required — `Csms::builder().bind(addr).build()` is an error. To accept
everyone, say `authenticate(AcceptEveryStation)`.

## Running the pair

```console
$ cargo run --features full --example minimal_csms
$ cargo run --features full --example minimal_station
```

The station boots, runs a short transaction driven by the real start/stop rules, and
disconnects. The CSMS prints the version-agnostic view of each message and records the
transaction in an idempotent ledger.
