//! A recording driver for the sans-I/O engine.

use alloc::string::String;
use alloc::vec::Vec;

use crate::engine::{
    CallFailure, CallOutcome, Engine, IncomingRequest, Instant, MessageStore, Output,
    ProtocolViolation, Timer,
};

/// Drains an [`Engine`] and answers focused questions about what it produced.
///
/// The sans-I/O shape is what makes every timing rule in OCPP testable in microseconds, but
/// it hands back a `Vec<Output>` that a test then has to sift. This does the sifting.
///
/// Most tests want [`Sim`](super::Sim), which owns the clock as well and drains into one of
/// these after every step. Reach for `Recorder` directly when the engine is driven by
/// something else.
///
/// ```
/// use ocpp_kit::Version;
/// use ocpp_kit::engine::{Engine, EngineConfig, Input, Instant, Role};
/// use ocpp_kit::testkit::Recorder;
/// use serde_json::value::RawValue;
///
/// let mut engine = Engine::new(EngineConfig::new(Role::ChargingStation, Version::V2_1));
/// engine.handle(Instant::ZERO, Input::Connected { version: Version::V2_1 });
///
/// let payload = RawValue::from_string(r#"{"reason":"PowerUp"}"#.into()).unwrap();
/// engine.call(Instant::ZERO, "BootNotification", payload).unwrap();
///
/// let mut recorder = Recorder::new();
/// recorder.drain(&mut engine);
/// assert!(recorder.only_frame().contains("BootNotification"));
/// ```
#[derive(Debug, Default)]
pub struct Recorder {
    outputs: Vec<Output>,
}

impl Recorder {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes everything the engine currently wants done, replacing what was recorded before.
    ///
    /// Each call is one "turn", so a test reads as a sequence of stimulus and response rather
    /// than as one growing pile.
    pub fn drain<S: MessageStore>(&mut self, engine: &mut Engine<S>) -> &mut Self {
        self.outputs = engine.drain();
        self
    }

    /// Everything recorded in the last [`drain`](Self::drain), in order.
    #[must_use]
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    /// The OCPP-J text frames the engine wants written.
    #[must_use]
    pub fn frames(&self) -> Vec<&str> {
        self.outputs
            .iter()
            .filter_map(|output| match output {
                Output::Transmit(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The single frame the engine wants written.
    ///
    /// # Panics
    ///
    /// If there was not exactly one — which in a test is the assertion you wanted anyway, and
    /// the message names what it found instead.
    #[must_use]
    pub fn only_frame(&self) -> &str {
        let frames = self.frames();
        assert_eq!(
            frames.len(),
            1,
            "expected exactly one frame, found {frames:?}"
        );
        frames[0]
    }

    /// The requests the peer sent, for the application to answer.
    #[must_use]
    pub fn requests(&self) -> Vec<&IncomingRequest> {
        self.outputs
            .iter()
            .filter_map(|output| match output {
                Output::Request(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// The calls that finished, successfully or not.
    #[must_use]
    pub fn outcomes(&self) -> Vec<&CallOutcome> {
        self.outputs
            .iter()
            .filter_map(|output| match output {
                Output::Outcome(outcome) => Some(outcome),
                _ => None,
            })
            .collect()
    }

    /// Why each call that failed did.
    #[must_use]
    pub fn failures(&self) -> Vec<(&str, &CallFailure)> {
        self.outcomes()
            .into_iter()
            .filter_map(|outcome| {
                outcome
                    .result
                    .as_ref()
                    .err()
                    .map(|failure| (outcome.action.as_str(), failure))
            })
            .collect()
    }

    /// The rules the peer broke. None is fatal on its own.
    #[must_use]
    pub fn violations(&self) -> Vec<&ProtocolViolation> {
        self.outputs
            .iter()
            .filter_map(|output| match output {
                Output::Violation(violation) => Some(violation),
                _ => None,
            })
            .collect()
    }

    /// The timers the engine armed, and when they should fire.
    #[must_use]
    pub fn timers(&self) -> Vec<(Timer, Instant)> {
        self.outputs
            .iter()
            .filter_map(|output| match output {
                Output::SetTimer { timer, at } => Some((*timer, *at)),
                _ => None,
            })
            .collect()
    }

    /// When `timer` was last armed for, if it was.
    #[must_use]
    pub fn timer(&self, timer: Timer) -> Option<Instant> {
        self.timers()
            .into_iter()
            .rev()
            .find(|(armed, _)| *armed == timer)
            .map(|(_, at)| at)
    }

    /// The `MessageId` of the single frame the engine wants written.
    ///
    /// The id is generated inside the engine, so a test that wants to answer a call has no
    /// other way to learn it.
    ///
    /// # Panics
    ///
    /// If there was not exactly one frame, or it is not a JSON array with a string id.
    #[must_use]
    pub fn only_frame_id(&self) -> String {
        let frame = self.only_frame();
        let elements: Vec<&serde_json::value::RawValue> =
            serde_json::from_str(frame).unwrap_or_else(|_| panic!("{frame} is not a JSON array"));
        let id = elements
            .get(1)
            .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok());
        id.unwrap_or_else(|| panic!("{frame} has no string MessageId"))
    }
}
