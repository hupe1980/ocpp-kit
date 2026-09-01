+++
title = "Tokio transport"
description = "Charging Station, CSMS and Local Controller over Tokio: subprotocol negotiation, security profiles 1-3, network configuration slots, failover and reconnect back-off."
weight = 50
+++

`ocpp_kit::transport` (feature `tokio`) turns the engine into three running programs. All
three drive the same [`Engine`](@/docs/engine.md); this layer only moves bytes and time.

## Charging Station

```rust,no_run
use ocpp_kit::transport::{ClientTls, SecurityProfile, Station};
use ocpp_kit::engine::Backoff;
use ocpp_kit::Version;
use std::time::Duration;

# fn build(my_handler: impl ocpp_kit::transport::Handler, store: ocpp_kit::engine::MemStore)
#     -> Result<(), Box<dyn std::error::Error>> {
let station = Station::builder()
    .identity("CS-0001")?
    .url("wss://csms.example.com/ocpp")   // the identity is appended as the last segment
    .versions([Version::V2_1, Version::V2_0_1, Version::V1_6])
    .security_profile(SecurityProfile::TlsClientCertificate)
    .tls(
        ClientTls::builder()
            .root_file("csms-root.pem")?
            .client_certificate("station-chain.pem", "station-key.pem")?
            .build()?,
    )
    .ping_interval(Some(Duration::from_secs(60)))  // WebSocketPingInterval
    .backoff(Backoff::default())                   // Part 4 §5.4
    .store(store)                                  // durable transaction queue
    .handler(my_handler)
    .build()?;

let handle = station.spawn()?;
# Ok(()) }
```

* **Negotiation follows Part 4 §3.1.2 exactly.** The client lists its subprotocols in
  preference order and the server must echo back exactly one. A handshake that completes
  *without* the header is how a CSMS says "I speak none of these" (§3.1.1); it is reported as
  `NegotiationError::NoSubprotocol` rather than silently assumed to be 1.6.
* **Reconnect keeps the engine.** Queue, boot state and heartbeat survive a reconnect, so a
  station does not repeat `BootNotification` on one (Part 4 §5.4). The back-off is the
  specification's own: `wait_minimum + random(random_range)`, doubled up to `repeat_times`
  and then held. The random part is drawn from the operating system, not the clock — a fleet
  is exactly the population whose clocks are synchronised to one NTP source, so clock-derived
  jitter is correlated precisely where §5.4 needs it not to be.
* **A session that ends badly is still just a session.** A failed write or a peer that breaks
  the WebSocket protocol ends the *connection* and starts the back-off; it does not stop the
  station. §5.4 says to reconnect when the connection is lost and does not carve out the ways
  in which it can be lost.
* **Basic auth is version-correct.** 1.6's `AuthorizationKey` is a hexadecimal string whose
  *decoded octets* are the password; 2.x's `BasicAuthPassword` is sent as UTF-8 and must be
  at least 16 characters (A00.FR.205). `BasicAuthPassword::for_version` picks the right one, and an
  identity containing `:` is rejected at build time (A00.FR.204).
* **An unanswered ping ends the session.** Part 4 §5.3 makes the WebSocket ping the
  end-to-end liveness check, and it is only a check if something acts on the silence: a mobile
  network that drops a connection leaves a socket that stays writable for minutes. `Keepalive`
  carries both halves — how often to ping, and how long to wait for the pong — and the timeout
  turns a dead link into a reconnect instead of a hang.
* **Compression is offered** when the `compression` feature is on — optional for a station,
  recommended by Part 4 §3.4. See [the WebSocket layer](@/docs/websocket.md).

### Network configuration slots

A 2.x station does not have *a* CSMS URL. It has numbered configuration slots, an ordered
priority list (`OCPPCommCtrlr.NetworkConfigurationPriority`) and a per-slot attempt budget
(`NetworkProfileConnectionAttempts`). That is what makes migrating to a new CSMS — use case
B10 — a configuration change rather than a reflash.

```rust,no_run
use ocpp_kit::transport::{BasicAuthPassword, NetworkProfile, NetworkProfiles, SecurityProfile, Station};
use std::time::Duration;

# fn build() -> Result<(), Box<dyn std::error::Error>> {
# let password = || BasicAuthPassword::utf8("0123456789abcdef").unwrap();
let station = Station::builder()
    .identity("CS-0001")?
    .network_profiles(
        NetworkProfiles::new([
            NetworkProfile::new(0, "wss://old-csms.example.com/ocpp")
                .security_profile(SecurityProfile::BasicAuth)
                .password(password()),
            NetworkProfile::new(1, "wss://new-csms.example.com/ocpp")
                .security_profile(SecurityProfile::BasicAuth)
                .password(password())
                // Part 4 §4.1.1: overrides the default message timeout while this slot is active.
                .message_timeout(Duration::from_secs(45)),
        ])
        // Try the new CSMS first; fall back to the old one.
        .priority([1, 0])?
        .connection_attempts(3),
    )
    .build()?;
# let _ = station;
# Ok(()) }
```

Every slot is validated at build time, not when the station first fails over to it: a fallback
profile that turns out to be missing its password is worse than no fallback at all. A
successful connection resets the attempt counter, so a flapping link does not walk the list,
and `Event::NetworkProfileSelected` reports the slot in use — what
`OCPPCommCtrlr.ActiveNetworkProfile` shows the CSMS.

## CSMS

```rust,no_run
use ocpp_kit::transport::{Auth, AuthOutcome, Csms, ServerTls};
use ocpp_kit::decode::DecodeOptions;
use ocpp_kit::Version;

# fn build(my_handler: impl ocpp_kit::transport::Handler) -> Result<(), Box<dyn std::error::Error>> {
let csms = Csms::builder()
    .bind("0.0.0.0:9000".parse()?)
    .versions([Version::V2_1, Version::V2_0_1, Version::V1_6])
    .tls(ServerTls::with_client_auth("csms-chain.pem", "csms-key.pem", "station-roots.pem")?)
    .authenticate(|auth: Auth| async move { check(&auth).await })
    // Vendor quirk profiles: forgiving with the fleet that needs it, strict with the rest.
    .decode_options_for(|identity| {
        if identity.as_str().starts_with("LEGACY-") {
            DecodeOptions::lenient()
        } else {
            DecodeOptions::strict()
        }
    })
    .max_connections(50_000)
    // 2.1 Part 4 §3.4 Table 2 makes RFC 7692 *required* for a CSMS; it is on by default.
    .compression(true)
    .handler(my_handler)
    .build()?;
# Ok(()) }
# async fn check(_: &Auth) -> AuthOutcome { AuthOutcome::accept() }
```

The HTTP upgrade is performed by this crate rather than handed to a WebSocket callback,
because OCPP puts requirements on it a synchronous callback cannot meet: authentication is a
database lookup, so it has to be `async`; an unknown identity should be answered **404** and
bad credentials **401**, so an operator can tell a typo from a wrong password (Part 4 §3.1.1);
and a client whose subprotocols the CSMS cannot speak must get a *successful* handshake with
no `Sec-WebSocket-Protocol` header, followed by an immediate close.

### What the authenticator resolved rides the session

An authenticator has already looked the station up to decide whether to admit it. Hanging that
lookup on the session spares every handler a second map keyed on `Identity` — and spares it
the question of what to do when that map misses for a station the authenticator definitely
admitted:

```rust,no_run
use ocpp_kit::transport::{Auth, AuthOutcome, Ctx, SessionContext};

struct ChargePoint { row_id: u64, tenant: String }

async fn authenticate(auth: Auth) -> AuthOutcome {
    match lookup(auth.identity.as_str()).await {
        Some(point) => AuthOutcome::Accept(SessionContext::new(point)),
        None => AuthOutcome::Unknown,
    }
}

fn handle(ctx: &Ctx) {
    // Present for every request on this session, with no lookup and no `expect`.
    let point: &ChargePoint = ctx.session().unwrap();
    let _ = (&point.row_id, &point.tenant);
}
# async fn lookup(_: &str) -> Option<ChargePoint> { None }
```

`Handle::session` reads the same value, so code calling *out* to a station sees the same
resolution as code answering a call from one. `AuthOutcome::accept()` is the short spelling
for admitting a station without storing anything.

The router keeps one session per identity. A station that reconnects after a network partition
supersedes its own zombie session: the old one is given `supersede_drain` to finish its
in-flight answer before it is cut, and it can only ever remove *its own* entry from the router,
never the successor that replaced it. `CsmsHandle::call(&identity, request)` reaches any
connected station from anywhere in your application.

`max_connections` bounds established sessions, which is no help against a peer that opens
sockets and then says nothing — so `max_pending_handshakes` bounds the other half and
`handshake_timeout` makes each of those slots turn over. Per session, handler tasks are bounded
too: a peer that ignores the one-outstanding-`CALL` rule is served, but not at unlimited cost.

## Local Controller

Part 4 chapter 6, implemented literally.

```rust,no_run
use ocpp_kit::rpc::{CallError, Frame};
use ocpp_kit::transport::{Direction, LocalController, RelayDecision};
use ocpp_kit::types::Identity;

# fn build() -> Result<(), Box<dyn std::error::Error>> {
let controller = LocalController::builder()
    .bind("0.0.0.0:9100".parse()?)
    .upstream("wss://csms.example.com/ocpp")
    // The site controller owns smart charging; the CSMS may not address the stations directly.
    .relay(|_: &Identity, direction: Direction, frame: &Frame<'_>| {
        if direction == Direction::Southbound && frame.action() == Some("SetChargingProfile") {
            RelayDecision::Reject(CallError::not_supported("SetChargingProfile"))
        } else {
            RelayDecision::Forward
        }
    })
    .build()?;
# Ok(()) }
```

* **§6.2** — one upstream connection per attached station, under the *station's* identity and
  path, so the CSMS cannot tell the controller is there.
* **§6.3** — losing either leg closes the other, which is what makes the station start
  queueing its transaction messages instead of believing it is still online.
* **§6.4** — the relay never originates a `CALL` of its own: everything it emits is an answer
  quoting the id it is answering, so there is nothing to collide. A `Relay` that *does* want to
  inject calls should use `CounterIds::with_prefix` to keep its ids disjoint from the CSMS's.
* **§3.4** — compression is negotiated separately on each leg, because they are two separate
  connections and a Local Controller must support it on both.
* **§5.3** — and so is the ping/pong check, which the specification notes is point-to-point and
  "makes a difference in an extended network topology with a Local Controller". A dropped
  upstream would otherwise leave the station connected to a controller connected to nothing,
  believing it is online and so *not queueing*. Set it with `keepalive(…)`.
* The OCPP-J text is relayed unchanged, so a [signed message](@/docs/security.md#signed-messages)
  still verifies at the far end.

## Handlers

One trait, one method. Decode with the version's dispatch union and `match`.

```rust,no_run
use ocpp_kit::engine::IncomingRequest;
use ocpp_kit::rpc::CallError;
use ocpp_kit::transport::{BoxFuture, Ctx, Handler};
use ocpp_kit::v2_1;
use ocpp_kit::RawValue;

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
                    // `ctx.handle()` can call the peer back from inside a handler: reverse
                    // calls are queued by the engine, so this cannot deadlock.
                    ctx.reply(&v2_1::ResetResponse::new(v2_1::ResetStatus::Accepted))
                }
                other => Err(CallError::not_supported(other.action().as_str())),
            }
        })
    }
}
```

Returning `Err` sends a `CALLERROR` with the code your `CallError` names — and a decoding
failure converts into the *right* code automatically, because `CallError: From<DecodeError>`.
A `SEND` is dispatched to the same handler and whatever it returns is discarded (FR.07).
