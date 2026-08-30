//! # ocpp-kit
//!
//! An Open Charge Point Protocol (OCPP) toolkit for Rust, targeting OCPP 1.6J,
//! 2.0.1 and 2.1 over JSON/WebSocket (OCPP-J).
//!
//! **This is a placeholder release that reserves the crate name.** It contains
//! no functionality yet. The planned scope is:
//!
//! - typed, validated message payloads generated from the official JSON schemas,
//! - OCPP-J framing (`CALL`, `CALLRESULT`, `CALLERROR`, `CALLRESULTERROR`, `SEND`),
//! - a sans-I/O protocol engine usable from any runtime (including `no_std` + `alloc`),
//! - Tokio/TLS transports for charging stations, CSMS and local controllers.
//!
//! Follow progress at <https://github.com/hupe1980/ocpp-kit>.
#![no_std]
#![forbid(unsafe_code)]

/// Crate version, as reserved on crates.io.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
