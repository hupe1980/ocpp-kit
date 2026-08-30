//! The loop that couples a WebSocket to an [`Engine`].
//!
//! Everything version- and role-specific lives in the engine; this file only turns
//! [`Output`]s into socket writes, timer sleeps and handler tasks, and turns socket reads
//! and application commands into [`Input`]s.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::value::RawValue;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};

use crate::decode::DecodeOptions;
use crate::engine::{
    Answer, BootState, CallFailure, CallOptions, CallToken, ClockSample, CloseReason, Engine,
    Input, Instant, MessageStore, Output, ProtocolViolation, Timer,
};
use crate::message::{ActionName, Confirmed, NoResponse, Request, Unconfirmed};
use crate::rpc::CallError;
use crate::types::{Identity, MessageId};
use crate::version::Version;

use super::TransportError;
use super::ws::{CloseCode, CloseFrame, Message, WebSocket, WsError};

/// A boxed future, so the handler traits stay object-safe without a macro crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How many handler tasks one session may run at once.
///
/// The specification allows a single outstanding `CALL` per direction; this is generous
/// headroom for the peers that break the rule, and a hard stop for the ones that abuse it.
const MAX_CONCURRENT_HANDLERS: usize = 32;

/// A request that arrived from the peer.
pub use crate::engine::IncomingRequest;

/// What an application implements to answer the peer.
///
/// One method, taking the action name and the still-unparsed payload. Decode it with the
/// per-version dispatch union — `v2_1::CsmsRequest::decode` on a station,
/// `v2_1::CsRequest::decode` on a CSMS — and `match` on the result.
///
/// ```no_run
/// use ocpp_kit::rpc::CallError;
/// use ocpp_kit::transport::{BoxFuture, Ctx, Handler};
/// use ocpp_kit::engine::IncomingRequest;
/// use ocpp_kit::v2_1;
/// use serde_json::value::RawValue;
///
/// struct MyStation;
///
/// impl Handler for MyStation {
///     fn on_request(
///         &self,
///         ctx: Ctx,
///         request: IncomingRequest,
///     ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
///         Box::pin(async move {
///             let action = v2_1::Action::from_wire(&request.action)
///                 .ok_or_else(|| CallError::not_implemented(&request.action))?;
///             match v2_1::CsmsRequest::decode(action, &request.payload, ctx.decode_options())? {
///                 v2_1::CsmsRequest::Reset(_) => ctx.reply(&v2_1::ResetResponse::new(
///                     v2_1::ResetStatus::Accepted,
///                 )),
///                 other => Err(CallError::not_supported(other.action().as_str())),
///             }
///         })
///     }
/// }
/// ```
pub trait Handler: Send + Sync + 'static {
    /// Answers one request. Returning `Err` sends a `CALLERROR`.
    ///
    /// A `SEND` (`request.kind == MessageKind::Send`) is never answered; whatever this
    /// returns for one is discarded.
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>>;

    /// Observes session lifecycle events. The default does nothing.
    fn on_event(&self, _ctx: &Ctx, _event: &Event) {}
}

/// A handler that answers every request with `NotImplemented`.
///
/// Useful as a starting point and in tests.
pub struct NotImplemented;

impl Handler for NotImplemented {
    fn on_request(
        &self,
        _ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move { Err(CallError::not_implemented(&request.action)) })
    }
}

/// What happened to a session.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Event {
    /// The WebSocket handshake completed.
    Connected {
        /// The negotiated version.
        version: Version,
    },
    /// The connection ended.
    Disconnected {
        /// Why, for logging.
        reason: String,
    },
    /// The boot state machine moved.
    BootState(BootState),
    /// A `currentTime` was observed in a `BootNotificationResponse` or `HeartbeatResponse`.
    ClockSample(ClockSample),
    /// The peer broke a rule. Not fatal.
    Violation(ProtocolViolation),
    /// The peer could not use a `CALLRESULT` we sent (OCPP 2.1 `CALLRESULTERROR`).
    ResultRejected {
        /// The id of the rejected result.
        id: MessageId,
        /// Why.
        error: CallError,
    },
    /// The Charging Station switched to a different network configuration slot.
    ///
    /// This is what `OCPPCommCtrlr.ActiveNetworkProfile` reports.
    NetworkProfileSelected {
        /// The configuration slot now in use.
        configuration_slot: i32,
        /// Where it points.
        url: String,
    },
    /// A Charging Station is waiting before dialling again (Part 4 §5.4).
    Reconnecting {
        /// How many attempts have failed.
        attempt: u32,
        /// How long the station will wait.
        delay: Duration,
    },
}

/// A snapshot of a session, published on a `watch` channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionState {
    /// Whether the WebSocket is up.
    pub connected: bool,
    /// The negotiated version.
    pub version: Version,
    /// The boot state machine's state.
    pub boot: BootState,
    /// How many messages are waiting to be sent.
    pub queued: usize,
}

/// What a completed call hands back: how it ended, and the id it was correlated with.
pub(crate) type CallAnswer = Result<(Answer, Option<MessageId>), CallFailure>;

pub(crate) enum Command {
    Call {
        action: String,
        payload: Box<RawValue>,
        options: CallOptions,
        /// Carries the answer and the `MessageId` it arrived under, which a
        /// `CALLRESULTERROR` has to quote.
        reply: oneshot::Sender<CallAnswer>,
    },
    Respond {
        id: MessageId,
        result: Result<Box<RawValue>, CallError>,
    },
    /// A `CALLRESULT` we received could not be used (OCPP 2.1 `CALLRESULTERROR`).
    RejectResult {
        id: MessageId,
        error: CallError,
    },
    Shutdown {
        deadline: Duration,
    },
}

pub(crate) struct Shared {
    pub identity: Identity,
    pub remote: Option<SocketAddr>,
    pub decode: DecodeOptions,
    pub commands: mpsc::Sender<Command>,
    pub events: broadcast::Sender<Event>,
    pub state: watch::Receiver<SessionState>,
}

/// Everything a handler needs: who the peer is, what was negotiated, and a way to call back.
#[derive(Clone)]
pub struct Ctx {
    shared: Arc<Shared>,
    version: Version,
    id: MessageId,
}

impl Ctx {
    pub(crate) fn new(shared: Arc<Shared>, version: Version, id: MessageId) -> Self {
        Self {
            shared,
            version,
            id,
        }
    }

    /// The Charging Station this session belongs to.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.shared.identity
    }

    /// The peer's address, when the transport knows it.
    #[must_use]
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.shared.remote
    }

    /// The negotiated protocol version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// The `MessageId` of the request being handled.
    #[must_use]
    pub fn message_id(&self) -> &MessageId {
        &self.id
    }

    /// The decoding policy configured for this peer.
    #[must_use]
    pub fn decode_options(&self) -> &DecodeOptions {
        &self.shared.decode
    }

    /// A handle for calling the peer back, including from inside a handler.
    ///
    /// Reverse calls are queued by the engine, so using this does not deadlock against the
    /// one-outstanding-`CALL` rule.
    #[must_use]
    pub fn handle(&self) -> Handle {
        Handle {
            shared: self.shared.clone(),
        }
    }

    /// Serializes a typed response payload.
    pub fn reply<R: serde::Serialize>(&self, response: &R) -> Result<Box<RawValue>, CallError> {
        serde_json::value::to_raw_value(response)
            .map_err(|error| CallError::internal(format!("response is not serializable: {error}")))
    }
}

/// The application's handle on a session.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// The Charging Station identity this handle talks to.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.shared.identity
    }

    /// The current session state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        *self.shared.state.borrow()
    }

    /// Subscribes to session events.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.shared.events.subscribe()
    }

    /// Waits until the session is connected and the boot state machine allows traffic.
    ///
    /// Returns `false` if the session ends first.
    pub async fn wait_ready(&self) -> bool {
        let mut state = self.shared.state.clone();
        loop {
            if state.borrow().connected && state.borrow().boot.allows_traffic() {
                return true;
            }
            if state.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Sends a typed request and waits for the typed response.
    ///
    /// The [`Confirmed`] bound excludes the OCPP 2.1 `SEND`s, which are never answered
    /// (Part 4 §4.2.4); they go through [`send`](Self::send).
    ///
    /// ```no_run
    /// # async fn example(handle: ocpp_kit::transport::Handle) -> Result<(), Box<dyn std::error::Error>> {
    /// use ocpp_kit::v2_1;
    /// let response = handle.call(v2_1::HeartbeatRequest::new()).await?;
    /// println!("CSMS time is {}", response.current_time);
    /// # Ok(()) }
    /// ```
    pub async fn call<R>(&self, request: R) -> Result<R::Response, CallFailure>
    where
        R: Request + Confirmed,
    {
        self.call_with(request, CallOptions::default()).await
    }

    /// Sends a typed request with per-call overrides.
    ///
    /// [`CallOptions::triggered`] is the one that matters most: B02.FR.02 keeps a station
    /// quiet until the CSMS accepts it, *except* for messages the CSMS asked for. A
    /// `TriggerMessage` answered without it sits in the queue until boot completes.
    ///
    /// ```no_run
    /// # async fn example(handle: ocpp_kit::transport::Handle) -> Result<(), Box<dyn std::error::Error>> {
    /// use ocpp_kit::engine::CallOptions;
    /// use ocpp_kit::v2_1;
    ///
    /// // Answering a TriggerMessage(StatusNotification) while the boot is still `Pending`.
    /// handle
    ///     .call_with(
    ///         v2_1::StatusNotificationRequest::new(
    ///             ocpp_kit::types::DateTime::now(),
    ///             v2_1::ConnectorStatus::Available,
    ///             1,
    ///             1,
    ///         ),
    ///         CallOptions::triggered(),
    ///     )
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub async fn call_with<R>(
        &self,
        request: R,
        options: CallOptions,
    ) -> Result<R::Response, CallFailure>
    where
        R: Request + Confirmed,
    {
        let payload = serde_json::value::to_raw_value(&request).map_err(|error| {
            CallFailure::Rejected(CallError::internal(format!(
                "request is not serializable: {error}"
            )))
        })?;
        let (answer, id) = self
            .call_raw_with_id(R::ACTION.as_str(), payload, options)
            .await?;
        let Some(raw) = answer.into_payload() else {
            // Unreachable: `Confirmed` is only implemented for actions with a response.
            return Err(CallFailure::Rejected(CallError::internal(
                "a CALL was completed as a SEND",
            )));
        };
        match crate::decode::decode_payload::<R::Response>(&raw, &self.shared.decode) {
            Ok(response) => Ok(response),
            Err(error) => {
                let error = CallError::from(error);
                // Part 4 §4.2.3: on 2.1 the sender of an unusable `CALLRESULT` is told so
                // with a `CALLRESULTERROR` instead of being left believing it succeeded.
                // Older versions have no such message, so the failure stays local.
                if let Some(id) = id {
                    let _ = self
                        .shared
                        .commands
                        .send(Command::RejectResult {
                            id,
                            error: error.clone(),
                        })
                        .await;
                }
                Err(CallFailure::Rejected(error))
            }
        }
    }

    /// Sends an unconfirmed OCPP 2.1 `SEND` and returns once it has been written.
    ///
    /// Part 4 §4.2.4 forbids the receiver from answering, so `Ok(())` means "on the wire",
    /// not "accepted".
    ///
    /// ```no_run
    /// # async fn example(handle: ocpp_kit::transport::Handle) -> Result<(), Box<dyn std::error::Error>> {
    /// use ocpp_kit::types::DateTime;
    /// use ocpp_kit::v2_1;
    /// handle
    ///     .send(v2_1::NotifyPeriodicEventStreamRequest::new(
    ///         vec![],
    ///         1,
    ///         0,
    ///         DateTime::now(),
    ///     ))
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub async fn send<R>(&self, request: R) -> Result<(), CallFailure>
    where
        R: Request<Response = NoResponse> + Unconfirmed,
    {
        self.send_with(request, CallOptions::default()).await
    }

    /// Sends an unconfirmed `SEND` with per-call overrides.
    pub async fn send_with<R>(&self, request: R, options: CallOptions) -> Result<(), CallFailure>
    where
        R: Request<Response = NoResponse> + Unconfirmed,
    {
        let payload = serde_json::value::to_raw_value(&request).map_err(|error| {
            CallFailure::Rejected(CallError::internal(format!(
                "request is not serializable: {error}"
            )))
        })?;
        self.call_raw_with_id(R::ACTION.as_str(), payload, options)
            .await
            .map(|_| ())
    }

    /// Sends a request the caller has already serialized.
    ///
    /// Returns [`Answer::Sent`] for a `SEND` and [`Answer::Result`] for everything else.
    pub async fn call_raw(
        &self,
        action: &str,
        payload: Box<RawValue>,
        options: CallOptions,
    ) -> Result<Answer, CallFailure> {
        self.call_raw_with_id(action, payload, options)
            .await
            .map(|(answer, _)| answer)
    }

    /// As [`call_raw`](Self::call_raw), but also reports the `MessageId` the call went out
    /// with — which is what a `CALLRESULTERROR` has to quote.
    async fn call_raw_with_id(
        &self,
        action: &str,
        payload: Box<RawValue>,
        options: CallOptions,
    ) -> Result<(Answer, Option<MessageId>), CallFailure> {
        let (reply, answer) = oneshot::channel();
        let command = Command::Call {
            action: action.to_owned(),
            payload,
            options,
            reply,
        };
        if self.shared.commands.send(command).await.is_err() {
            return Err(CallFailure::Disconnected);
        }
        let outcome = answer.await.unwrap_or(Err(CallFailure::Disconnected))?;
        Ok(outcome)
    }

    /// Starts a graceful drain and waits for it to finish or time out.
    pub async fn shutdown(&self, deadline: Duration) {
        let _ = self
            .shared
            .commands
            .send(Command::Shutdown { deadline })
            .await;
        let mut state = self.shared.state.clone();
        while state.borrow().connected {
            if state.changed().await.is_err() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The driver loop
// ---------------------------------------------------------------------------

/// How a session run ended.
pub(crate) enum Ended {
    /// The peer or the network closed it; a station should reconnect.
    Disconnected(String),
    /// The engine asked for the connection to close.
    Closed(CloseReason),
}

pub(crate) struct Driver<S: MessageStore> {
    pub engine: Engine<S>,
    pub shared: Arc<Shared>,
    pub handler: Arc<dyn Handler>,
    pub state: watch::Sender<SessionState>,
    pub started: tokio::time::Instant,
    pending: HashMap<CallToken, oneshot::Sender<CallAnswer>>,
    timers: BTreeMap<Timer, Instant>,
    /// Bounds how many handler tasks may run at once for this session.
    ///
    /// The engine already caps the peer's unanswered calls; this caps the *work* they can
    /// start, so one talkative station cannot occupy the runtime on behalf of the rest.
    slots: Arc<Semaphore>,
}

/// How the driver watches for a connection that has died without saying so.
///
/// A TCP connection that a mobile network silently dropped stays writable for minutes. The
/// WebSocket ping is the only end-to-end liveness signal there is (Part 4 §5.3), and it is
/// only a signal if an unanswered one eventually ends the session.
#[derive(Clone, Copy, Debug)]
pub struct Keepalive {
    /// How often to send a ping. `None` disables both the ping and the timeout.
    pub interval: Option<Duration>,
    /// How long to wait for the pong before giving up on the connection.
    pub timeout: Duration,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self {
            interval: Some(Duration::from_secs(60)),
            timeout: Duration::from_secs(30),
        }
    }
}

impl Keepalive {
    /// A keepalive with the given ping interval and the default timeout.
    #[must_use]
    pub fn every(interval: Duration) -> Self {
        Self {
            interval: Some(interval),
            ..Self::default()
        }
    }

    /// No pings, and therefore no liveness check.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            interval: None,
            timeout: Duration::ZERO,
        }
    }

    /// Sets how long an unanswered ping may go unanswered.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl<S: MessageStore> Driver<S> {
    pub fn new(
        engine: Engine<S>,
        shared: Arc<Shared>,
        handler: Arc<dyn Handler>,
        state: watch::Sender<SessionState>,
    ) -> Self {
        Self {
            engine,
            shared,
            handler,
            state,
            started: tokio::time::Instant::now(),
            pending: HashMap::new(),
            timers: BTreeMap::new(),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS)),
        }
    }

    fn now(&self) -> Instant {
        Instant::from_millis(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    /// Runs one connection until it ends. The engine survives, so a station keeps its queue
    /// and its boot state across reconnects (Part 4 §5.4: no `BootNotification` on a mere
    /// reconnect).
    ///
    /// This cannot fail, only end: every way a socket can break is a reason to reconnect, and
    /// there is no error a caller could usefully do anything else with.
    pub async fn run<St>(
        &mut self,
        socket: WebSocket<St>,
        commands: &mut mpsc::Receiver<Command>,
        version: Version,
        keepalive: Keepalive,
    ) -> Ended
    where
        St: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        let (mut sink, mut stream) = socket.split();

        self.engine.handle(self.now(), Input::Connected { version });
        self.publish(true, version);
        let _ = self.shared.events.send(Event::Connected { version });

        let mut ping = keepalive.interval.map(|interval| {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer
        });
        // `Some` while a ping is outstanding: the deadline by which a pong must arrive.
        let mut pong_due: Option<tokio::time::Instant> = None;

        // Every failure inside the loop ends this *connection*, never the driver: the engine
        // has to be told the socket is gone so it can fail or requeue what was outstanding,
        // and a station has to be free to reconnect (Part 4 §5.4, which says to reconnect
        // when the connection is lost and does not enumerate the ways it can be lost).
        // Returning early — which a bare `?` on the write path did — skipped both.
        let ended = loop {
            match self.flush(&mut sink).await {
                Ok(Some(outcome)) => break outcome,
                Ok(None) => {}
                Err(error) => break Ended::Disconnected(error.to_string()),
            }

            let sleep = self.next_deadline();
            // No `biased`: a fair poll keeps a flood of application commands from starving
            // the socket, and vice versa.
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(command) => self.apply(command),
                        None => break Ended::Closed(CloseReason::Requested),
                    }
                }

                message = stream.next() => {
                    // A pong proves the peer is alive; RFC 6455 §5.5.3 does not require the
                    // payload to match, and some peers do not echo it.
                    if matches!(message, Some(Ok(Message::Pong(_)))) {
                        pong_due = None;
                    }
                    if let Some(ended) = self.on_message(message, &mut sink).await {
                        break ended;
                    }
                }

                () = sleep => {
                    let now = self.now();
                    self.engine.handle(now, Input::Timeout);
                    self.timers.retain(|_, at| *at > now);
                }

                _ = async { ping.as_mut().expect("checked").tick().await }, if ping.is_some() => {
                    if pong_due.is_none() {
                        pong_due = Some(tokio::time::Instant::now() + keepalive.timeout);
                    }
                    if let Err(error) = sink.send(Message::Ping(Vec::new())).await {
                        break Ended::Disconnected(error.to_string());
                    }
                }

                // Part 4 §5.3: the ping is the end-to-end liveness check. A connection that
                // stops answering is gone, however healthy the TCP socket still looks.
                () = async { tokio::time::sleep_until(pong_due.expect("checked")).await },
                    if pong_due.is_some() =>
                {
                    break Ended::Disconnected(format!(
                        "no pong within {:?} of a WebSocket ping",
                        keepalive.timeout
                    ));
                }
            }
        };

        self.engine.handle(self.now(), Input::Disconnected);
        let _ = self.flush(&mut sink).await;
        let reason = match &ended {
            Ended::Disconnected(reason) => reason.clone(),
            Ended::Closed(why) => format!("{why:?}"),
        };
        self.publish(false, version);
        let _ = self.shared.events.send(Event::Disconnected { reason });
        if matches!(ended, Ended::Closed(_)) {
            let _ = sink
                .send(Message::Close(Some(CloseFrame::new(CloseCode::NORMAL, ""))))
                .await;
        }
        let _ = sink.close().await;
        ended
    }

    /// Handles one message off the socket. `Some` ends the connection.
    async fn on_message<Si>(
        &mut self,
        message: Option<Result<Message, WsError>>,
        sink: &mut Si,
    ) -> Option<Ended>
    where
        Si: futures_util::Sink<Message, Error = WsError> + Unpin,
    {
        match message {
            Some(Ok(Message::Text(text))) => {
                self.engine.handle(self.now(), Input::Received(&text));
                None
            }
            Some(Ok(Message::Ping(payload))) => {
                // RFC 6455 §5.5.2: answer as soon as practical.
                sink.send(Message::Pong(payload))
                    .await
                    .err()
                    .map(|error| Ended::Disconnected(error.to_string()))
            }
            // Part 4 §4.1: OCPP-J is text-only. A binary frame — and a pong, already
            // accounted for by the caller — is ignored rather than treated as fatal.
            Some(Ok(Message::Pong(_) | Message::Binary(_))) => None,
            Some(Ok(Message::Close(frame))) => {
                let reason = frame.as_ref().map_or_else(
                    || "peer closed".to_owned(),
                    |frame| format!("peer closed: {} {}", frame.code, frame.reason),
                );
                // §5.5.1: echo the close, then stop.
                let echo = frame.map(|frame| CloseFrame::new(frame.code, ""));
                let _ = sink.send(Message::Close(echo)).await;
                Some(Ended::Disconnected(reason))
            }
            Some(Err(error)) => {
                // A protocol error deserves the close code RFC 6455 assigns it, so the peer
                // learns what it did wrong.
                if let Some(code) = error.close_code() {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame::new(
                            code,
                            error.to_string(),
                        ))))
                        .await;
                }
                Some(Ended::Disconnected(error.to_string()))
            }
            None => Some(Ended::Disconnected("stream ended".to_owned())),
        }
    }

    /// Fails every outstanding call, for a session that ends without the engine running.
    pub fn abandon(&mut self, why: &CallFailure) {
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Err(why.clone()));
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Call {
                action,
                payload,
                options,
                reply,
            } => match self.engine.call_with(self.now(), &action, payload, options) {
                Ok(token) => {
                    self.pending.insert(token, reply);
                }
                Err(error) => {
                    let _ = reply.send(Err(CallFailure::Rejected(CallError::internal(
                        error.to_string(),
                    ))));
                }
            },
            Command::RejectResult { id, error } => {
                // Only 2.1 has CALLRESULTERROR; on older versions the engine says so and the
                // failure has already been reported to the caller.
                let _ = self.engine.reject_result(self.now(), &id, error);
            }
            Command::Respond { id, result } => {
                let now = self.now();
                let outcome = match result {
                    Ok(payload) => self.engine.respond(now, &id, &payload),
                    Err(error) => self.engine.respond_error(now, &id, error),
                };
                if let Err(error) = outcome {
                    let _ = self.shared.events.send(Event::Violation(
                        ProtocolViolation::UnexpectedResponse {
                            id: match error {
                                crate::engine::EngineError::NoSuchRequest(id) => id,
                                _ => id,
                            },
                        },
                    ));
                }
            }
            Command::Shutdown { deadline } => {
                let now = self.now();
                self.engine.shutdown(now, now.saturating_add(deadline));
            }
        }
    }

    /// Drains the engine's outputs, writing frames and spawning handler tasks.
    ///
    /// Returns `Some` when the engine asked for the connection to close.
    async fn flush<Si>(&mut self, sink: &mut Si) -> Result<Option<Ended>, TransportError>
    where
        Si: futures_util::Sink<Message, Error = WsError> + Unpin,
    {
        let mut close = None;
        let mut wrote = false;
        while let Some(output) = self.engine.poll_output() {
            match output {
                Output::Transmit(text) => {
                    sink.feed(Message::Text(text)).await?;
                    wrote = true;
                }
                Output::Request(request) => self.dispatch(request),
                Output::Outcome(outcome) => {
                    if let Some(reply) = self.pending.remove(&outcome.token) {
                        let id = outcome.id;
                        let _ = reply.send(outcome.result.map(|answer| (answer, id)));
                    }
                }
                Output::SetTimer { timer, at } => {
                    self.timers.insert(timer, at);
                }
                Output::ClearTimer(timer) => {
                    self.timers.remove(&timer);
                }
                Output::BootState(state) => {
                    let _ = self.shared.events.send(Event::BootState(state));
                }
                Output::ClockSample(sample) => {
                    let _ = self.shared.events.send(Event::ClockSample(sample));
                }
                Output::ResultRejected { id, error } => {
                    let _ = self.shared.events.send(Event::ResultRejected { id, error });
                }
                Output::Violation(violation) => {
                    let _ = self.shared.events.send(Event::Violation(violation));
                }
                Output::Close(reason) => close = Some(Ended::Closed(reason)),
            }
        }
        if wrote {
            sink.flush().await?;
        }
        let version = self.engine.version();
        self.publish(self.engine.is_connected(), version);
        Ok(close)
    }

    fn dispatch(&self, request: IncomingRequest) {
        let handler = self.handler.clone();
        let shared = self.shared.clone();
        let slots = self.slots.clone();
        let ctx = Ctx::new(shared.clone(), self.engine.version(), request.id.clone());
        let is_send = request.kind == crate::message::MessageKind::Send;
        let id = request.id.clone();
        tokio::spawn(async move {
            // Acquiring the permit *inside* the task keeps the driver loop non-blocking:
            // the request waits its turn instead of the socket doing so.
            let Ok(_permit) = slots.acquire().await else {
                return;
            };
            let result = handler.on_request(ctx, request).await;
            // FR.07 — a SEND is never answered.
            if is_send {
                return;
            }
            let _ = shared.commands.send(Command::Respond { id, result }).await;
        });
    }

    fn publish(&self, connected: bool, version: Version) {
        let next = SessionState {
            connected,
            version,
            boot: self.engine.boot_state(),
            queued: self.engine.queued(),
        };
        self.state.send_if_modified(|state| {
            if *state == next {
                false
            } else {
                *state = next;
                true
            }
        });
    }

    /// Sleeps until the earliest armed timer, or forever when none is armed.
    fn next_deadline(&self) -> impl Future<Output = ()> + use<S> {
        let next = self.timers.values().min().copied();
        let started = self.started;
        async move {
            match next {
                Some(at) => {
                    tokio::time::sleep_until(started + Duration::from_millis(at.as_millis())).await;
                }
                None => std::future::pending::<()>().await,
            }
        }
    }
}
