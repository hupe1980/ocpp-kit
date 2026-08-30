//! Layer 2 — the sans-I/O protocol engine.
//!
//! [`Engine`] is a pure state machine. It owns no socket, reads no clock and spawns no task:
//! the driver feeds it [`Input`]s and drains [`Output`]s. That is what makes every timing
//! rule in OCPP — message timeouts, the linear transaction-retry schedule, boot back-off —
//! testable in microseconds, and what lets the same code run on Tokio, inside an `embassy`
//! firmware loop, or in a browser.
//!
//! Every entry point takes the driver's current [`Instant`]: the engine has no clock of its
//! own, and a deadline armed against a stale one fires the moment it is armed.
//!
//! ```
//! use ocpp_kit::Version;
//! use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Output, Role};
//! use serde_json::value::RawValue;
//!
//! let now = Instant::ZERO;
//! let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
//! engine.handle(now, Input::Connected { version: Version::V2_1 });
//!
//! let payload = RawValue::from_string(
//!     r#"{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}"#.into(),
//! )
//! .unwrap();
//! engine.call(now, "BootNotification", payload).unwrap();
//!
//! let frame = engine
//!     .drain()
//!     .into_iter()
//!     .find_map(|output| match output {
//!         Output::Transmit(text) => Some(text),
//!         _ => None,
//!     })
//!     .unwrap();
//! assert!(frame.starts_with(r#"[2,"#));
//! ```
//!
//! In a test, [`Sim`](crate::testkit::Sim) owns the clock for you and the same session reads
//! as a transcript.
//!
//! # Rules this layer enforces
//!
//! * **One outstanding `CALL` per direction** (2.x Part 4 §4.1.1 `SHALL NOT`; 1.6J §4.1.1
//!   `SHOULD NOT` — hence [`InboundConcurrency`]). Our own extra calls are queued, never
//!   rejected.
//! * **`SEND` is exempt** and is never answered (Part 4 §4.2.4, FR.07).
//! * **Unknown message type numbers** are ignored on 1.6J and 2.1 and answered with
//!   `MessageTypeNotSupported` on 2.0.1 (§4.4).
//! * **Message timeout**, freeing the slot and starting the next queued call.
//! * **Transaction-message retries only**, on the linear schedule of 1.6 §3.7.1 and the 2.x
//!   `MessageAttempts[TransactionEvent]` variables, in chronological order, skipping a
//!   message once its attempts are exhausted.
//! * **Durable offline queueing** of transaction messages (E04/E08/E12) via
//!   [`MessageStore`] — [`MemStore`] in memory, [`FileStore`] on disk, or your own.
//! * **The boot state machine** (B01–B04) on *both* sides, including the CSMS's
//!   `SecurityError` answer to an unsolicited call from a `Pending` station (B02.FR.09).
//! * **Graceful drain**: finish the in-flight call, flush the queue, then close.

mod backoff;
mod queue;
#[cfg(feature = "std")]
mod store_file;
mod time;

use alloc::boxed::Box;
use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::actions;
use crate::message::{MessageKind, Origin};
use crate::rpc::{CallError, CallErrorRef, ErrorCode, Frame, FrameError, FrameReply};
use crate::types::{DateTime, IdGenerator, MessageId};
use crate::version::Version;

pub use backoff::Backoff;
pub use queue::{MemStore, MessageStore, QueuedCall, RetryPolicy, Seq, StoreError};
#[cfg(feature = "std")]
pub use store_file::FileStore;
pub use time::{Instant, Timer};

/// Which side of the connection this engine is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A Charging Station: it dials out, and it boots.
    ChargingStation,
    /// A CSMS: it accepts connections and decides whether a station may proceed.
    Csms,
}

impl Role {
    /// The origin this engine is allowed to send.
    const fn sends(self) -> Origin {
        match self {
            Role::ChargingStation => Origin::ChargingStation,
            Role::Csms => Origin::Csms,
        }
    }

    /// The origin this engine expects to receive.
    const fn receives(self) -> Origin {
        match self {
            Role::ChargingStation => Origin::Csms,
            Role::Csms => Origin::ChargingStation,
        }
    }
}

/// What to do when the peer breaks the one-outstanding-`CALL` rule.
///
/// 1.6J §4.1.1 words it as `SHOULD NOT`, 2.x Part 4 §4.1.1 as `SHALL NOT`. Field stations
/// break it either way, and dropping their calls is usually worse than serving them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InboundConcurrency {
    /// Answer the extra call normally. The default, and the only workable choice on 1.6.
    #[default]
    Serve,
    /// Answer the extra call with `CALLERROR: ProtocolError`.
    Reject,
}

/// Whether the engine sends `Heartbeat` on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HeartbeatPolicy {
    /// The engine schedules and sends `Heartbeat` itself.
    ///
    /// `OCPPCommCtrlr.HeartbeatInterval` is defined as the *interval of inactivity* — "no
    /// OCPP exchanges" — after which a station sends `Heartbeat`, so the timer is an idle
    /// timer that every frame in either direction resets, not a fixed period. The payload is
    /// an empty object in every version, so no application code is needed.
    #[default]
    Automatic,
    /// The application sends `Heartbeat` itself; the engine only reports the interval.
    Manual,
}

/// What happens to outgoing calls while the connection is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflinePolicy {
    /// Queue every message, not just transaction-related ones — the 1.6
    /// `QueueAllMessages` behaviour.
    pub queue_all_messages: bool,
    /// Upper bound on the in-memory queue. Reached, further calls fail with
    /// [`CallFailure::QueueFull`] instead of growing without limit.
    pub max_queued: usize,
}

impl Default for OfflinePolicy {
    fn default() -> Self {
        Self {
            queue_all_messages: false,
            max_queued: 1024,
        }
    }
}

/// Engine configuration.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct EngineConfig {
    /// Which side this is.
    pub role: Role,
    /// The negotiated protocol version.
    pub version: Version,
    /// How long to wait for a `CALLRESULT` / `CALLERROR`.
    ///
    /// The specification deliberately leaves this implementation-defined (Part 4 §4.1.1);
    /// 2.x sources it from `NetworkConnectionProfile.messageTimeout`, falling back to
    /// `OCPPCommCtrlr.MessageTimeout[Default]`. 30 s is the widely used default.
    pub call_timeout: Duration,
    /// What to do about a peer that keeps two calls in flight.
    pub inbound_concurrency: InboundConcurrency,
    /// Whether the engine sends `Heartbeat` itself.
    pub heartbeat: HeartbeatPolicy,
    /// Retry schedule for transaction-related messages.
    pub retry: RetryPolicy,
    /// Offline queueing behaviour.
    pub offline: OfflinePolicy,
    /// Reconnect back-off parameters, for the driver to use (Part 4 §5.4).
    pub backoff: Backoff,
    /// Whether the boot state machine gates outgoing calls (B02.FR.02).
    ///
    /// Always on for a Charging Station; a CSMS uses it to enforce B02.FR.09.
    pub enforce_boot_gate: bool,
    /// How long to wait before re-sending `BootNotification` when the CSMS answered
    /// `Pending` or `Rejected` with `interval = 0` (B02.FR.07).
    pub boot_retry_fallback: Duration,
    /// Upper bound on the peer's simultaneously unanswered `CALL`s.
    ///
    /// The specification allows exactly one (Part 4 §4.1.1), and
    /// [`InboundConcurrency::Serve`] deliberately tolerates more — but "more" must still be
    /// finite, or a peer that never stops asking can make the receiver hold requests, and
    /// handler tasks, without limit. Beyond this many the extra call is answered
    /// `ProtocolError` regardless of [`InboundConcurrency`].
    pub max_peer_requests: usize,
}

impl EngineConfig {
    /// A configuration with the specification's defaults.
    #[must_use]
    pub fn new(role: Role, version: Version) -> Self {
        Self {
            role,
            version,
            call_timeout: Duration::from_secs(30),
            inbound_concurrency: InboundConcurrency::Serve,
            heartbeat: HeartbeatPolicy::Automatic,
            retry: RetryPolicy::default(),
            offline: OfflinePolicy::default(),
            backoff: Backoff::default(),
            enforce_boot_gate: true,
            boot_retry_fallback: Duration::from_secs(30),
            max_peer_requests: 64,
        }
    }

    /// Sets the message timeout.
    #[must_use]
    pub const fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Sets the transaction retry schedule.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets the offline queueing policy.
    #[must_use]
    pub const fn with_offline(mut self, offline: OfflinePolicy) -> Self {
        self.offline = offline;
        self
    }

    /// Sets the heartbeat policy.
    #[must_use]
    pub const fn with_heartbeat(mut self, heartbeat: HeartbeatPolicy) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    /// Sets what happens when the peer keeps two calls in flight.
    #[must_use]
    pub const fn with_inbound_concurrency(mut self, mode: InboundConcurrency) -> Self {
        self.inbound_concurrency = mode;
        self
    }
}

/// The boot state machine (OCPP 2.x block B; 1.6 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BootState {
    /// No `BootNotification` has been answered yet. Only `BootNotification` and messages the
    /// CSMS explicitly asked for may be sent (B02.FR.02).
    #[default]
    Idle,
    /// The CSMS answered `Pending`: it is still configuring the station. The connection
    /// stays open (B02.FR.06) and the station may only answer what it is asked.
    Pending,
    /// The CSMS answered `Accepted`. Normal operation.
    Accepted,
    /// The CSMS answered `Rejected`. The station must wait for the interval before trying
    /// again (B02.FR.08).
    Rejected,
}

impl BootState {
    /// Whether ordinary, unsolicited calls may be sent.
    ///
    /// Only `Accepted` does: B01.FR.08 keeps a freshly powered station quiet until the CSMS
    /// answers, and B02.FR.02 keeps it quiet while the answer is `Pending`.
    #[must_use]
    pub const fn allows_traffic(self) -> bool {
        matches!(self, BootState::Accepted)
    }

    /// Whether a *CSMS* must answer an unsolicited call with `SecurityError`.
    ///
    /// B01.FR.10 and B02.FR.09 both have the same precondition: *the Charging Station has
    /// received a `BootNotificationResponse` whose status is not `Accepted`*. A station that
    /// has not been answered at all — the normal state of a reconnected station, which
    /// Part 4 §5.4 tells not to repeat its `BootNotification` — is therefore not covered,
    /// and blocking it would break every reconnect.
    #[must_use]
    pub const fn blocks_unsolicited_traffic(self) -> bool {
        matches!(self, BootState::Pending | BootState::Rejected)
    }
}

/// A handle to a call the application asked for.
///
/// Issued when the call is accepted — which may be long before it reaches the wire, since it
/// can sit in the offline queue — and reported back in [`CallOutcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallToken(u64);

impl fmt::Display for CallToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "call#{}", self.0)
    }
}

/// Per-call overrides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CallOptions {
    /// The CSMS asked for this message (`TriggerMessage`, `GetBaseReport`, …), so the boot
    /// gate does not apply to it (B02.FR.02).
    pub triggered: bool,
    /// Override whether the message is queued durably. Defaults to "yes for
    /// transaction-related messages".
    pub persist: Option<bool>,
    /// Override whether the message survives a disconnection. Defaults to the
    /// [`OfflinePolicy`].
    pub queue_when_offline: Option<bool>,
}

impl CallOptions {
    /// Marks the call as one the peer explicitly requested.
    #[must_use]
    pub const fn triggered() -> Self {
        Self {
            triggered: true,
            persist: None,
            queue_when_offline: None,
        }
    }

    /// Overrides whether the call is written to the durable [`MessageStore`].
    #[must_use]
    pub const fn persist(mut self, persist: bool) -> Self {
        self.persist = Some(persist);
        self
    }

    /// Overrides whether the call waits out a disconnection instead of failing.
    #[must_use]
    pub const fn queue_when_offline(mut self, queue: bool) -> Self {
        self.queue_when_offline = Some(queue);
        self
    }
}

/// Something the peer did that the specification forbids, but which is not fatal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolViolation {
    /// A `CALLRESULT` / `CALLERROR` arrived for an id that is not outstanding.
    UnexpectedResponse {
        /// The id that was answered.
        id: MessageId,
    },
    /// A message type number outside 2–6 (Part 4 §4.4).
    UnknownMessageType {
        /// The raw JSON of the type element.
        number: String,
    },
    /// The peer started a second `CALL` while one was still outstanding.
    ConcurrentCall {
        /// The id of the extra call.
        id: MessageId,
    },
    /// The peer sent an action defined as a `SEND` as a `CALL`, or the other way round
    /// (N15.FR.01).
    WrongMessageKind {
        /// The action name.
        action: String,
        /// The kind it arrived as.
        received: MessageKind,
    },
    /// The peer sent an action it is not allowed to originate.
    WrongDirection {
        /// The action name.
        action: String,
    },
    /// The peer used a `MessageId` longer than 36 characters.
    NonConformingMessageId {
        /// The id, kept verbatim so responses still correlate.
        id: MessageId,
    },
    /// The peer reused a `MessageId` that is still unanswered (Part 4 §4.1.4, §4.2.3).
    DuplicateMessageId {
        /// The id that was reused.
        id: MessageId,
    },
    /// The peer has more unanswered `CALL`s than
    /// [`EngineConfig::max_peer_requests`] allows.
    TooManyPeerRequests {
        /// The id of the call that went over the limit.
        id: MessageId,
    },
    /// The frame could not be parsed at all.
    MalformedFrame {
        /// The parse failure.
        error: FrameError,
    },
    /// The durable store failed.
    StoreFailure {
        /// The store's complaint.
        error: StoreError,
    },
    /// A frame this peer wanted to send could not be serialized.
    ///
    /// Only reachable from a payload the application supplied, since everything else the
    /// engine builds is known-good JSON.
    UnserializableFrame {
        /// What `serde_json` said.
        reason: String,
    },
}

/// Why the engine wants the connection closed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// A graceful drain finished.
    Drained,
    /// The drain deadline passed with work still queued.
    DrainTimedOut,
    /// The application asked for it.
    Requested,
}

/// A request the peer sent that the application must handle.
#[derive(Clone, Debug)]
pub struct IncomingRequest {
    /// Correlation id. Pass it back to [`Engine::respond`].
    pub id: MessageId,
    /// The action name, already checked against the version and the direction.
    pub action: String,
    /// The still-unparsed payload.
    pub payload: Box<RawValue>,
    /// `Call` (answer it) or `Send` (do not).
    pub kind: MessageKind,
}

/// Why a call did not succeed.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CallFailure {
    /// The peer answered `CALLERROR`.
    Rejected(CallError),
    /// No answer arrived before [`EngineConfig::call_timeout`].
    Timeout,
    /// The connection went away and the message was not queued.
    Disconnected,
    /// A transaction message used up its attempts and was skipped (1.6 §3.7.1).
    RetriesExhausted,
    /// The offline queue is full.
    QueueFull,
    /// The application withdrew the call before it was transmitted.
    Cancelled,
    /// The engine is draining and refused a new call.
    ShuttingDown,
}

impl fmt::Display for CallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallFailure::Rejected(error) => write!(f, "peer answered {error}"),
            CallFailure::Timeout => f.write_str("no answer before the message timeout"),
            CallFailure::Disconnected => f.write_str("connection lost"),
            CallFailure::RetriesExhausted => f.write_str("transaction message attempts exhausted"),
            CallFailure::QueueFull => f.write_str("offline queue is full"),
            CallFailure::Cancelled => f.write_str("cancelled before transmission"),
            CallFailure::ShuttingDown => f.write_str("engine is draining"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CallFailure {}

/// How a message the application started ended, when it ended well.
///
/// A `CALL` succeeds when the peer answers; a `SEND` when it is written, because Part 4
/// §4.2.4 forbids the peer from ever answering one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Answer {
    /// The peer's `CALLRESULT` payload.
    Result(Box<RawValue>),
    /// A `SEND` reached the wire. There is nothing further to wait for.
    Sent,
}

impl Answer {
    /// The peer's payload, or `None` for a `SEND`.
    #[must_use]
    pub fn payload(&self) -> Option<&RawValue> {
        match self {
            Answer::Result(payload) => Some(payload),
            Answer::Sent => None,
        }
    }

    /// Unwraps the peer's payload, or `None` for a `SEND`.
    #[must_use]
    pub fn into_payload(self) -> Option<Box<RawValue>> {
        match self {
            Answer::Result(payload) => Some(payload),
            Answer::Sent => None,
        }
    }
}

/// The final result of a call the application started.
#[derive(Clone, Debug)]
pub struct CallOutcome {
    /// The handle returned by [`Engine::call`].
    pub token: CallToken,
    /// The `MessageId` it was sent with, if it ever reached the wire.
    pub id: Option<MessageId>,
    /// The action.
    pub action: String,
    /// How it ended.
    pub result: Result<Answer, CallFailure>,
}

/// An authoritative timestamp observed in a `BootNotificationResponse` or
/// `HeartbeatResponse`.
///
/// A station whose `ClockCtrlr.TimeSource` is `Heartbeat` uses this to discipline its clock;
/// `at` is the monotonic instant the answer arrived, so the application can subtract the
/// round-trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSample {
    /// The CSMS's `currentTime`.
    pub csms_time: DateTime,
    /// When the answer was processed, on the driver's monotonic clock.
    pub at: Instant,
}

/// What the driver must do next.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Output {
    /// Write this UTF-8 text frame to the WebSocket.
    Transmit(String),
    /// The peer sent a request. Answer a `Call` with [`Engine::respond`]; a `Send` needs no
    /// answer and must not get one.
    Request(IncomingRequest),
    /// One of our calls finished.
    Outcome(CallOutcome),
    /// The peer rejected a `CALLRESULT` we sent, with `CALLRESULTERROR` (OCPP 2.1).
    ResultRejected {
        /// The id of the result the peer could not use.
        id: MessageId,
        /// Why.
        error: CallError,
    },
    /// Arm a timer. Re-arming an already-armed timer replaces it.
    SetTimer {
        /// Which timer.
        timer: Timer,
        /// When it should fire, on the driver's monotonic clock.
        at: Instant,
    },
    /// Disarm a timer.
    ClearTimer(Timer),
    /// The boot state changed (Charging Station side), or was decided (CSMS side).
    BootState(BootState),
    /// A `currentTime` was observed.
    ClockSample(ClockSample),
    /// The peer misbehaved. Never fatal by itself; log it and carry on.
    Violation(ProtocolViolation),
    /// Close the connection.
    Close(CloseReason),
}

/// Events the driver feeds into the engine, via [`Engine::handle`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum Input<'a> {
    /// The WebSocket handshake completed with this negotiated version.
    Connected {
        /// The subprotocol the peer selected.
        version: Version,
    },
    /// One complete text frame arrived.
    Received(&'a str),
    /// The connection went away.
    Disconnected,
    /// Nothing happened but time. Every timer whose deadline has passed fires.
    Timeout,
}

/// Why a command was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineError {
    /// This version does not define the action.
    UnknownAction(String),
    /// This peer is not allowed to originate the action.
    WrongDirection(String),
    /// A `SEND` was requested on a version that has no `SEND` (before 2.1).
    SendNotSupported(String),
    /// The action is a `SEND` but was submitted as a call, or the other way round.
    WrongMessageKind(String),
    /// No outstanding request has that id.
    NoSuchRequest(MessageId),
    /// `CALLRESULTERROR` needs OCPP 2.1.
    CallResultErrorNotSupported,
    /// The engine is draining.
    ShuttingDown,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::UnknownAction(action) => write!(f, "unknown action {action:?}"),
            EngineError::WrongDirection(action) => {
                write!(f, "this peer may not originate {action:?}")
            }
            EngineError::SendNotSupported(action) => {
                write!(f, "{action:?} is a SEND, which needs OCPP 2.1")
            }
            EngineError::WrongMessageKind(action) => {
                write!(f, "{action:?} was submitted with the wrong message kind")
            }
            EngineError::NoSuchRequest(id) => write!(f, "no outstanding request with id {id}"),
            EngineError::CallResultErrorNotSupported => {
                f.write_str("CALLRESULTERROR needs OCPP 2.1")
            }
            EngineError::ShuttingDown => f.write_str("engine is draining"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EngineError {}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Pending {
    token: CallToken,
    action: String,
    payload: Box<RawValue>,
    kind: MessageKind,
    transactional: bool,
    attempts: u32,
    /// Sequence number in the durable store, when persisted.
    seq: Option<Seq>,
    /// Not eligible for transmission before this instant (retry back-off).
    not_before: Option<Instant>,
    triggered: bool,
    queue_when_offline: bool,
}

#[derive(Debug)]
struct InFlight {
    id: MessageId,
    token: CallToken,
    action: String,
    transactional: bool,
    attempts: u32,
    seq: Option<Seq>,
    payload: Box<RawValue>,
    triggered: bool,
    queue_when_offline: bool,
    deadline: Instant,
}

/// Minimal view of a `BootNotificationResponse`, the one payload the engine looks inside.
///
/// The three members it needs have the same names and meanings in 1.6 and 2.x, so one shape
/// covers every version. Nothing else is parsed, and a payload that does not match is
/// ignored rather than treated as an error — the application still sees it in full.
#[derive(Deserialize)]
struct BootAnswer<'a> {
    status: &'a str,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    #[serde(rename = "currentTime")]
    current_time: Option<DateTime>,
}

#[derive(Deserialize)]
struct HeartbeatAnswer {
    #[serde(rename = "currentTime")]
    current_time: DateTime,
}

#[derive(Deserialize)]
struct TriggerRequest<'a> {
    #[serde(default)]
    #[serde(rename = "requestedMessage")]
    requested_message: Option<&'a str>,
}

/// The sans-I/O OCPP protocol engine.
///
/// See the [module documentation](self) for the rules it enforces.
pub struct Engine<S: MessageStore = MemStore> {
    config: EngineConfig,
    store: S,
    ids: Box<dyn IdGenerator + Send>,
    outputs: VecDeque<Output>,
    queue: VecDeque<Pending>,
    inflight: Option<InFlight>,
    /// The ids of the `CALL`s the peer is waiting on.
    ///
    /// Normally at most one — that is the rule — but [`InboundConcurrency::Serve`] means a
    /// peer that breaks it is answered anyway, and every one of its calls still has to be
    /// matched to its answer.
    peer_inflight: BTreeSet<MessageId>,
    connected: bool,
    version: Version,
    now: Instant,
    next_token: u64,
    boot: BootState,
    heartbeat_interval: Option<Duration>,
    /// Actions the CSMS asked this station for, which therefore survive the boot gate
    /// (B02.FR.09).
    solicited: BTreeSet<String>,
    draining: Option<Instant>,
    boot_retry_at: Option<Instant>,
    heartbeat_at: Option<Instant>,
}

impl Engine<MemStore> {
    /// An engine with an in-memory queue.
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        Self::with_store(config, MemStore::new()).expect("MemStore never fails")
    }
}

impl<S: MessageStore> Engine<S> {
    /// An engine backed by a durable [`MessageStore`].
    ///
    /// Anything the store still holds is loaded into the queue, so a Charging Station
    /// replays the transaction messages a power cut interrupted.
    pub fn with_store(config: EngineConfig, store: S) -> Result<Self, StoreError> {
        let version = config.version;
        let mut engine = Self {
            config,
            store,
            ids: default_ids(),
            outputs: VecDeque::new(),
            queue: VecDeque::new(),
            inflight: None,
            peer_inflight: BTreeSet::new(),
            connected: false,
            version,
            now: Instant::ZERO,
            next_token: 0,
            boot: BootState::Idle,
            heartbeat_interval: None,
            solicited: BTreeSet::new(),
            draining: None,
            boot_retry_at: None,
            heartbeat_at: None,
        };
        for (seq, entry) in engine.store.pending()? {
            let token = engine.mint_token();
            engine.queue.push_back(Pending {
                token,
                action: entry.action,
                payload: entry.payload,
                kind: entry.kind,
                transactional: entry.transactional,
                attempts: entry.attempts,
                seq: Some(seq),
                not_before: None,
                triggered: false,
                queue_when_offline: true,
            });
        }
        Ok(engine)
    }

    /// Changes the message timeout.
    ///
    /// 2.x sources it from the active `NetworkConnectionProfile.messageTimeout`, falling back
    /// to `OCPPCommCtrlr.MessageTimeout[Default]` — so it changes when the station fails over
    /// to a different network configuration slot, and it must be changeable at run time.
    /// A call that is already outstanding keeps the deadline it was given.
    pub fn set_call_timeout(&mut self, timeout: Duration) {
        self.config.call_timeout = timeout;
    }

    /// Replaces the [`IdGenerator`].
    ///
    /// A Local Controller uses this to guarantee that the ids it invents cannot collide with
    /// the CSMS's (Part 4 §6.4).
    pub fn set_id_generator(&mut self, ids: Box<dyn IdGenerator + Send>) {
        self.ids = ids;
    }

    /// The configuration this engine was built with.
    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The negotiated version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Whether the transport is currently up.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// The boot state machine's current state.
    #[must_use]
    pub fn boot_state(&self) -> BootState {
        self.boot
    }

    /// The heartbeat interval the CSMS asked for, once boot succeeded.
    #[must_use]
    pub fn heartbeat_interval(&self) -> Option<Duration> {
        self.heartbeat_interval
    }

    /// How many messages are waiting to be sent.
    ///
    /// This is what `GetTransactionStatus.messagesInQueue` reports.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The durable store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Whether one of *our* calls is currently outstanding.
    #[must_use]
    pub fn has_outstanding_call(&self) -> bool {
        self.inflight.is_some()
    }

    /// How many of the *peer's* calls are waiting for an answer.
    ///
    /// More than one means the peer broke the one-outstanding-`CALL` rule and
    /// [`InboundConcurrency::Serve`] let it.
    #[must_use]
    pub fn awaiting_response(&self) -> usize {
        self.peer_inflight.len()
    }

    // -- driver interface ---------------------------------------------------

    /// Feeds one event into the state machine at time `now`.
    ///
    /// `now` is the driver's monotonic clock, taken on every input rather than only on
    /// [`Input::Timeout`]: every input can arm a deadline, and one computed from a stale
    /// clock fires the moment it is armed.
    pub fn handle(&mut self, now: Instant, input: Input<'_>) {
        self.advance(now);
        match input {
            Input::Connected { version } => self.on_connected(version),
            Input::Received(text) => self.on_received(text),
            Input::Disconnected => self.on_disconnected(),
            Input::Timeout => self.on_timeout(),
        }
    }

    /// Moves the clock forward, never backwards: an older instant must not resurrect a
    /// deadline that has already passed.
    fn advance(&mut self, now: Instant) {
        if now > self.now {
            self.now = now;
        }
    }

    /// The engine's view of the driver's clock.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.now
    }

    /// Takes the next thing the driver must do.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    /// Takes everything the driver must do, in order.
    pub fn drain(&mut self) -> Vec<Output> {
        self.outputs.drain(..).collect()
    }

    // -- application interface ----------------------------------------------

    /// Starts a `CALL` (or a `SEND`, if that is what the action is).
    ///
    /// The call is accepted immediately and reaches the wire when the one-outstanding rule,
    /// the boot gate and the connection allow it. The returned [`CallToken`] appears again
    /// in the matching [`CallOutcome`].
    pub fn call(
        &mut self,
        now: Instant,
        action: &str,
        payload: Box<RawValue>,
    ) -> Result<CallToken, EngineError> {
        self.call_with(now, action, payload, CallOptions::default())
    }

    /// Starts a call with per-call overrides.
    pub fn call_with(
        &mut self,
        now: Instant,
        action: &str,
        payload: Box<RawValue>,
        options: CallOptions,
    ) -> Result<CallToken, EngineError> {
        self.advance(now);
        if self.draining.is_some() {
            return Err(EngineError::ShuttingDown);
        }
        let Some(kind) = actions::kind(self.version, action) else {
            return Err(EngineError::UnknownAction(action.to_string()));
        };
        let origin = actions::origin(self.version, action).unwrap_or(Origin::Both);
        if !allows(origin, self.config.role.sends()) {
            return Err(EngineError::WrongDirection(action.to_string()));
        }
        if kind == MessageKind::Send && !self.version.has_extended_message_types() {
            return Err(EngineError::SendNotSupported(action.to_string()));
        }

        let transactional = actions::is_transaction_related(self.version, action);
        let persist = options.persist.unwrap_or(transactional);
        let queue_when_offline = options
            .queue_when_offline
            .unwrap_or(transactional || self.config.offline.queue_all_messages);

        let token = self.mint_token();
        if !self.connected && !queue_when_offline {
            // Nothing about this message survives an outage, and there is no outage to wait
            // out — reporting that now beats letting it sit in a queue it was never meant to
            // enter until the *next* disconnection notices it.
            self.fail(token, None, action, CallFailure::Disconnected);
            return Ok(token);
        }
        if self.queue.len() >= self.config.offline.max_queued {
            self.fail(token, None, action, CallFailure::QueueFull);
            return Ok(token);
        }

        let mut seq = None;
        if persist {
            let entry = QueuedCall {
                action: action.to_string(),
                payload: payload.clone(),
                kind,
                attempts: 0,
                transactional,
            };
            match self.store.push(&entry) {
                Ok(value) => seq = Some(value),
                Err(error) => {
                    self.outputs
                        .push_back(Output::Violation(ProtocolViolation::StoreFailure { error }));
                    self.fail(token, None, action, CallFailure::QueueFull);
                    return Ok(token);
                }
            }
        }

        // The CSMS records what it asked the station for, so the boot gate can tell a
        // solicited follow-up call from an unsolicited one (B02.FR.09).
        if self.config.role == Role::Csms {
            self.remember_solicited(action, &payload);
        }

        self.queue.push_back(Pending {
            token,
            action: action.to_string(),
            payload,
            kind,
            transactional,
            attempts: 0,
            seq,
            not_before: None,
            triggered: options.triggered,
            queue_when_offline,
        });
        self.pump();
        Ok(token)
    }

    /// Answers an outstanding request with a `CALLRESULT`.
    pub fn respond(
        &mut self,
        now: Instant,
        id: &MessageId,
        payload: &RawValue,
    ) -> Result<(), EngineError> {
        self.advance(now);
        self.take_peer_request(id)?;
        // The CSMS side learns the boot verdict from the answer it is sending.
        if self.config.role == Role::Csms {
            self.observe_boot_answer(payload, /* outgoing */ true);
        }
        let frame = Frame::CallResult {
            id: id.clone(),
            payload: alloc::borrow::Cow::Borrowed(payload),
        };
        self.transmit(&frame);
        Ok(())
    }

    /// Answers an outstanding request with a `CALLERROR`.
    pub fn respond_error(
        &mut self,
        now: Instant,
        id: &MessageId,
        error: CallError,
    ) -> Result<(), EngineError> {
        self.advance(now);
        self.take_peer_request(id)?;
        let frame = Frame::CallError {
            id: id.clone(),
            error: error.into(),
        };
        self.transmit(&frame);
        Ok(())
    }

    /// Tells the peer that the `CALLRESULT` it sent could not be used (OCPP 2.1
    /// `CALLRESULTERROR`).
    ///
    /// Before 2.1 no such message exists, and the failure can only be logged locally.
    pub fn reject_result(
        &mut self,
        now: Instant,
        id: &MessageId,
        error: CallError,
    ) -> Result<(), EngineError> {
        self.advance(now);
        if !self.version.has_extended_message_types() {
            return Err(EngineError::CallResultErrorNotSupported);
        }
        let frame = Frame::CallResultError {
            id: id.clone(),
            error: error.into(),
        };
        self.transmit(&frame);
        Ok(())
    }

    /// Abandons a call that has not been transmitted yet.
    ///
    /// Returns `true` if it was still in the queue. A call that is already outstanding
    /// cannot be withdrawn — the peer will answer it.
    pub fn cancel(&mut self, token: CallToken) -> bool {
        let Some(index) = self.queue.iter().position(|entry| entry.token == token) else {
            return false;
        };
        let entry = self.queue.remove(index).expect("index from position");
        self.ack_store(entry.seq);
        self.outputs.push_back(Output::Outcome(CallOutcome {
            token,
            id: None,
            action: entry.action,
            result: Err(CallFailure::Cancelled),
        }));
        true
    }

    /// Starts a graceful drain: no new calls are accepted, the outstanding call is allowed
    /// to finish and the queue is flushed, then [`Output::Close`] is emitted.
    ///
    /// If `deadline` passes first, the connection is closed with
    /// [`CloseReason::DrainTimedOut`] and the queue is left in the durable store.
    pub fn shutdown(&mut self, now: Instant, deadline: Instant) {
        self.advance(now);
        if self.draining.is_some() {
            return;
        }
        self.draining = Some(deadline);
        self.outputs.push_back(Output::SetTimer {
            timer: Timer::DrainDeadline,
            at: deadline,
        });
        self.check_drain();
    }

    // -- inbound -------------------------------------------------------------

    fn on_connected(&mut self, version: Version) {
        self.version = version;
        self.connected = true;
        self.peer_inflight.clear();
        // The engine survives reconnects (Part 4 §5.4: no repeat `BootNotification`), so an
        // already-accepted station has to pick its heartbeat back up here — nothing else
        // will re-arm it.
        self.arm_heartbeat();
        self.pump();
    }

    fn on_disconnected(&mut self) {
        self.connected = false;
        self.peer_inflight.clear();
        self.outputs
            .push_back(Output::ClearTimer(Timer::CallTimeout));
        self.outputs.push_back(Output::ClearTimer(Timer::Heartbeat));
        self.heartbeat_at = None;

        if let Some(inflight) = self.inflight.take() {
            self.requeue_or_fail(inflight, CallFailure::Disconnected);
        }
        // Messages that must not survive the outage are failed now, in order.
        let mut kept = VecDeque::with_capacity(self.queue.len());
        while let Some(entry) = self.queue.pop_front() {
            if entry.queue_when_offline {
                kept.push_back(entry);
            } else {
                self.ack_store(entry.seq);
                self.outputs.push_back(Output::Outcome(CallOutcome {
                    token: entry.token,
                    id: None,
                    action: entry.action,
                    result: Err(CallFailure::Disconnected),
                }));
            }
        }
        self.queue = kept;
        self.check_drain();
    }

    fn on_timeout(&mut self) {
        let now = self.now;

        if let Some(deadline) = self.inflight.as_ref().map(|f| f.deadline) {
            if now >= deadline {
                let inflight = self.inflight.take().expect("checked");
                self.outputs
                    .push_back(Output::ClearTimer(Timer::CallTimeout));
                self.requeue_or_fail(inflight, CallFailure::Timeout);
            }
        }

        if let Some(at) = self.heartbeat_at {
            if now >= at {
                self.heartbeat_at = None;
                if self.connected && self.boot.allows_traffic() {
                    self.send_heartbeat();
                }
                // Sending re-arms the idle timer by way of `transmit`; this covers the cases
                // where nothing was sent, or the `Heartbeat` had to queue behind a call.
                self.arm_heartbeat();
            }
        }

        if let Some(at) = self.boot_retry_at {
            if now >= at {
                self.boot_retry_at = None;
                // B02.FR.04 / FR.08: the station re-sends BootNotification once the interval
                // has passed. Only the application can build the payload, so the engine
                // reports the transition and re-opens the gate.
                self.boot = BootState::Idle;
                self.outputs.push_back(Output::BootState(BootState::Idle));
            }
        }

        if let Some(deadline) = self.draining {
            if now >= deadline && !self.drain_complete() {
                self.outputs
                    .push_back(Output::Close(CloseReason::DrainTimedOut));
                self.draining = None;
                return;
            }
        }

        self.pump();
        self.check_drain();
    }

    fn on_received(&mut self, text: &str) {
        // `HeartbeatInterval` is defined as an interval of *inactivity*, so traffic in either
        // direction postpones the next `Heartbeat`.
        self.arm_heartbeat();
        let frame = match Frame::parse(text, self.version) {
            Ok(frame) => frame,
            Err(error) => {
                let reply = error.reply(self.version);
                let reply_id = error.reply_id();
                let code = error.error_code();
                // One violation per frame: the specific one when the frame got far enough to
                // name what was wrong with it, the general one otherwise.
                self.outputs.push_back(Output::Violation(match &error {
                    FrameError::UnknownMessageType { number } => {
                        ProtocolViolation::UnknownMessageType {
                            number: number.clone(),
                        }
                    }
                    _ => ProtocolViolation::MalformedFrame { error },
                }));
                // §4.2.3 — a CALL gets a CALLERROR, a CALLRESULT gets a CALLRESULTERROR, and
                // a SEND or a broken error frame gets nothing at all.
                let answer = match reply {
                    FrameReply::Ignore => return,
                    FrameReply::CallError => Frame::CallError {
                        id: reply_id,
                        error: CallError::new(code, "frame could not be processed").into(),
                    },
                    FrameReply::CallResultError => Frame::CallResultError {
                        id: reply_id,
                        error: CallError::new(code, "result could not be processed").into(),
                    },
                };
                self.transmit(&answer);
                return;
            }
        };

        if !frame.id().is_conforming() {
            self.outputs.push_back(Output::Violation(
                ProtocolViolation::NonConformingMessageId {
                    id: frame.id().clone(),
                },
            ));
        }

        match frame {
            Frame::Call {
                id,
                action,
                payload,
            } => {
                self.on_call(id, &action, payload.into_owned(), MessageKind::Call);
            }
            Frame::Send {
                id,
                action,
                payload,
            } => {
                self.on_call(id, &action, payload.into_owned(), MessageKind::Send);
            }
            Frame::CallResult { id, payload } => self.on_call_result(&id, payload.into_owned()),
            Frame::CallError { id, error } => self.on_call_error(&id, &error),
            Frame::CallResultError { id, error } => {
                self.outputs.push_back(Output::ResultRejected {
                    id,
                    error: error.to_call_error(),
                });
            }
        }
    }

    fn on_call(
        &mut self,
        id: MessageId,
        action: &str,
        payload: Box<RawValue>,
        received: MessageKind,
    ) {
        let Some(expected) = actions::kind(self.version, action) else {
            self.reject(&id, received, CallError::not_implemented(action));
            return;
        };
        if expected != received {
            // N15.FR.01: a SEND-only action must not arrive as a CALL, and a CALL must not
            // arrive as a SEND.
            self.outputs
                .push_back(Output::Violation(ProtocolViolation::WrongMessageKind {
                    action: action.to_string(),
                    received,
                }));
            self.reject(
                &id,
                received,
                CallError::new(
                    ErrorCode::ProtocolError,
                    format!("{action} is a {expected:?} message, not a {received:?} message"),
                ),
            );
            return;
        }

        let origin = actions::origin(self.version, action).unwrap_or(Origin::Both);
        if !allows(origin, self.config.role.receives()) {
            self.outputs
                .push_back(Output::Violation(ProtocolViolation::WrongDirection {
                    action: action.to_string(),
                }));
            self.reject(&id, received, CallError::not_supported(action));
            return;
        }

        if received == MessageKind::Call {
            // §4.2.3 lists "an existing message with the same unique identifier is being
            // handled already" as a CALLERROR condition — and answering it as an ordinary
            // request would make the two answers indistinguishable to the peer.
            if self.peer_inflight.contains(&id) {
                self.outputs
                    .push_back(Output::Violation(ProtocolViolation::DuplicateMessageId {
                        id: id.clone(),
                    }));
                self.reject(
                    &id,
                    received,
                    CallError::new(
                        ErrorCode::RpcFrameworkError,
                        "a CALL with this MessageId is already being handled (Part 4 §4.2.3)",
                    ),
                );
                return;
            }
            if !self.peer_inflight.is_empty() {
                self.outputs
                    .push_back(Output::Violation(ProtocolViolation::ConcurrentCall {
                        id: id.clone(),
                    }));
                // Serving extra calls is a kindness to non-conforming peers, not an
                // invitation to hold unbounded state on their behalf.
                let over_limit = self.peer_inflight.len() >= self.config.max_peer_requests;
                if over_limit {
                    self.outputs.push_back(Output::Violation(
                        ProtocolViolation::TooManyPeerRequests { id: id.clone() },
                    ));
                }
                if over_limit || self.config.inbound_concurrency == InboundConcurrency::Reject {
                    self.reject(
                        &id,
                        received,
                        CallError::new(
                            ErrorCode::ProtocolError,
                            "a CALL is already outstanding in this direction (Part 4 §4.1.1)",
                        ),
                    );
                    return;
                }
            }
        }

        // B01.FR.10 / B02.FR.09 — a CSMS answers an unsolicited call with SecurityError once
        // it has told the station it is *not* accepted. The precondition is "the Charging
        // Station has received a BootNotificationResponse with a status other than
        // Accepted", so it does **not** apply before any BootNotification has been answered:
        // Part 4 §5.4 explicitly lets a station reconnect without repeating one.
        // BootNotification itself is always allowed, and so is anything the CSMS asked for.
        if self.config.role == Role::Csms
            && self.config.enforce_boot_gate
            && self.boot.blocks_unsolicited_traffic()
            && action != "BootNotification"
            && !self.solicited.contains(action)
        {
            self.reject(
                &id,
                received,
                CallError::security(format!(
                    "{action} is not allowed while the charging station is {:?} (B02.FR.09)",
                    self.boot
                )),
            );
            return;
        }

        if self.config.role == Role::Csms && action == "BootNotification" {
            self.boot = BootState::Idle;
        }

        if received == MessageKind::Call {
            self.peer_inflight.insert(id.clone());
        }
        self.outputs.push_back(Output::Request(IncomingRequest {
            id,
            action: action.to_string(),
            payload,
            kind: received,
        }));
    }

    fn on_call_result(&mut self, id: &MessageId, payload: Box<RawValue>) {
        let Some(inflight) = self.take_inflight(id) else {
            self.outputs
                .push_back(Output::Violation(ProtocolViolation::UnexpectedResponse {
                    id: id.clone(),
                }));
            return;
        };
        self.ack_store(inflight.seq);
        self.outputs
            .push_back(Output::ClearTimer(Timer::CallTimeout));

        match inflight.action.as_str() {
            "BootNotification" if self.config.role == Role::ChargingStation => {
                self.observe_boot_answer(&payload, false);
            }
            "Heartbeat" => {
                if let Ok(answer) = serde_json::from_str::<HeartbeatAnswer>(payload.get()) {
                    self.outputs.push_back(Output::ClockSample(ClockSample {
                        csms_time: answer.current_time,
                        at: self.now,
                    }));
                }
            }
            _ => {}
        }

        self.outputs.push_back(Output::Outcome(CallOutcome {
            token: inflight.token,
            id: Some(inflight.id),
            action: inflight.action,
            result: Ok(Answer::Result(payload)),
        }));
        self.pump();
        self.check_drain();
    }

    fn on_call_error(&mut self, id: &MessageId, error: &CallErrorRef<'_>) {
        let Some(inflight) = self.take_inflight(id) else {
            self.outputs
                .push_back(Output::Violation(ProtocolViolation::UnexpectedResponse {
                    id: id.clone(),
                }));
            return;
        };
        // A CALLERROR is a definitive answer, so it is not retried — unlike a timeout or a
        // dropped connection, retrying it would just produce the same error.
        self.ack_store(inflight.seq);
        self.outputs
            .push_back(Output::ClearTimer(Timer::CallTimeout));
        self.outputs.push_back(Output::Outcome(CallOutcome {
            token: inflight.token,
            id: Some(inflight.id),
            action: inflight.action,
            result: Err(CallFailure::Rejected(error.to_call_error())),
        }));
        self.pump();
        self.check_drain();
    }

    // -- helpers -------------------------------------------------------------

    fn mint_token(&mut self) -> CallToken {
        let token = CallToken(self.next_token);
        self.next_token += 1;
        token
    }

    fn take_inflight(&mut self, id: &MessageId) -> Option<InFlight> {
        match self.inflight.as_ref() {
            Some(inflight) if &inflight.id == id => self.inflight.take(),
            _ => None,
        }
    }

    fn take_peer_request(&mut self, id: &MessageId) -> Result<(), EngineError> {
        if self.peer_inflight.remove(id) {
            return Ok(());
        }
        Err(EngineError::NoSuchRequest(id.clone()))
    }

    fn reject(&mut self, id: &MessageId, kind: MessageKind, error: CallError) {
        // FR.07 — a SEND is never answered, not even to report that it was unusable.
        if kind == MessageKind::Send {
            return;
        }
        let frame = Frame::CallError {
            id: id.clone(),
            error: error.into(),
        };
        self.transmit(&frame);
    }

    fn transmit(&mut self, frame: &Frame<'_>) {
        match frame.to_json(self.version) {
            Ok(text) => {
                self.outputs.push_back(Output::Transmit(text));
                self.arm_heartbeat();
            }
            Err(error) => {
                self.outputs
                    .push_back(Output::Violation(ProtocolViolation::StoreFailure {
                        error: StoreError::new(format!("frame could not be serialized: {error}")),
                    }));
            }
        }
    }

    fn ack_store(&mut self, seq: Option<Seq>) {
        if let Some(seq) = seq {
            if let Err(error) = self.store.ack(seq) {
                self.outputs
                    .push_back(Output::Violation(ProtocolViolation::StoreFailure { error }));
            }
        }
    }

    fn fail(&mut self, token: CallToken, id: Option<MessageId>, action: &str, why: CallFailure) {
        self.outputs.push_back(Output::Outcome(CallOutcome {
            token,
            id,
            action: action.to_string(),
            result: Err(why),
        }));
    }

    /// Decides what happens to a call that lost its answer: a transaction message goes back
    /// on the queue for its next scheduled attempt; anything else fails now.
    ///
    /// Only transaction messages are replayed, and deliberately so. They are the ones the
    /// specification makes idempotent — a CSMS deduplicates `TransactionEvent` by
    /// `transactionId` and `seqNo` — so a second copy is harmless. Re-sending an
    /// `Authorize` or a `SetVariables` whose answer merely went missing is not obviously
    /// safe, and the caller, which knows what the message meant, gets to decide.
    fn requeue_or_fail(&mut self, inflight: InFlight, why: CallFailure) {
        let attempts = inflight.attempts;
        if inflight.transactional && self.config.retry.may_retry(attempts) {
            let delay = self.config.retry.delay_after(attempts);
            let not_before = self.now.saturating_add(delay);
            if let Some(seq) = inflight.seq {
                if let Err(error) = self.store.set_attempts(seq, attempts) {
                    self.outputs
                        .push_back(Output::Violation(ProtocolViolation::StoreFailure { error }));
                }
            }
            self.queue.push_front(Pending {
                token: inflight.token,
                action: inflight.action,
                payload: inflight.payload,
                kind: MessageKind::Call,
                transactional: true,
                attempts,
                seq: inflight.seq,
                not_before: Some(not_before),
                triggered: inflight.triggered,
                queue_when_offline: inflight.queue_when_offline,
            });
            self.outputs.push_back(Output::SetTimer {
                timer: Timer::TransactionRetry,
                at: not_before,
            });
            return;
        }
        let why = if inflight.transactional {
            CallFailure::RetriesExhausted
        } else {
            why
        };
        self.ack_store(inflight.seq);
        self.outputs.push_back(Output::Outcome(CallOutcome {
            token: inflight.token,
            id: Some(inflight.id),
            action: inflight.action,
            result: Err(why),
        }));
    }

    /// Whether the entry at `index` may go out right now.
    ///
    /// Two rules decide it. B02.FR.02 gates everything before the CSMS accepts the station.
    /// And 1.6 §3.7 — carried into 2.x — says transaction-related messages are delivered *in
    /// chronological order*: "the delivery of new transaction-related messages SHALL wait
    /// until the queue has been emptied". So a transaction message may only go out if it is
    /// the oldest one queued, even when a later one is due and it is still waiting out its
    /// retry interval. Messages that are *not* transaction-related are explicitly allowed to
    /// overtake the queue, so that an `Authorize` is not held up behind a stuck meter value.
    fn eligible(&self, index: usize) -> bool {
        let entry = &self.queue[index];
        if entry.not_before.is_some_and(|at| self.now < at) {
            return false;
        }
        if entry.transactional {
            let oldest = self
                .queue
                .iter()
                .position(|other| other.transactional)
                .expect("this entry is transactional");
            if oldest != index {
                return false;
            }
        }
        if !self.config.enforce_boot_gate || self.config.role != Role::ChargingStation {
            return true;
        }
        // B02.FR.02 — before the CSMS accepts the station, only BootNotification and
        // messages the CSMS explicitly asked for may be sent.
        self.boot.allows_traffic() || entry.action == "BootNotification" || entry.triggered
    }

    fn pump(&mut self) {
        if !self.connected {
            return;
        }

        // Part 4 §4.2.4 — a SEND is exempt from the one-outstanding-CALL rule, so it goes
        // out whether or not a CALL is in flight, and completes as soon as it is written.
        while let Some(index) = (0..self.queue.len())
            .find(|index| self.queue[*index].kind == MessageKind::Send && self.eligible(*index))
        {
            let entry = self.queue.remove(index).expect("index from position");
            let id = self.ids.next_id();
            let frame = Frame::Send {
                id: id.clone(),
                action: (&entry.action).into(),
                payload: alloc::borrow::Cow::Borrowed(entry.payload.as_ref()),
            };
            self.transmit(&frame);
            self.ack_store(entry.seq);
            self.outputs.push_back(Output::Outcome(CallOutcome {
                token: entry.token,
                id: Some(id),
                action: entry.action,
                result: Ok(Answer::Sent),
            }));
        }

        while self.inflight.is_none() {
            let Some(index) = (0..self.queue.len()).find(|index| {
                self.queue[*index].kind == MessageKind::Call && self.eligible(*index)
            }) else {
                break;
            };
            let entry = self.queue.remove(index).expect("index from position");
            let id = self.ids.next_id();
            let frame = Frame::Call {
                id: id.clone(),
                action: (&entry.action).into(),
                payload: alloc::borrow::Cow::Borrowed(entry.payload.as_ref()),
            };
            self.transmit(&frame);
            let deadline = self.now.saturating_add(self.config.call_timeout);
            self.outputs.push_back(Output::SetTimer {
                timer: Timer::CallTimeout,
                at: deadline,
            });
            self.inflight = Some(InFlight {
                id,
                token: entry.token,
                action: entry.action,
                transactional: entry.transactional,
                attempts: entry.attempts + 1,
                seq: entry.seq,
                payload: entry.payload,
                triggered: entry.triggered,
                queue_when_offline: entry.queue_when_offline,
                deadline,
            });
        }
    }

    fn send_heartbeat(&mut self) {
        // `HeartbeatRequest` is an empty object in 1.6 and 2.x alike.
        let now = self.now;
        let _ = self.call_with(
            now,
            "Heartbeat",
            empty_payload(),
            CallOptions::default().queue_when_offline(false),
        );
    }

    /// (Re-)arms the inactivity timer that triggers the next `Heartbeat`.
    ///
    /// `OCPPCommCtrlr.HeartbeatInterval` is the interval of inactivity after which a station
    /// sends `Heartbeat`, so this is called for every frame in either direction — not once
    /// per period. A `Heartbeat` that times out or is answered with a `CALLERROR` therefore
    /// cannot stop the sequence, which is what a fixed schedule anchored on the response
    /// would do.
    fn arm_heartbeat(&mut self) {
        if self.config.heartbeat != HeartbeatPolicy::Automatic
            || self.config.role != Role::ChargingStation
        {
            return;
        }
        let Some(interval) = self.heartbeat_interval.filter(|d| !d.is_zero()) else {
            return;
        };
        let at = self.now.saturating_add(interval);
        if self.heartbeat_at == Some(at) {
            return;
        }
        self.heartbeat_at = Some(at);
        self.outputs.push_back(Output::SetTimer {
            timer: Timer::Heartbeat,
            at,
        });
    }

    /// Reads `status`, `interval` and `currentTime` out of a `BootNotificationResponse`.
    fn observe_boot_answer(&mut self, payload: &RawValue, outgoing: bool) {
        let Ok(answer) = serde_json::from_str::<BootAnswer<'_>>(payload.get()) else {
            return;
        };
        let state = match answer.status {
            "Accepted" => BootState::Accepted,
            "Pending" => BootState::Pending,
            "Rejected" => BootState::Rejected,
            _ => return,
        };
        self.boot = state;
        self.outputs.push_back(Output::BootState(state));

        if let Some(time) = answer.current_time {
            if !outgoing {
                self.outputs.push_back(Output::ClockSample(ClockSample {
                    csms_time: time,
                    at: self.now,
                }));
            }
        }

        let interval = answer.interval.map(Duration::from_secs);
        match state {
            BootState::Accepted => {
                self.heartbeat_interval = interval.filter(|d| !d.is_zero());
                if !outgoing {
                    self.arm_heartbeat();
                    self.pump();
                }
            }
            BootState::Pending | BootState::Rejected if !outgoing => {
                // B02.FR.04 / FR.07 / FR.08: wait `interval` before retrying, or a
                // locally chosen back-off when the CSMS said 0.
                let wait = interval
                    .filter(|d| !d.is_zero())
                    .unwrap_or(self.config.boot_retry_fallback);
                let at = self.now.saturating_add(wait);
                self.boot_retry_at = Some(at);
                self.outputs.push_back(Output::SetTimer {
                    timer: Timer::BootRetry,
                    at,
                });
            }
            _ => {}
        }
    }

    /// Remembers the follow-up calls a CSMS request licenses the station to make.
    fn remember_solicited(&mut self, action: &str, payload: &RawValue) {
        if action == "TriggerMessage" || action == "ExtendedTriggerMessage" {
            if let Ok(trigger) = serde_json::from_str::<TriggerRequest<'_>>(payload.get()) {
                if let Some(requested) = trigger.requested_message {
                    self.solicited.insert(requested.to_string());
                    // 1.6 and 2.x both name the message, not the action, so a couple of
                    // aliases are needed.
                    if requested == "SignChargePointCertificate" {
                        self.solicited.insert("SignCertificate".to_string());
                    }
                }
            }
        }
        for follow_up in actions::solicited_by(self.version, action) {
            self.solicited.insert((*follow_up).to_string());
        }
    }

    fn drain_complete(&self) -> bool {
        self.inflight.is_none() && self.queue.is_empty()
    }

    fn check_drain(&mut self) {
        if self.draining.is_some() && self.drain_complete() {
            self.draining = None;
            self.outputs
                .push_back(Output::ClearTimer(Timer::DrainDeadline));
            self.outputs.push_back(Output::Close(CloseReason::Drained));
        }
    }
}

/// The best [`IdGenerator`] the build offers.
///
/// Random ids when an entropy source is available, because Part 4 §4.1.4 requires ids to be
/// unique across *every* connection under one identity, not just within one. Without one,
/// a counter — which the caller must then prefix per boot; see
/// [`CounterIds`](crate::types::CounterIds).
fn default_ids() -> Box<dyn IdGenerator + Send> {
    #[cfg(feature = "getrandom")]
    {
        Box::new(crate::types::RandomIds::new())
    }
    #[cfg(not(feature = "getrandom"))]
    {
        Box::new(crate::types::CounterIds::default())
    }
}

fn allows(origin: Origin, side: Origin) -> bool {
    match side {
        Origin::ChargingStation => origin.from_charging_station(),
        Origin::Csms => origin.from_csms(),
        Origin::Both => true,
    }
}

fn empty_payload() -> Box<RawValue> {
    RawValue::from_string("{}".to_string()).expect("`{}` is valid JSON")
}
