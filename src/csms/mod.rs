//! Layer 4 — reusable CSMS building blocks (feature `csms`).
//!
//! The two pieces every CSMS ends up writing, and every CSMS gets subtly wrong the first
//! time: an **idempotent transaction ledger** (because stations legitimately re-send events)
//! and a **version-agnostic view** of what a station said (because supporting 1.6, 2.0.1 and
//! 2.1 should not mean three copies of the business logic).

pub mod events;
pub mod ledger;
