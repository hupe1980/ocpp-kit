#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![doc = include_str!("lib.md")]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[macro_use]
mod macros;

pub mod actions;
#[cfg(feature = "csms")]
pub mod csms;
mod decimal;
pub mod decode;
pub mod engine;
pub mod message;
pub mod metering;
pub mod rpc;
pub mod standard;
#[cfg(feature = "station")]
pub mod station;
#[cfg(feature = "testkit")]
pub mod testkit;
#[cfg(feature = "tokio")]
pub mod transport;
pub mod types;
pub mod validate;
pub mod version;

#[cfg(feature = "v1_6")]
pub mod v1_6;
#[cfg(feature = "v2_0_1")]
pub mod v2_0_1;
#[cfg(feature = "v2_1")]
pub mod v2_1;

pub use version::{Subprotocol, Version};

/// An unparsed JSON value.
///
/// Re-exported from `serde_json` so payloads can be handled without adding a `serde_json`
/// dependency of your own — and, more importantly, without the risk of linking a *different*
/// `serde_json` than this crate did.
pub use serde_json::value::RawValue;

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
