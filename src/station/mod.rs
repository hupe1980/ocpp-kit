//! Layer 4 — reusable Charging Station building blocks (feature `station`).
//!
//! These are components, not a framework. Each one owns a piece of OCPP that is fiddly
//! enough to be worth getting right once — the device model, the transaction start/stop
//! rules, the local authorization order, the composite-schedule calculation — and none of
//! them assume anything about how the rest of your firmware is organised. Take the ones you
//! want.

pub mod authorization;
pub mod configuration;
pub mod device_model;
pub mod smart_charging;
pub mod transactions;
