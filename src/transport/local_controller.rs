//! The Local Controller: OCPP 2.x Part 4 chapter 6, implemented literally.
//!
//! A Local Controller sits between a group of Charging Stations and the CSMS. What makes it
//! more than a TCP proxy is a short list of requirements that this module implements:
//!
//! * **§6.2 — one upstream connection per station, under the station's own identity and
//!   path.** The CSMS sees exactly what it would see without the Local Controller, so no
//!   CSMS-side support is needed.
//! * **§6.3 — close propagation in both directions.** Losing the CSMS leg closes the matching
//!   station leg, which is what makes the station start queueing its transaction messages
//!   instead of believing it is still online; losing the station leg closes the CSMS leg.
//! * **§6.4 — the Local Controller's own calls must not collide** with the CSMS's message
//!   ids. This relay never *originates* a `CALL`: everything it emits southbound is an answer
//!   to a station's own `CALL`, quoting that station's id, so there is nothing to collide.
//!   A [`Relay`] that wants to inject calls of its own must generate ids that cannot clash —
//!   [`CounterIds::with_prefix`](crate::types::CounterIds::with_prefix) is there for exactly
//!   that.
//! * **Message-level relaying.** Messages are parsed only far enough to route and inspect
//!   them, and the OCPP-J text is forwarded unchanged — so a signed message (Part 4 chapter 7)
//!   still verifies at the far end. The two legs are separate WebSocket connections and
//!   negotiate compression independently, which §3.4 requires a Local Controller to support.
//! * **§6.5 — separate TLS roles**: a server certificate southbound, a client certificate
//!   northbound, both belonging to the Local Controller itself.
//! * **§5.3 — a liveness check on each leg.** The ping is point-to-point, and a Local
//!   Controller is two connections; see [`LocalControllerBuilder::keepalive`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::Uri;
use tokio::net::{TcpListener, TcpStream};

use crate::rpc::{CallError, ErrorCode, Frame};
use crate::types::Identity;
use crate::version::{Subprotocol, Version};

use super::TransportError;
use super::connection::Keepalive;
use super::stream::MaybeTls;
use super::ws::WsError;
use super::ws::handshake::{
    ClientRequest, client_handshake, read_request, write_accept, write_refusal,
};
use super::ws::{
    CloseCode, CloseFrame, Config as WsConfig, Message, Role as WsRole, WebSocket, WsCodec,
};

/// Which way a frame is travelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Charging Station → CSMS.
    Northbound,
    /// CSMS → Charging Station.
    Southbound,
}

/// What the Local Controller should do with a frame.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RelayDecision {
    /// Forward the original bytes untouched. Signed messages depend on this.
    Forward,
    /// Do not forward; answer the sender with this `CALLERROR` instead.
    ///
    /// Ignored for frames that must not be answered (`SEND`, and responses).
    Reject(CallError),
    /// Drop the frame silently. For `SEND` telemetry the Local Controller absorbs.
    Drop,
    /// Forward this text instead of the original.
    ///
    /// Rewriting a signed message invalidates its signature, so only do this to frames you
    /// know are unsigned.
    Replace(String),
}

/// Inspects — and optionally intercepts — relayed frames.
///
/// This is where local load management lives: a Local Controller in the Smart Charging
/// K-block role answers `SetChargingProfile` itself, or narrows a limit before passing it on.
pub trait Relay: Send + Sync + 'static {
    /// Called for every frame, in both directions.
    fn inspect(
        &self,
        identity: &Identity,
        direction: Direction,
        frame: &Frame<'_>,
    ) -> RelayDecision;
}

/// Forwards everything unchanged.
pub struct PassThrough;

impl Relay for PassThrough {
    fn inspect(&self, _: &Identity, _: Direction, _: &Frame<'_>) -> RelayDecision {
        RelayDecision::Forward
    }
}

impl<F> Relay for F
where
    F: Fn(&Identity, Direction, &Frame<'_>) -> RelayDecision + Send + Sync + 'static,
{
    fn inspect(
        &self,
        identity: &Identity,
        direction: Direction,
        frame: &Frame<'_>,
    ) -> RelayDecision {
        self(identity, direction, frame)
    }
}

/// How the Local Controller authenticates northbound.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub enum UpstreamCredentials {
    /// Forward the station's own `Authorization` header, so the CSMS keeps its per-station
    /// credentials. The default.
    #[default]
    PassThrough,
    /// Send nothing; the northbound leg relies on TLS client certificates (profile 3).
    None,
}

/// Builds a [`LocalController`].
pub struct LocalControllerBuilder {
    bind: Option<SocketAddr>,
    upstream: Option<String>,
    #[cfg(feature = "rustls")]
    server_tls: Option<super::tls::ServerTls>,
    #[cfg(feature = "rustls")]
    client_tls: Option<super::tls::ClientTls>,
    relay: Arc<dyn Relay>,
    credentials: UpstreamCredentials,
    versions: Option<Subprotocol>,
    ws_config: WsConfig,
    #[cfg(feature = "compression")]
    compression: bool,
    connect_timeout: Duration,
    keepalive: Keepalive,
}

impl Default for LocalControllerBuilder {
    fn default() -> Self {
        Self {
            bind: None,
            upstream: None,
            #[cfg(feature = "rustls")]
            server_tls: None,
            #[cfg(feature = "rustls")]
            client_tls: None,
            relay: Arc::new(PassThrough),
            credentials: UpstreamCredentials::default(),
            versions: None,
            ws_config: WsConfig::default(),
            // 2.1 Part 4 §3.4 Table 2: a Local Controller is *required* to support it, on the
            // southbound side as a server and on the northbound side as a client.
            #[cfg(feature = "compression")]
            compression: true,
            connect_timeout: Duration::from_secs(20),
            keepalive: Keepalive::default(),
        }
    }
}

impl LocalControllerBuilder {
    /// Where attached Charging Stations connect.
    #[must_use]
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = Some(addr);
        self
    }

    /// The CSMS endpoint, without the identity — the same form a station would use.
    #[must_use]
    pub fn upstream(mut self, url: impl Into<String>) -> Self {
        self.upstream = Some(url.into());
        self
    }

    /// The TLS server identity presented to Charging Stations (§6.5: a "CSMS" certificate
    /// belonging to the Local Controller).
    #[cfg(feature = "rustls")]
    #[must_use]
    pub fn server_tls(mut self, tls: super::tls::ServerTls) -> Self {
        self.server_tls = Some(tls);
        self
    }

    /// The TLS client identity presented to the CSMS (§6.5: a "Charging Station" certificate
    /// belonging to the Local Controller).
    #[cfg(feature = "rustls")]
    #[must_use]
    pub fn client_tls(mut self, tls: super::tls::ClientTls) -> Self {
        self.client_tls = Some(tls);
        self
    }

    /// Whether to negotiate RFC 7692 `permessage-deflate` on both legs.
    ///
    /// On by default with the `compression` feature, because 2.1 Part 4 §3.4 Table 2 makes it
    /// **required** for a Local Controller. The two legs are separate WebSocket connections,
    /// so each negotiates on its own — a station that cannot compress still reaches a CSMS
    /// that can.
    #[cfg(feature = "compression")]
    #[must_use]
    pub fn compression(mut self, enabled: bool) -> Self {
        self.compression = enabled;
        self
    }

    /// The interception hook.
    #[must_use]
    pub fn relay(mut self, relay: impl Relay) -> Self {
        self.relay = Arc::new(relay);
        self
    }

    /// How to authenticate northbound.
    #[must_use]
    pub fn upstream_credentials(mut self, credentials: UpstreamCredentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// The WebSocket keepalive, applied to *both* legs.
    ///
    /// Part 4 §5.3's liveness check is point-to-point, and a Local Controller is two
    /// independent connections. Without it a dropped upstream leaves the station connected to
    /// a controller connected to nothing: it believes it is online, so it does not queue.
    #[must_use]
    pub fn keepalive(mut self, keepalive: Keepalive) -> Self {
        self.keepalive = keepalive;
        self
    }

    /// Restricts the versions offered northbound.
    ///
    /// By default the Local Controller offers exactly what the station offered, so the CSMS
    /// makes the same choice it would have made without it.
    #[must_use]
    pub fn versions(mut self, versions: impl IntoIterator<Item = Version>) -> Self {
        self.versions = Some(Subprotocol::new(versions));
        self
    }

    /// Builds the Local Controller.
    pub fn build(self) -> Result<LocalController, TransportError> {
        let bind = self
            .bind
            .ok_or_else(|| TransportError::Configuration("bind address is required".into()))?;
        let upstream = self
            .upstream
            .ok_or_else(|| TransportError::Configuration("upstream URL is required".into()))?;
        Ok(LocalController {
            bind,
            upstream,
            #[cfg(feature = "rustls")]
            server_tls: self.server_tls,
            #[cfg(feature = "rustls")]
            client_tls: self.client_tls,
            relay: self.relay,
            credentials: self.credentials,
            versions: self.versions,
            ws_config: self.ws_config,
            #[cfg(feature = "compression")]
            compression: self.compression,
            connect_timeout: self.connect_timeout,
            keepalive: self.keepalive,
        })
    }
}

/// An OCPP Local Controller.
pub struct LocalController {
    bind: SocketAddr,
    upstream: String,
    #[cfg(feature = "rustls")]
    server_tls: Option<super::tls::ServerTls>,
    #[cfg(feature = "rustls")]
    client_tls: Option<super::tls::ClientTls>,
    relay: Arc<dyn Relay>,
    credentials: UpstreamCredentials,
    versions: Option<Subprotocol>,
    ws_config: WsConfig,
    #[cfg(feature = "compression")]
    compression: bool,
    connect_timeout: Duration,
    keepalive: Keepalive,
}

impl LocalController {
    /// Starts building a Local Controller.
    #[must_use]
    pub fn builder() -> LocalControllerBuilder {
        LocalControllerBuilder::default()
    }

    /// Serves until the listener fails.
    pub async fn serve(self) -> Result<(), TransportError> {
        let listener = TcpListener::bind(self.bind).await?;
        self.serve_on(listener).await
    }

    /// Serves on a listener the caller already bound.
    pub async fn serve_on(self, listener: TcpListener) -> Result<(), TransportError> {
        let controller = Arc::new(self);
        loop {
            let (stream, remote) = listener.accept().await?;
            let controller = controller.clone();
            tokio::spawn(async move {
                if let Err(error) = controller.attach(stream, remote).await {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(%remote, %error, "station connection ended");
                    let _ = (error, remote);
                }
            });
        }
    }

    /// Accepts one station, opens the matching CSMS connection, and relays until either
    /// side goes away.
    async fn attach(
        self: Arc<Self>,
        stream: TcpStream,
        remote: SocketAddr,
    ) -> Result<(), TransportError> {
        stream.set_nodelay(true)?;
        let mut south = self.wrap_server(stream).await?;
        let request = read_request(&mut south).await?;
        // RFC 6455 §4.2.1 / §4.4 — the same courtesy the CSMS extends: say what was wrong
        // instead of resetting the connection.
        if let Some(defect) = request.upgrade_defect() {
            let (status, reason) = defect.status();
            let _ = write_refusal(&mut south, status, reason).await;
            return Ok(());
        }
        let Some(identity) = request.identity() else {
            let _ = write_refusal(&mut south, 400, "Bad Request").await;
            return Ok(());
        };

        let offered = Subprotocol::new(request.subprotocols());
        let northbound_offer = match &self.versions {
            Some(limit) => Subprotocol::new(
                offered
                    .offered()
                    .iter()
                    .copied()
                    .filter(|v| limit.offered().contains(v)),
            ),
            None => offered.clone(),
        };
        let authorization = request.header("authorization").map(ToOwned::to_owned);

        // §6.2 — the upstream connection uses the *station's* identity and path, so the CSMS
        // cannot tell the Local Controller is there.
        let north = tokio::time::timeout(
            self.connect_timeout,
            self.connect_upstream(&identity, &northbound_offer, authorization.as_deref()),
        )
        .await
        .map_err(|_| TransportError::Configuration("upstream connect timed out".into()))?;

        let (north, version) = match north {
            Ok(pair) => pair,
            Err(error) => {
                // The station must learn immediately that there is no path to the CSMS, so
                // that it starts queueing rather than waiting on a dead connection.
                let _ = write_refusal(&mut south, 502, "Bad Gateway").await;
                return Err(error);
            }
        };

        // The southbound leg negotiates compression on its own: the two legs are separate
        // WebSocket connections, and §3.4 requires the Local Controller to support it on both.
        #[allow(unused_mut)]
        let mut codec = WsCodec::new(WsRole::Server, self.ws_config);
        #[allow(unused_mut)]
        let mut extensions: Option<String> = None;
        #[cfg(feature = "compression")]
        if self.compression {
            if let Some((params, response)) =
                super::ws::deflate::accept_offer(request.extensions().as_deref())
            {
                codec = codec.with_deflate(params);
                extensions = Some(response);
            }
        }

        write_accept(
            &mut south,
            request.websocket_key().unwrap_or_default(),
            Some(version.subprotocol()),
            extensions.as_deref(),
        )
        .await?;
        let south = super::ws::attach(south, codec, &request.head.rest);

        #[cfg(feature = "tracing")]
        tracing::info!(identity = %identity, %remote, %version, "station attached");
        let _ = remote;

        self.pump(identity, version, south, north).await
    }

    async fn wrap_server(&self, stream: TcpStream) -> Result<MaybeTls, TransportError> {
        #[cfg(feature = "rustls")]
        if let Some(tls) = &self.server_tls {
            return Ok(MaybeTls::ServerTls(Box::new(
                tls.acceptor().accept(stream).await?,
            )));
        }
        Ok(MaybeTls::Plain(stream))
    }

    async fn connect_upstream(
        &self,
        identity: &Identity,
        offer: &Subprotocol,
        authorization: Option<&str>,
    ) -> Result<(WebSocket<MaybeTls>, Version), TransportError> {
        let uri: Uri = self
            .upstream
            .parse()
            .map_err(|_| TransportError::Url(self.upstream.clone()))?;
        let secure = matches!(uri.scheme_str(), Some("wss" | "https"));
        let host = uri
            .host()
            .ok_or_else(|| TransportError::Url(self.upstream.clone()))?
            .to_owned();
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });

        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        tcp.set_nodelay(true)?;
        let mut stream = if secure {
            #[cfg(feature = "rustls")]
            {
                let tls = self.client_tls.as_ref().ok_or_else(|| {
                    TransportError::Configuration("a wss upstream needs client_tls".into())
                })?;
                let name = super::tls::ClientTls::server_name(&host)
                    .map_err(|error| TransportError::Configuration(error.to_string()))?;
                MaybeTls::ClientTls(Box::new(tls.connector().connect(name, tcp).await?))
            }
            #[cfg(not(feature = "rustls"))]
            return Err(TransportError::Configuration(
                "TLS needs the `rustls` feature".into(),
            ));
        } else {
            MaybeTls::Plain(tcp)
        };

        // §6.2 — the same identity and path the station used, so the CSMS cannot tell.
        let path = format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            super::ws::handshake::percent_encode(identity.as_str())
        );
        let authority = if uri.port().is_some() {
            format!("{host}:{port}")
        } else {
            host.clone()
        };
        let authorization = match (&self.credentials, authorization) {
            (UpstreamCredentials::PassThrough, value) => value,
            (UpstreamCredentials::None, _) => None,
        };
        #[allow(unused_mut)]
        let mut extensions: Option<String> = None;
        #[cfg(feature = "compression")]
        if self.compression {
            extensions = Some(super::ws::deflate::DeflateParams::default().client_offer());
        }

        let handshake = client_handshake(
            &mut stream,
            &ClientRequest {
                host: &authority,
                path: &path,
                subprotocols: &offer.header_value(),
                authorization,
                extensions: extensions.as_deref(),
            },
        )
        .await?;
        let version = offer.accept(handshake.subprotocol.as_deref())?;

        #[allow(unused_mut)]
        let mut codec = WsCodec::new(WsRole::Client, self.ws_config);
        #[cfg(feature = "compression")]
        if extensions.is_some() {
            if let Ok(Some(params)) =
                super::ws::deflate::accept_response(handshake.extensions.as_deref())
            {
                codec = codec.with_deflate(params);
            }
        }
        Ok((super::ws::attach(stream, codec, &handshake.rest), version))
    }

    /// Relays frames until either leg closes, then closes the other (§6.3).
    async fn pump(
        &self,
        identity: Identity,
        version: Version,
        south: WebSocket<MaybeTls>,
        north: WebSocket<MaybeTls>,
    ) -> Result<(), TransportError> {
        let (mut south_tx, mut south_rx) = south.split();
        let (mut north_tx, mut north_rx) = north.split();

        // Part 4 §5.3 — the two legs are independent connections, so each needs its own
        // liveness check.
        let keepalive = self.keepalive;
        let mut ping = keepalive.interval.map(|interval| {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer
        });
        let mut south_pong_due: Option<tokio::time::Instant> = None;
        let mut north_pong_due: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                message = south_rx.next() => {
                    let carry_on = self
                        .relay_one(
                            &identity,
                            version,
                            Direction::Northbound,
                            message,
                            &mut south_tx,
                            &mut north_tx,
                            &mut south_pong_due,
                        )
                        .await;
                    if !carry_on {
                        break;
                    }
                }

                message = north_rx.next() => {
                    let carry_on = self
                        .relay_one(
                            &identity,
                            version,
                            Direction::Southbound,
                            message,
                            &mut north_tx,
                            &mut south_tx,
                            &mut north_pong_due,
                        )
                        .await;
                    if !carry_on {
                        break;
                    }
                }

                _ = async { ping.as_mut().expect("checked").tick().await }, if ping.is_some() => {
                    let deadline = tokio::time::Instant::now() + keepalive.timeout;
                    south_pong_due.get_or_insert(deadline);
                    north_pong_due.get_or_insert(deadline);
                    if south_tx.send(Message::Ping(Vec::new())).await.is_err()
                        || north_tx.send(Message::Ping(Vec::new())).await.is_err()
                    {
                        break;
                    }
                }

                () = async { tokio::time::sleep_until(south_pong_due.expect("checked")).await },
                    if south_pong_due.is_some() =>
                {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(identity = %identity, "the station stopped answering pings");
                    break;
                }

                () = async { tokio::time::sleep_until(north_pong_due.expect("checked")).await },
                    if north_pong_due.is_some() =>
                {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(identity = %identity, "the CSMS stopped answering pings");
                    break;
                }
            }
        }

        // §6.3 — one leg closing closes the other, so the station starts queueing.
        let _ = south_tx
            .send(Message::Close(Some(CloseFrame::new(
                CloseCode::GOING_AWAY,
                "",
            ))))
            .await;
        let _ = north_tx
            .send(Message::Close(Some(CloseFrame::new(
                CloseCode::GOING_AWAY,
                "",
            ))))
            .await;
        let _ = south_tx.close().await;
        let _ = north_tx.close().await;
        #[cfg(feature = "tracing")]
        tracing::info!(identity = %identity, "station detached");
        Ok(())
    }

    /// Relays one message from `from` to `to`. Returns `false` when this leg has ended.
    ///
    /// One function with the sinks swapped, not two mirrored copies: §6.3 and §5.3 both apply
    /// to each leg, and mirrored copies are how one ends up missing a rule the other has.
    #[allow(clippy::too_many_arguments)]
    async fn relay_one<From, To>(
        &self,
        identity: &Identity,
        version: Version,
        direction: Direction,
        message: Option<Result<Message, WsError>>,
        from: &mut From,
        to: &mut To,
        pong_due: &mut Option<tokio::time::Instant>,
    ) -> bool
    where
        From: futures_util::Sink<Message, Error = WsError> + Unpin,
        To: futures_util::Sink<Message, Error = WsError> + Unpin,
    {
        match message {
            Some(Ok(Message::Text(text))) => match self.decide(identity, direction, &text, version)
            {
                Relayed::Forward(text) => to.send(Message::Text(text)).await.is_ok(),
                Relayed::Answer(text) => from.send(Message::Text(text)).await.is_ok(),
                Relayed::Drop => true,
            },
            // RFC 6455 §5.5.2 — answered here rather than forwarded: the two legs are separate
            // connections, and §5.3 says the check is point-to-point.
            Some(Ok(Message::Ping(payload))) => from.send(Message::Pong(payload)).await.is_ok(),
            Some(Ok(Message::Pong(_))) => {
                // §5.5.3 does not require the payload to be echoed, so any pong is proof of
                // life.
                *pong_due = None;
                true
            }
            Some(Ok(Message::Close(frame))) => {
                let _ = to.send(Message::Close(frame)).await;
                false
            }
            // Part 4 §4.1: OCPP-J is text-only; a binary frame is not fatal.
            Some(Ok(Message::Binary(_))) => true,
            // §6.3 — whatever ends one leg must still close the other, so this returns
            // rather than propagating and the close handling below runs.
            Some(Err(error)) => {
                #[cfg(feature = "tracing")]
                tracing::debug!(identity = %identity, %error, "relay leg failed");
                let _ = error;
                false
            }
            None => false,
        }
    }

    fn decide(
        &self,
        identity: &Identity,
        direction: Direction,
        text: &str,
        version: Version,
    ) -> Relayed {
        // Parse only enough to route and inspect. On anything unparseable the Local
        // Controller stays out of the way: the endpoints get to apply their own rules.
        let Ok(frame) = Frame::parse(text, version) else {
            return Relayed::Forward(text.to_owned());
        };
        match self.relay.inspect(identity, direction, &frame) {
            RelayDecision::Forward => Relayed::Forward(text.to_owned()),
            RelayDecision::Replace(text) => Relayed::Forward(text),
            RelayDecision::Drop => Relayed::Drop,
            RelayDecision::Reject(error) => match &frame {
                Frame::Call { id, .. } => {
                    let answer = Frame::CallError {
                        id: id.clone(),
                        error: (&error).into(),
                    };
                    answer
                        .to_json(version)
                        .map_or(Relayed::Drop, Relayed::Answer)
                }
                // A SEND is never answered (FR.07), and answering a response is meaningless.
                _ => Relayed::Drop,
            },
        }
    }
}

enum Relayed {
    Forward(String),
    Answer(String),
    Drop,
}

/// A ready-made [`Relay`] that refuses one action locally.
///
/// Handy for a Local Controller that owns smart charging for its site and must not let the
/// CSMS address the stations directly.
pub struct RefuseActions {
    actions: Vec<String>,
}

impl RefuseActions {
    /// Refuses the given actions with `NotSupported`.
    #[must_use]
    pub fn new(actions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            actions: actions.into_iter().map(Into::into).collect(),
        }
    }
}

impl Relay for RefuseActions {
    fn inspect(&self, _: &Identity, _: Direction, frame: &Frame<'_>) -> RelayDecision {
        match frame.action() {
            Some(action) if self.actions.iter().any(|a| a == action) => RelayDecision::Reject(
                CallError::new(ErrorCode::NotSupported, "handled by the local controller"),
            ),
            _ => RelayDecision::Forward,
        }
    }
}
