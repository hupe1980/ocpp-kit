//! Test scaffolding for code built on this crate (feature `testkit`).
//!
//! What this crate tests itself with:
//!
//! * [`Sim`] — an [`Engine`](crate::engine::Engine) plus the clock a driver would own, so a
//!   test reads as a transcript: `connect`, `call`, `advance(Duration::from_secs(31))`.
//! * [`Recorder`] — the question-answering half of it, which lets a test ask what came out
//!   instead of matching over a `Vec<Output>` by hand.
//! * [`MockCsms`] and [`MockStation`] — a working peer on a loopback port, in one line, that
//!   records every action it was asked for.
//!
//! # Not here
//!
//! No JSON Schema validator: the schemas check the *generated types* in CI, and a payload
//! built out of those types is conformant by construction. No scenario file format either —
//! with [`Sim`] a transcript is a handful of statements in a `#[test]`.
//!
//! Put `ocpp-kit` with `features = ["testkit"]` in `[dev-dependencies]`, not
//! `[dependencies]`.

mod recorder;
mod sim;

pub use recorder::Recorder;
pub use sim::Sim;

#[cfg(feature = "tokio")]
mod mock;

#[cfg(feature = "tokio")]
pub use mock::{Exchange, MockCsms, MockStation};
