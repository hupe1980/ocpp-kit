//! Working peers on a loopback port, for integration tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::value::RawValue;
use tokio::net::TcpListener;

use crate::engine::IncomingRequest;
use crate::rpc::CallError;
use crate::transport::{
    Auth, AuthOutcome, BasicAuthPassword, BoxFuture, Csms, CsmsHandle, Ctx, Handle, Handler,
    SecurityProfile, Station, TransportError,
};
use crate::types::{DateTime, Identity};
use crate::version::Version;

/// One request a mock peer handled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    /// Which station it belongs to.
    pub identity: Identity,
    /// The action name.
    pub action: String,
    /// The payload, as JSON text.
    pub payload: String,
}

/// How a mock peer answers one request. `Err` becomes a `CALLERROR`.
pub type Answer = dyn Fn(&Ctx, &IncomingRequest) -> Result<Box<RawValue>, CallError> + Send + Sync;

/// Records what a mock peer was asked for, and answers it.
struct Recording {
    seen: Mutex<Vec<Exchange>>,
    identity: Identity,
    answer: Box<Answer>,
}

impl Handler for Recording {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        Box::pin(async move {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(Exchange {
                    identity: ctx.identity().clone(),
                    action: request.action.clone(),
                    payload: request.payload.get().to_owned(),
                });
            }
            (self.answer)(&ctx, &request)
        })
    }
}

/// The canned answers a mock gives, so a test does not have to write a handler to get a
/// working peer.
///
/// Everything else is answered `NotImplemented`, which is what a real peer would do and what
/// a test wants to notice.
fn default_answer(ctx: &Ctx, request: &IncomingRequest) -> Result<Box<RawValue>, CallError> {
    let now = DateTime::now().to_string();
    let body = match request.action.as_str() {
        "BootNotification" => {
            format!(r#"{{"currentTime":"{now}","interval":300,"status":"Accepted"}}"#)
        }
        "Heartbeat" => format!(r#"{{"currentTime":"{now}"}}"#),
        "Authorize" => r#"{"idTokenInfo":{"status":"Accepted"}}"#.to_owned(),
        "TransactionEvent"
        | "StatusNotification"
        | "NotifyEvent"
        | "NotifyReport"
        | "MeterValues"
        | "SecurityEventNotification"
        | "LogStatusNotification"
        | "FirmwareStatusNotification" => "{}".to_owned(),
        // Station side.
        "Reset" | "ChangeAvailability" | "UnlockConnector" | "TriggerMessage" | "SetVariables"
        | "GetVariables" | "ClearCache" => r#"{"status":"Accepted"}"#.to_owned(),
        _ => return Err(CallError::not_implemented(&request.action)),
    };
    let _ = ctx;
    RawValue::from_string(body)
        .map_err(|error| CallError::internal(format!("mock answer is not JSON: {error}")))
}

/// A CSMS on an ephemeral loopback port that answers the common actions and records what it
/// was sent.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use ocpp_kit::testkit::MockCsms;
///
/// let csms = MockCsms::start().await?;
/// // … point a station at `csms.url()` …
/// assert!(csms.saw("BootNotification"));
/// # Ok(()) }
/// ```
pub struct MockCsms {
    url: String,
    port: u16,
    handle: CsmsHandle,
    recording: Arc<Recording>,
}

impl MockCsms {
    /// Starts a CSMS on `127.0.0.1:0`, accepting every station and every supported version.
    pub async fn start() -> Result<Self, TransportError> {
        Self::builder().start().await
    }

    /// Configures a mock CSMS before starting it.
    #[must_use]
    pub fn builder() -> MockCsmsBuilder {
        MockCsmsBuilder {
            versions: vec![Version::V2_1, Version::V2_0_1, Version::V1_6],
            answer: None,
        }
    }

    /// The endpoint a station should connect to. The identity is appended by the station.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The port it is listening on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The live CSMS handle, for calling a connected station back.
    #[must_use]
    pub fn handle(&self) -> &CsmsHandle {
        &self.handle
    }

    /// Every request it has answered, oldest first.
    #[must_use]
    pub fn exchanges(&self) -> Vec<Exchange> {
        self.recording
            .seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// Whether it has been asked for `action`.
    #[must_use]
    pub fn saw(&self, action: &str) -> bool {
        self.exchanges()
            .iter()
            .any(|exchange| exchange.action == action)
    }

    /// Waits until it has been asked for `action`, or the deadline passes.
    ///
    /// Returns whether it arrived.
    pub async fn wait_for(&self, action: &str, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline {
            if self.saw(action) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.saw(action)
    }
}

/// Builds a [`MockCsms`].
pub struct MockCsmsBuilder {
    versions: Vec<Version>,
    answer: Option<Box<Answer>>,
}

impl MockCsmsBuilder {
    /// The versions to offer, most preferred first.
    #[must_use]
    pub fn versions(mut self, versions: impl IntoIterator<Item = Version>) -> Self {
        self.versions = versions.into_iter().collect();
        self
    }

    /// Replaces the canned answers.
    ///
    /// Return `Err(CallError::…)` to make the mock answer a `CALLERROR`, which is how a test
    /// exercises the failure paths a real CSMS produces.
    #[must_use]
    pub fn answer(
        mut self,
        answer: impl Fn(&Ctx, &IncomingRequest) -> Result<Box<RawValue>, CallError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.answer = Some(Box::new(answer));
        self
    }

    /// Binds and starts it.
    pub async fn start(self) -> Result<MockCsms, TransportError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let recording = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
            identity: Identity::new("mock-csms").expect("a valid identity"),
            answer: self.answer.unwrap_or_else(|| Box::new(default_answer)),
        });

        let csms = Csms::builder()
            .bind(addr)
            .versions(self.versions)
            .authenticate(|_: Auth| async { AuthOutcome::accept() })
            .handler(SharedHandler(recording.clone()))
            // A test should fail on its own assertions, not on a keepalive.
            .ping_interval(None)
            .build()?;
        let handle = csms.handle();
        tokio::spawn(async move {
            let _ = csms.serve_on(listener).await;
        });

        Ok(MockCsms {
            url: format!("ws://127.0.0.1:{}/ocpp", addr.port()),
            port: addr.port(),
            handle,
            recording,
        })
    }
}

/// A Charging Station connected to a CSMS, answering the common CSMS-initiated actions and
/// recording what it was sent.
///
/// It does **not** send `BootNotification` on its own: which payload to boot with is the
/// thing a test usually wants to control. Call [`boot`](Self::boot) for the ordinary one.
pub struct MockStation {
    handle: Handle,
    recording: Arc<Recording>,
}

impl MockStation {
    /// Connects a station with the given identity to `url`.
    ///
    /// Returns as soon as the session is spawned; the connection itself is established in the
    /// background, exactly as a real station's is. Await the first call to know it is up.
    pub fn connect(url: &str, identity: &str) -> Result<Self, TransportError> {
        let recording = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
            identity: Identity::new(identity)
                .map_err(|error| TransportError::Configuration(error.to_string()))?,
            answer: Box::new(default_answer),
        });
        let handle = Station::builder()
            .identity(identity)?
            .url(url)
            // Profile 1 is the only one that works over `ws://`, and A00.FR.203 makes the
            // password mandatory even there; the mock CSMS accepts whatever it is given.
            .security_profile(SecurityProfile::BasicAuth)
            .password(
                BasicAuthPassword::utf8("mock-station-secret")
                    .map_err(TransportError::Credential)?,
            )
            .ping_interval(None)
            .handler(SharedHandler(recording.clone()))
            .build()?
            .spawn()?;
        Ok(Self { handle, recording })
    }

    /// The handle, for calling the CSMS.
    #[must_use]
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// The identity it connected with.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.recording.identity
    }

    /// Sends an ordinary `BootNotification` and waits for the answer.
    ///
    /// Returns the raw response payload, so the same call works on every version.
    pub async fn boot(&self, version: Version) -> Result<Box<RawValue>, CallError> {
        let payload = match version {
            Version::V1_6 => r#"{"chargePointModel":"Model-1","chargePointVendor":"ACME"}"#,
            _ => {
                r#"{"reason":"PowerUp","chargingStation":{"model":"Model-1","vendorName":"ACME"}}"#
            }
        };
        let payload = RawValue::from_string(payload.to_owned())
            .map_err(|error| CallError::internal(error.to_string()))?;
        self.handle
            .call_raw(
                "BootNotification",
                payload,
                crate::engine::CallOptions::default(),
            )
            .await
            .map_err(|failure| match failure {
                crate::engine::CallFailure::Rejected(error) => error,
                other => CallError::internal(other.to_string()),
            })?
            .into_payload()
            .ok_or_else(|| CallError::internal("BootNotification is not a SEND"))
    }

    /// Every request it has answered, oldest first.
    #[must_use]
    pub fn exchanges(&self) -> Vec<Exchange> {
        self.recording
            .seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// Whether it has been asked for `action`.
    #[must_use]
    pub fn saw(&self, action: &str) -> bool {
        self.exchanges()
            .iter()
            .any(|exchange| exchange.action == action)
    }

    /// Drains and closes the session.
    pub async fn shutdown(&self, deadline: Duration) {
        self.handle.shutdown(deadline).await;
    }
}

/// Lets the mock keep a reference to the handler it installed.
struct SharedHandler(Arc<Recording>);

impl Handler for SharedHandler {
    fn on_request(
        &self,
        ctx: Ctx,
        request: IncomingRequest,
    ) -> BoxFuture<'_, Result<Box<RawValue>, CallError>> {
        self.0.on_request(ctx, request)
    }
}
