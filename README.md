# ocpp-kit

[![crates.io](https://img.shields.io/crates/v/ocpp-kit.svg)](https://crates.io/crates/ocpp-kit)
[![docs.rs](https://docs.rs/ocpp-kit/badge.svg)](https://docs.rs/ocpp-kit)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

An [Open Charge Point Protocol](https://openchargealliance.org/) (OCPP) toolkit for Rust —
**OCPP 1.6J, 2.0.1 and 2.1** over JSON/WebSocket.

> **Status: placeholder.** Version `0.0.0` reserves the crate name and contains no
> functionality yet. Implementation is in progress.

## Planned scope

| Layer | What |
|---|---|
| Types | Typed, validated payloads for every action, generated from the official OCA JSON schemas (39 / 64 / 91 actions) |
| RPC | OCPP-J framing: `CALL`, `CALLRESULT`, `CALLERROR`, `CALLRESULTERROR`, `SEND`; spec-exact error codes; signed messages (JWS) |
| Engine | Sans-I/O protocol state machine — request/response correlation, one-outstanding-call rule, timeouts, transaction-message retries, offline queue, boot state machine; `no_std` + `alloc` |
| Transport | Tokio + WebSocket + rustls for **charging stations**, **CSMS** and **local controllers**; security profiles 1/2/3; permessage-deflate |
| Domain | Opt-in building blocks: device model, transaction state machine, smart-charging composite schedules, local authorization list, certificate management |

## JSON schemas

The OCPP JSON schemas in [`schemas/`](schemas/) are published by the Open Charge Alliance and are
redistributed unmodified; see [`schemas/NOTICE`](schemas/NOTICE). They are not covered by this
crate's license.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
