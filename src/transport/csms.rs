//! The CSMS side of the transport: a WebSocket server with a per-identity session router.
//!
//! The HTTP upgrade is performed here rather than delegated, because OCPP puts requirements
//! on it that a synchronous callback cannot meet:
//! authentication is a database lookup (so it must be `async`), an unknown Charging Station
//! identity should be answered with **404**, bad credentials with **401**, and a client whose
//! subprotocols the CSMS cannot speak must get a *successful* handshake **without** a
//! `Sec-WebSocket-Protocol` header followed by an immediate close (Part 4 §3.1.1).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, mpsc, watch};

use crate::decode::DecodeOptions;
use crate::engine::{BootState, CallFailure, CallOptions, Engine, EngineConfig, MemStore, Role};
use crate::message::{Confirmed, NoResponse, Request, Unconfirmed};
use crate::types::Identity;
use crate::version::{Subprotocol, Version};

use super::TransportError;
use super::connection::{BoxFuture, Driver, Handle, Handler, Keepalive, SessionState, Shared};
use super::security::{Credentials, SecurityProfile};
use super::stream::MaybeTls;
use super::ws::handshake::{read_request, write_accept, write_refusal};
use super::ws::{
    CloseCode, CloseFrame, Config as WsConfig, Message, Role as WsRole, WebSocket, WsCodec,
};

/// What a CSMS is told about a connecting Charging Station.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Auth {
    /// The identity from the URL path, already length- and character-checked.
    pub identity: Identity,
    /// What the station presented.
    ///
    /// For HTTP Basic the user name has already been checked against
    /// [`identity`](Self::identity) (A00.FR.204, A00.FR.207): a mismatch is answered 401
    /// before an [`Authenticator`] is asked anything.
    pub credentials: Credentials,
    /// Where it connected from.
    pub remote: SocketAddr,
    /// Which profile the connection is using, derived from the transport and the credentials.
    pub profile: SecurityProfile,
    /// The full request path, for multi-tenant deployments.
    pub path: String,
    /// The versions the station offered, most preferred first.
    pub offered: Vec<Version>,
}

/// The verdict on a connecting Charging Station.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthOutcome {
    /// Let it in.
    Accept,
    /// The credentials are wrong — answered with HTTP 401.
    Reject,
    /// The identity is not provisioned — answered with HTTP 404, as Part 4 §3.1.1 suggests,
    /// so an operator can tell a typo from a bad password.
    Unknown,
}

/// Decides whether a Charging Station may connect.
pub trait Authenticator: Send + Sync + 'static {
    /// Runs one authentication.
    fn authenticate(&self, auth: Auth) -> BoxFuture<'_, AuthOutcome>;
}

impl<F, Fut> Authenticator for F
where
    F: Fn(Auth) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AuthOutcome> + Send + 'static,
{
    fn authenticate(&self, auth: Auth) -> BoxFuture<'_, AuthOutcome> {
        Box::pin(self(auth))
    }
}

/// An [`Authenticator`] that accepts every station, whatever it presents.
///
/// For development and tests. It has to be passed to [`CsmsBuilder::authenticate`] explicitly:
/// a CSMS that authenticates nobody should be something you chose, not something you forgot.
pub struct AcceptEveryStation;

impl Authenticator for AcceptEveryStation {
    fn authenticate(&self, _auth: Auth) -> BoxFuture<'_, AuthOutcome> {
        Box::pin(async { AuthOutcome::Accept })
    }
}

/// Something that happened to a station's session.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    /// A station connected.
    Opened {
        /// Which station.
        identity: Identity,
        /// The negotiated version.
        version: Version,
        /// Where it connected from.
        remote: SocketAddr,
    },
    /// A station's session ended.
    Closed {
        /// Which station.
        identity: Identity,
        /// Why.
        reason: String,
    },
    /// A station reconnected while an older session was still open; the older one was closed.
    Superseded {
        /// Which station.
        identity: Identity,
    },
    /// A connection was refused during the handshake.
    Refused {
        /// The identity it claimed, when the path carried a usable one.
        identity: Option<Identity>,
        /// The HTTP status that was returned.
        status: u16,
        /// Where it connected from.
        remote: SocketAddr,
    },
}

/// Builds a [`Csms`].
pub struct CsmsBuilder {
    bind: Option<SocketAddr>,
    versions: Subprotocol,
    #[cfg(feature = "rustls")]
    tls: Option<super::tls::ServerTls>,
    authenticator: Option<Arc<dyn Authenticator>>,
    handler: Option<Arc<dyn Handler>>,
    decode: Arc<dyn Fn(&Identity) -> DecodeOptions + Send + Sync>,
    engine: Option<EngineConfig>,
    max_connections: usize,
    max_pending_handshakes: usize,
    handshake_timeout: Duration,
    keepalive: Keepalive,
    ws_config: WsConfig,
    #[cfg(feature = "compression")]
    compression: bool,
    supersede: bool,
    supersede_drain: Duration,
}

impl Default for CsmsBuilder {
    fn default() -> Self {
        Self {
            bind: None,
            versions: Subprotocol::default(),
            #[cfg(feature = "rustls")]
            tls: None,
            authenticator: None,
            handler: None,
            decode: Arc::new(|_| DecodeOptions::strict()),
            engine: None,
            max_connections: 10_000,
            max_pending_handshakes: 512,
            handshake_timeout: Duration::from_secs(15),
            keepalive: Keepalive::default(),
            ws_config: WsConfig::default(),
            // 2.1 Part 4 §3.4 Table 2: a CSMS is *required* to support permessage-deflate.
            #[cfg(feature = "compression")]
            compression: true,
            supersede: true,
            supersede_drain: Duration::from_secs(5),
        }
    }
}

impl CsmsBuilder {
    /// The address to listen on.
    #[must_use]
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = Some(addr);
        self
    }

    /// The versions this CSMS speaks, most preferred first.
    ///
    /// Negotiation follows the *client's* preference order among these (Part 4 §3.1.2).
    #[must_use]
    pub fn versions(mut self, versions: impl IntoIterator<Item = Version>) -> Self {
        self.versions = Subprotocol::new(versions);
        self
    }

    /// Serves TLS — security profile 2, or profile 3 when the configuration demands a
    /// client certificate.
    #[cfg(feature = "rustls")]
    #[must_use]
    pub fn tls(mut self, tls: super::tls::ServerTls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// How to authenticate connecting stations. **Required** —
    /// [`build`](Self::build) fails without it; pass [`AcceptEveryStation`] to accept
    /// everyone on purpose.
    ///
    /// HTTP Basic credentials have already been checked against the identity in the URL
    /// (A00.FR.204, A00.FR.207), so [`Auth::credentials`]'s user name and
    /// [`Auth::identity`] are the same string.
    #[must_use]
    pub fn authenticate(mut self, authenticator: impl Authenticator) -> Self {
        self.authenticator = Some(Arc::new(authenticator));
        self
    }

    /// The handler for requests stations send.
    #[must_use]
    pub fn handler(mut self, handler: impl Handler) -> Self {
        self.handler = Some(Arc::new(handler));
        self
    }

    /// Per-station decoding policy — the hook for vendor quirk profiles.
    ///
    /// The *frame* size limit is not per-station, because it applies before the identity is
    /// even known; set it with [`max_message_size_bytes`](Self::max_message_size_bytes).
    #[must_use]
    pub fn decode_options_for(
        mut self,
        pick: impl Fn(&Identity) -> DecodeOptions + Send + Sync + 'static,
    ) -> Self {
        self.decode = Arc::new(pick);
        self
    }

    /// The largest WebSocket message the server will buffer.
    ///
    /// Refused at the socket, before the payload is parsed, so a hostile peer cannot make the
    /// server allocate on demand. A compressed message is measured as it inflates, so a small
    /// frame cannot expand past the limit either.
    #[must_use]
    pub fn max_message_size_bytes(mut self, bytes: usize) -> Self {
        self.ws_config.max_message_size = bytes;
        self.ws_config.max_frame_size = bytes;
        self
    }

    /// Whether to negotiate RFC 7692 `permessage-deflate`.
    ///
    /// On by default with the `compression` feature, because 2.1 Part 4 §3.4 Table 2 makes it
    /// **required** for a CSMS. Turning it off is for debugging: a capture of uncompressed
    /// frames is a great deal easier to read.
    #[cfg(feature = "compression")]
    #[must_use]
    pub fn compression(mut self, enabled: bool) -> Self {
        self.compression = enabled;
        self
    }

    /// Overrides the engine configuration. The role and version are set per session.
    #[must_use]
    pub fn engine_config(mut self, config: EngineConfig) -> Self {
        self.engine = Some(config);
        self
    }

    /// The maximum number of simultaneous sessions.
    #[must_use]
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// How often to send a WebSocket ping to each station.
    ///
    /// A station that stops answering pings has its session closed after
    /// [`Keepalive::timeout`](super::Keepalive::timeout), which is what frees the identity
    /// for the station's own reconnect instead of leaving a zombie session holding it.
    #[must_use]
    pub fn ping_interval(mut self, interval: Option<Duration>) -> Self {
        self.keepalive.interval = interval;
        self
    }

    /// The full WebSocket keepalive policy: ping interval and pong deadline.
    #[must_use]
    pub fn keepalive(mut self, keepalive: Keepalive) -> Self {
        self.keepalive = keepalive;
        self
    }

    /// How many connections may be in the TLS or HTTP handshake at once.
    ///
    /// [`max_connections`](Self::max_connections) bounds *established* sessions, which is no
    /// help against a peer that opens sockets and then says nothing. This bounds the other
    /// half, and [`handshake_timeout`](Self::handshake_timeout) makes each slot turn over.
    #[must_use]
    pub fn max_pending_handshakes(mut self, max: usize) -> Self {
        self.max_pending_handshakes = max.max(1);
        self
    }

    /// How long a connection may take to get from `accept` to a finished OCPP-J handshake.
    #[must_use]
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Whether a new connection closes an existing session for the same identity.
    ///
    /// On by default: a station that reconnects after a network partition would otherwise be
    /// locked out by its own zombie session.
    #[must_use]
    pub fn supersede_existing(mut self, supersede: bool) -> Self {
        self.supersede = supersede;
        self
    }

    /// How long a superseded session may take to drain before it is cut.
    #[must_use]
    pub fn supersede_drain(mut self, deadline: Duration) -> Self {
        self.supersede_drain = deadline;
        self
    }

    /// Builds the server.
    pub fn build(self) -> Result<Csms, TransportError> {
        let bind = self
            .bind
            .ok_or_else(|| TransportError::Configuration("bind address is required".into()))?;
        let authenticator = self.authenticator.ok_or_else(|| {
            TransportError::Configuration(
                "an authenticator is required: call `authenticate(…)`, or \
                 `authenticate(AcceptEveryStation)` to accept every station on purpose"
                    .into(),
            )
        })?;
        Ok(Csms {
            bind,
            versions: self.versions,
            #[cfg(feature = "rustls")]
            tls: self.tls,
            authenticator,
            handler: self
                .handler
                .unwrap_or_else(|| Arc::new(super::connection::NotImplemented)),
            decode: self.decode,
            engine: self.engine,
            max_connections: self.max_connections,
            handshake_timeout: self.handshake_timeout,
            handshakes: Arc::new(tokio::sync::Semaphore::new(self.max_pending_handshakes)),
            keepalive: self.keepalive,
            ws_config: self.ws_config,
            #[cfg(feature = "compression")]
            compression: self.compression,
            supersede: self.supersede,
            supersede_drain: self.supersede_drain,
            router: Arc::new(Router::default()),
            events: broadcast::channel(1024).0,
        })
    }
}

#[derive(Default)]
struct Router {
    sessions: Mutex<HashMap<Identity, Session>>,
    /// Hands out a distinct number to every session ever opened.
    next_session: std::sync::atomic::AtomicU64,
}

impl Router {
    fn next_session_id(&self) -> u64 {
        self.next_session
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

struct Session {
    /// Identifies *this* session among all the ones this identity has had.
    ///
    /// Without it, a superseded session tearing down would remove the entry its successor had
    /// already installed, and the station would be connected but unreachable through
    /// [`CsmsHandle`] until it reconnected again.
    id: u64,
    handle: Handle,
    task: tokio::task::JoinHandle<()>,
}

/// An OCPP CSMS server.
pub struct Csms {
    bind: SocketAddr,
    versions: Subprotocol,
    #[cfg(feature = "rustls")]
    tls: Option<super::tls::ServerTls>,
    authenticator: Arc<dyn Authenticator>,
    handler: Arc<dyn Handler>,
    decode: Arc<dyn Fn(&Identity) -> DecodeOptions + Send + Sync>,
    engine: Option<EngineConfig>,
    max_connections: usize,
    handshake_timeout: Duration,
    handshakes: Arc<tokio::sync::Semaphore>,
    keepalive: Keepalive,
    ws_config: WsConfig,
    #[cfg(feature = "compression")]
    compression: bool,
    supersede: bool,
    supersede_drain: Duration,
    router: Arc<Router>,
    events: broadcast::Sender<SessionEvent>,
}

impl Csms {
    /// Starts building a CSMS.
    #[must_use]
    pub fn builder() -> CsmsBuilder {
        CsmsBuilder::default()
    }

    /// A handle for talking to connected stations. Clone it freely.
    #[must_use]
    pub fn handle(&self) -> CsmsHandle {
        CsmsHandle {
            router: self.router.clone(),
            events: self.events.clone(),
        }
    }

    /// Serves until the listener fails.
    pub async fn serve(self) -> Result<(), TransportError> {
        let listener = TcpListener::bind(self.bind).await?;
        #[cfg(feature = "tracing")]
        tracing::info!(address = ?listener.local_addr(), "CSMS listening");
        self.serve_on(listener).await
    }

    /// Serves on a listener the caller already bound — for tests, and for socket activation.
    pub async fn serve_on(self, listener: TcpListener) -> Result<(), TransportError> {
        let server = Arc::new(self);
        loop {
            let (stream, remote) = listener.accept().await?;
            // Admission control before the task, not inside it: a peer that opens sockets
            // faster than they can be handshaked must be made to wait on the accept queue,
            // where the kernel bounds it, rather than in a task of its own.
            let Ok(permit) = server.handshakes.clone().acquire_owned().await else {
                continue;
            };
            let server = server.clone();
            tokio::spawn(async move {
                let timeout = server.handshake_timeout;
                let outcome = tokio::time::timeout(timeout, server.accept(stream, remote)).await;
                drop(permit);
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        #[cfg(feature = "tracing")]
                        tracing::debug!(%remote, %error, "connection rejected");
                        let _ = error;
                    }
                    Err(_) => {
                        #[cfg(feature = "tracing")]
                        tracing::debug!(%remote, ?timeout, "handshake timed out");
                    }
                }
            });
        }
    }

    /// The address the server was *configured* with.
    ///
    /// Not necessarily the address it is listening on: `0.0.0.0:0` binds to an ephemeral
    /// port, and only the [`TcpListener`] knows which. Bind the listener yourself and use
    /// [`serve_on`](Self::serve_on) when the actual port matters.
    #[must_use]
    pub fn configured_addr(&self) -> SocketAddr {
        self.bind
    }

    #[allow(clippy::too_many_lines)]
    async fn accept(
        self: Arc<Self>,
        stream: TcpStream,
        remote: SocketAddr,
    ) -> Result<(), TransportError> {
        stream.set_nodelay(true)?;

        let mut socket = if cfg!(feature = "rustls") {
            #[cfg(feature = "rustls")]
            match &self.tls {
                Some(tls) => MaybeTls::ServerTls(Box::new(tls.acceptor().accept(stream).await?)),
                None => MaybeTls::Plain(stream),
            }
            #[cfg(not(feature = "rustls"))]
            MaybeTls::Plain(stream)
        } else {
            MaybeTls::Plain(stream)
        };

        let request = read_request(&mut socket).await?;
        // RFC 6455 §4.2.1 / §4.4 — answer a malformed upgrade rather than dropping the
        // socket, so an operator can see why.
        if let Some(defect) = request.upgrade_defect() {
            let (status, reason) = defect.status();
            self.refuse(&mut socket, request.identity(), status, reason, remote)
                .await;
            return Ok(());
        }
        let identity = request.identity();
        let peer_certificate = socket.peer_certificate();

        let profile = match (&peer_certificate, socket_is_tls(&socket)) {
            (Some(_), _) => SecurityProfile::TlsClientCertificate,
            (None, true) => SecurityProfile::TlsBasicAuth,
            (None, false) => SecurityProfile::BasicAuth,
        };

        let Some(identity) = identity else {
            self.refuse(&mut socket, None, 400, "Bad Request", remote)
                .await;
            return Ok(());
        };

        let credentials = match peer_certificate {
            Some(der) => Credentials::ClientCertificate { der },
            None => request
                .header("authorization")
                .map_or(Credentials::None, Credentials::from_authorization_header),
        };

        // A00.FR.204 makes the Basic username the identity from the URL; A00.FR.207 makes
        // validating that the CSMS's duty. Checked here rather than in the `Authenticator`,
        // because the natural way to write one is to look the password up by the username
        // while the session is filed under the identity from the *path* — so a peer could
        // present its own credentials under someone else's identity.
        if let Credentials::Basic { user, .. } = &credentials {
            if user != identity.as_str() {
                #[cfg(feature = "tracing")]
                tracing::warn!(
                    path_identity = %identity,
                    basic_user = %user,
                    %remote,
                    "the Basic username does not match the URL identity (A00.FR.204/207)"
                );
                self.refuse(&mut socket, Some(identity), 401, "Unauthorized", remote)
                    .await;
                return Ok(());
            }
        }

        let offered: Vec<Version> = request.subprotocols();
        let auth = Auth {
            identity: identity.clone(),
            credentials,
            remote,
            profile,
            path: request.path.clone(),
            offered: offered.clone(),
        };

        match self.authenticator.authenticate(auth).await {
            AuthOutcome::Accept => {}
            AuthOutcome::Reject => {
                self.refuse(&mut socket, Some(identity), 401, "Unauthorized", remote)
                    .await;
                return Ok(());
            }
            AuthOutcome::Unknown => {
                self.refuse(&mut socket, Some(identity), 404, "Not Found", remote)
                    .await;
                return Ok(());
            }
        }

        if self.router.sessions.lock().await.len() >= self.max_connections {
            self.refuse(
                &mut socket,
                Some(identity),
                503,
                "Service Unavailable",
                remote,
            )
            .await;
            return Ok(());
        }

        // Part 4 §3.1.2: the CSMS selects one of the *client's* offers, honouring the
        // client's preference order.
        let offer = Subprotocol::new(offered);
        let selected = offer.select(self.versions.offered());
        // Present: `upgrade_defect` refused the request above if it was not.
        let key = request.websocket_key().unwrap_or_default().to_owned();

        let Some(version) = selected else {
            // Part 4 §3.1.1: complete the handshake *without* the subprotocol header, then
            // close immediately. Answering with an error status here would be wrong.
            write_accept(&mut socket, &key, None, None).await?;
            let mut ws = super::ws::attach(
                socket,
                WsCodec::new(WsRole::Server, self.ws_config),
                &request.head.rest,
            );
            let _ = ws
                .send(Message::Close(Some(CloseFrame::new(
                    CloseCode::PROTOCOL_ERROR,
                    "no common OCPP subprotocol",
                ))))
                .await;
            let _ = ws.close().await;
            let _ = self.events.send(SessionEvent::Refused {
                identity: Some(identity),
                status: 101,
                remote,
            });
            return Ok(());
        };

        // 2.1 Part 4 §3.4: the CSMS *shall* support RFC 7692. A station that does not offer
        // it is served uncompressed and must not be disconnected for it.
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
            &mut socket,
            &key,
            Some(version.subprotocol()),
            extensions.as_deref(),
        )
        .await?;
        let ws = super::ws::attach(socket, codec, &request.head.rest);

        self.run_session(ws, identity, version, remote).await;
        Ok(())
    }

    async fn refuse(
        &self,
        socket: &mut MaybeTls,
        identity: Option<Identity>,
        status: u16,
        reason: &str,
        remote: SocketAddr,
    ) {
        let _ = write_refusal(socket, status, reason).await;
        let _ = tokio::io::AsyncWriteExt::shutdown(socket).await;
        let _ = self.events.send(SessionEvent::Refused {
            identity,
            status,
            remote,
        });
    }

    async fn run_session(
        self: &Arc<Self>,
        socket: WebSocket<MaybeTls>,
        identity: Identity,
        version: Version,
        remote: SocketAddr,
    ) {
        let (commands_tx, mut commands_rx) = mpsc::channel(64);
        let (events_tx, _) = broadcast::channel(256);
        let (state_tx, state_rx) = watch::channel(SessionState {
            connected: false,
            version,
            boot: BootState::Idle,
            queued: 0,
        });
        let shared = Arc::new(Shared {
            identity: identity.clone(),
            remote: Some(remote),
            decode: (self.decode)(&identity),
            commands: commands_tx,
            events: events_tx,
            state: state_rx,
        });
        let handle = Handle::new(shared.clone());

        let mut config = self
            .engine
            .clone()
            .unwrap_or_else(|| EngineConfig::new(Role::Csms, version));
        config.role = Role::Csms;
        config.version = version;

        let Ok(engine) = Engine::with_store(config, MemStore::new()) else {
            return;
        };
        let mut driver = Driver::new(engine, shared.clone(), self.handler.clone(), state_tx);

        let session_id = self.router.next_session_id();

        // Single active connection per identity: the newer one wins, and the older is drained
        // rather than cut, so an in-flight answer still reaches the station.
        let superseded = {
            let mut sessions = self.router.sessions.lock().await;
            match sessions.remove(&identity) {
                Some(previous) if self.supersede => {
                    let _ = self.events.send(SessionEvent::Superseded {
                        identity: identity.clone(),
                    });
                    Some(previous)
                }
                Some(previous) => {
                    // Keep the older session; the newer connection is the one that goes.
                    sessions.insert(identity.clone(), previous);
                    return;
                }
                None => None,
            }
        };
        if let Some(previous) = superseded {
            let drain = self.supersede_drain;
            tokio::spawn(async move {
                // The drain gets its deadline before the task is cut: aborting first kills
                // the driver loop, so the in-flight answer never reaches the station.
                let _ = tokio::time::timeout(drain, previous.handle.shutdown(drain)).await;
                previous.task.abort();
            });
        }

        let _ = self.events.send(SessionEvent::Opened {
            identity: identity.clone(),
            version,
            remote,
        });

        let server = self.clone();
        let session_identity = identity.clone();
        let keepalive = self.keepalive;
        let task = tokio::spawn(async move {
            let outcome = driver
                .run(socket, &mut commands_rx, version, keepalive)
                .await;
            driver.abandon(&CallFailure::Disconnected);
            let reason = match outcome {
                super::connection::Ended::Disconnected(reason) => reason,
                super::connection::Ended::Closed(why) => format!("{why:?}"),
            };
            {
                // Remove *this* session, not whichever one holds the identity now: a
                // superseded session finishing must not evict the successor that replaced it,
                // which would leave a connected station unreachable through `CsmsHandle`.
                let mut sessions = server.router.sessions.lock().await;
                if sessions
                    .get(&session_identity)
                    .is_some_and(|session| session.id == session_id)
                {
                    sessions.remove(&session_identity);
                }
            }
            let _ = server.events.send(SessionEvent::Closed {
                identity: session_identity,
                reason,
            });
        });

        self.router.sessions.lock().await.insert(
            identity,
            Session {
                id: session_id,
                handle,
                task,
            },
        );
    }
}

fn socket_is_tls(socket: &MaybeTls) -> bool {
    !matches!(socket, MaybeTls::Plain(_))
}

/// A handle on the running CSMS.
#[derive(Clone)]
pub struct CsmsHandle {
    router: Arc<Router>,
    events: broadcast::Sender<SessionEvent>,
}

impl CsmsHandle {
    /// The session for one station, if it is connected.
    pub async fn session(&self, identity: &Identity) -> Option<Handle> {
        self.router
            .sessions
            .lock()
            .await
            .get(identity)
            .map(|session| session.handle.clone())
    }

    /// Every connected station.
    pub async fn sessions(&self) -> Vec<Identity> {
        self.router.sessions.lock().await.keys().cloned().collect()
    }

    /// How many stations are connected.
    pub async fn len(&self) -> usize {
        self.router.sessions.lock().await.len()
    }

    /// Whether any station is connected.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Sends a typed request to one station and waits for its answer.
    pub async fn call<R: Request + Confirmed>(
        &self,
        identity: &Identity,
        request: R,
    ) -> Result<R::Response, CallFailure> {
        self.call_with(identity, request, CallOptions::default())
            .await
    }

    /// Sends a typed request with per-call overrides.
    pub async fn call_with<R: Request + Confirmed>(
        &self,
        identity: &Identity,
        request: R,
        options: CallOptions,
    ) -> Result<R::Response, CallFailure> {
        match self.session(identity).await {
            Some(handle) => handle.call_with(request, options).await,
            None => Err(CallFailure::Disconnected),
        }
    }

    /// Sends an unconfirmed OCPP 2.1 `SEND` to one station.
    pub async fn send<R>(&self, identity: &Identity, request: R) -> Result<(), CallFailure>
    where
        R: Request<Response = NoResponse> + Unconfirmed,
    {
        match self.session(identity).await {
            Some(handle) => handle.send(request).await,
            None => Err(CallFailure::Disconnected),
        }
    }

    /// Subscribes to connect / disconnect / refusal events.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Drains every session and closes it.
    pub async fn shutdown(&self, deadline: Duration) {
        let handles: Vec<Handle> = self
            .router
            .sessions
            .lock()
            .await
            .values()
            .map(|session| session.handle.clone())
            .collect();
        // Concurrently: a fleet of ten thousand sessions must not be drained one after the
        // other, or the deadline becomes a per-session cost rather than a total one.
        let drains = handles
            .into_iter()
            .map(|handle| async move { handle.shutdown(deadline).await });
        futures_util::future::join_all(drains).await;
    }
}
