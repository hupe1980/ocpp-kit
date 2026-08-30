//! A simulated driver for the sans-I/O engine.

use alloc::boxed::Box;
use alloc::string::String;
use core::time::Duration;

use serde_json::value::RawValue;

use crate::engine::{
    CallOptions, CallToken, Engine, EngineConfig, EngineError, Input, Instant, MemStore,
    MessageStore, Output, Timer,
};
use crate::rpc::CallError;
use crate::types::MessageId;
use crate::version::Version;

use super::Recorder;

/// An [`Engine`] plus the clock a driver would own, so a test reads as a transcript.
///
/// It moves the clock with [`advance`](Self::advance) and drains the engine into a
/// [`Recorder`] after every step, so a test does not thread an [`Instant`] by hand.
///
/// ```
/// use core::time::Duration;
/// use ocpp_kit::Version;
/// use ocpp_kit::engine::{EngineConfig, Role};
/// use ocpp_kit::testkit::Sim;
///
/// let mut sim = Sim::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
/// sim.connect(Version::V2_1);
/// sim.call("BootNotification", r#"{"reason":"PowerUp"}"#).unwrap();
/// assert!(sim.only_frame().contains("BootNotification"));
///
/// // Simulated time: the message timeout arrives in microseconds, not thirty seconds.
/// sim.advance(Duration::from_secs(31));
/// assert_eq!(sim.failures().len(), 1);
/// ```
pub struct Sim<S: MessageStore = MemStore> {
    engine: Engine<S>,
    recorder: Recorder,
    now: Instant,
    /// The timer map a real driver keeps, so `advance_to_next_timer` sees what is armed now.
    timers: alloc::collections::BTreeMap<Timer, Instant>,
}

impl Sim<MemStore> {
    /// A simulation over an engine with an in-memory queue, its clock at zero.
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        Self::with_engine(Engine::new(config))
    }
}

impl<S: MessageStore> Sim<S> {
    /// A simulation over an engine the caller built — with a durable store, say.
    #[must_use]
    pub fn with_engine(engine: Engine<S>) -> Self {
        Self {
            engine,
            recorder: Recorder::new(),
            now: Instant::ZERO,
            timers: alloc::collections::BTreeMap::new(),
        }
    }

    /// The engine underneath, for the questions [`Recorder`] does not answer.
    pub fn engine(&self) -> &Engine<S> {
        &self.engine
    }

    /// The engine underneath, mutably.
    pub fn engine_mut(&mut self) -> &mut Engine<S> {
        &mut self.engine
    }

    /// The simulated clock.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.now
    }

    // -- stimulus ------------------------------------------------------------

    /// The WebSocket handshake completed.
    pub fn connect(&mut self, version: Version) -> &mut Self {
        self.engine.handle(self.now, Input::Connected { version });
        self.turn()
    }

    /// The connection went away.
    pub fn disconnect(&mut self) -> &mut Self {
        self.engine.handle(self.now, Input::Disconnected);
        self.turn()
    }

    /// One text frame arrived from the peer.
    pub fn recv(&mut self, text: &str) -> &mut Self {
        self.engine.handle(self.now, Input::Received(text));
        self.turn()
    }

    /// Moves the clock forward and fires whatever became due.
    pub fn advance(&mut self, by: Duration) -> &mut Self {
        self.advance_to(self.now.saturating_add(by))
    }

    /// Moves the clock to an absolute instant and fires whatever became due.
    ///
    /// The clock never moves backwards, matching the engine's own guarantee: an earlier
    /// instant is ignored rather than silently putting the two out of step.
    pub fn advance_to(&mut self, at: Instant) -> &mut Self {
        if at > self.now {
            self.now = at;
        }
        self.engine.handle(self.now, Input::Timeout);
        self.turn()
    }

    /// Moves the clock to the earliest armed timer, so the next thing due happens.
    ///
    /// Returns `false` when nothing is armed, so a loop over it walks a whole session's
    /// timing without guessing intervals.
    pub fn advance_to_next_timer(&mut self) -> bool {
        let Some(at) = self.timers.values().min().copied() else {
            return false;
        };
        self.advance_to(at);
        true
    }

    // -- application ---------------------------------------------------------

    /// Starts a call, with the payload written as JSON text.
    ///
    /// # Panics
    ///
    /// If `payload` is not valid JSON — which in a test is a typo, not a condition to handle.
    pub fn call(&mut self, action: &str, payload: &str) -> Result<CallToken, EngineError> {
        self.call_with(action, payload, CallOptions::default())
    }

    /// Starts a call with per-call overrides.
    ///
    /// # Panics
    ///
    /// If `payload` is not valid JSON.
    pub fn call_with(
        &mut self,
        action: &str,
        payload: &str,
        options: CallOptions,
    ) -> Result<CallToken, EngineError> {
        let token = self
            .engine
            .call_with(self.now, action, raw(payload), options);
        self.turn();
        token
    }

    /// Answers an outstanding request with a `CALLRESULT`.
    ///
    /// # Panics
    ///
    /// If `payload` is not valid JSON.
    pub fn respond(&mut self, id: &MessageId, payload: &str) -> Result<(), EngineError> {
        let result = self.engine.respond(self.now, id, &raw(payload));
        self.turn();
        result
    }

    /// Answers an outstanding request with a `CALLERROR`.
    pub fn respond_error(&mut self, id: &MessageId, error: CallError) -> Result<(), EngineError> {
        let result = self.engine.respond_error(self.now, id, error);
        self.turn();
        result
    }

    /// Starts a graceful drain that must finish within `deadline`.
    pub fn shutdown(&mut self, deadline: Duration) -> &mut Self {
        let at = self.now.saturating_add(deadline);
        self.engine.shutdown(self.now, at);
        self.turn()
    }

    // -- observation ---------------------------------------------------------

    /// Drains the engine into the recorder, discarding the previous step's outputs.
    ///
    /// Every stimulus method does this already; call it directly only after driving the
    /// engine through [`engine_mut`](Self::engine_mut).
    pub fn turn(&mut self) -> &mut Self {
        self.recorder.drain(&mut self.engine);
        for output in self.recorder.outputs() {
            match output {
                Output::SetTimer { timer, at } => {
                    self.timers.insert(*timer, *at);
                }
                Output::ClearTimer(timer) => {
                    self.timers.remove(timer);
                }
                _ => {}
            }
        }
        // A timer that expired without being re-armed is gone, as it is for the Tokio driver.
        let now = self.now;
        self.timers.retain(|_, at| *at > now);
        self
    }

    /// What the last step produced.
    #[must_use]
    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// Every timer currently armed, and when it is due.
    ///
    /// [`Recorder::timers`] reports only what the *last step* armed; this is the driver's map.
    #[must_use]
    pub fn armed(&self) -> alloc::vec::Vec<(Timer, Instant)> {
        self.timers.iter().map(|(t, at)| (*t, *at)).collect()
    }

    /// When `timer` is due, if it is armed.
    #[must_use]
    pub fn armed_at(&self, timer: Timer) -> Option<Instant> {
        self.timers.get(&timer).copied()
    }
}

/// Forwards the questions a test asks most often, so `sim.only_frame()` reads as well as
/// `sim.recorder().only_frame()` does.
impl<S: MessageStore> core::ops::Deref for Sim<S> {
    type Target = Recorder;

    fn deref(&self) -> &Recorder {
        &self.recorder
    }
}

/// Parses a JSON payload written inline in a test.
///
/// # Panics
///
/// If the text is not valid JSON.
fn raw(payload: &str) -> Box<RawValue> {
    RawValue::from_string(String::from(payload))
        .unwrap_or_else(|error| panic!("{payload} is not valid JSON: {error}"))
}
