+++
title = "WebSocket and compression"
description = "Why ocpp-kit ships its own RFC 6455 implementation: OCPP 2.1 requires permessage-deflate of a CSMS, and a library without RFC 7692 must reject the RSV1 bit."
weight = 60
+++

Most crates that speak a protocol over WebSocket hand the WebSocket part to a library. This one
does not, and the reason is a single line of OCPP 2.1:

> **Table 2. WebSocket compression support requirement for devices**
> Charging Station: Optional · **CSMS: Required** · **Local Controller: Required**
> — Part 4 §3.4

RFC 7692 `permessage-deflate` marks a compressed message by setting the `RSV1` bit on its first
frame. A WebSocket library that does not implement the extension is *required* by RFC 6455
§5.2 to fail the connection when it sees a reserved bit set — and that is exactly what the
general-purpose Rust crates do, with no hook to change it. Supporting compression means owning
the frame layer.

So `ocpp_kit::transport::ws` is an RFC 6455 implementation, narrowed to what OCPP-J needs and
widened where the specification demands it.

## What it does

* **Framing** — all six opcodes, all three length forms, client-side masking, fragmentation and
  reassembly, control frames interleaved with a fragmented message.
* **Validation** — every rule RFC 6455 places on a frame, each with a test named after it: a
  server rejects an unmasked frame and a client a masked one, control frames must be short and
  unfragmented, reserved bits and opcodes are refused, a text payload must be UTF-8, a close
  code must be one a peer may actually send, lengths must be minimally encoded.
* **The handshake**, on both sides, because OCPP puts requirements on it a generic callback
  cannot meet — 404 for an unknown identity, 401 for bad credentials, and a *successful*
  handshake with no subprotocol header followed by an immediate close when there is no common
  version.
* **RFC 7692**, with context takeover, `no_context_takeover`, and decompression bounded as it
  inflates so a compressed payload cannot expand past the message limit. `RSV1` means
  "compressed", and only where §6 puts it: on the first frame of a data message. On a
  continuation or a control frame it is a reserved bit with no meaning, and is refused.
* **Liveness.** A ping that goes unanswered for the configured timeout ends the session, which
  is the only way Part 4 §5.3's end-to-end check actually checks anything: a connection a
  mobile network has dropped stays writable until the operating system's TCP timeout expires,
  and that can be minutes.

## How it is checked

Writing a WebSocket implementation is not a thing to do casually; the protocol has a long
history of framing bugs, and several of them are security bugs.

| | |
|---|---|
| **Unit tests** | ~40, one per rule: masking at every alignment, each length form, fragmentation, interleaved control frames, close-code validation, size limits, UTF-8, RSV bits |
| **Interop** | Every frame is put past `tokio-tungstenite` — an independent, widely used implementation — in both directions. Our server serves its client; our client talks to its server; a message it fragments, we reassemble. It is a **dev-dependency**: the reference, never the runtime. |
| **Fuzzing** | `cargo fuzz run websocket` drives the codec with arbitrary bytes, asserting that decoding always terminates, never panics, and that anything decoded survives a re-encode |
| **On the wire** | A test drives a raw socket, checks the CSMS answers `Sec-WebSocket-Extensions: permessage-deflate`, sends a compressed frame with `RSV1` set, and checks the answer comes back compressed too |

## Compression in practice

It is on by default wherever the specification asks for it, and negotiates the way RFC 7692
says:

```rust,no_run
use ocpp_kit::transport::{Csms, Station};

# fn build() -> Result<(), Box<dyn std::error::Error>> {
// A CSMS accepts the offer — it is required to.
let csms = Csms::builder()
    .bind("0.0.0.0:9000".parse()?)
    .compression(true)   // the default; `false` is for reading captures by eye
    .build()?;

// A station offers it — optional, but recommended.
let station = Station::builder()
    .identity("CS-0001")?
    .url("ws://csms.example.com/ocpp")
    .compression(true)
    .build();
# let _ = (csms, station);
# Ok(()) }
```

Two details that matter:

* **A short message is not compressed.** DEFLATE on a 40-byte `CALLRESULT` costs more bytes
  than it saves, so messages below a threshold go out with `RSV1` clear. A connection that has
  negotiated compression must still accept uncompressed messages, and this one does.
* **A CSMS that declines is not an error.** Part 4 §3.4 says a station that finds compression
  unused should *not* close the connection — "turning off compression can be very useful during
  development, testing and debugging".

Context takeover is where the ratio comes from. OCPP traffic is extremely repetitive: the same
action names, the same JSON keys, the same station identity, message after message. Carrying
the compressor's window between messages makes the second copy of a `TransactionEvent` compress
several times better than the first. `cargo bench` reports:

```text
ws/encode (masked)                time: [109.90 ns 110.34 ns 110.78 ns]
ws/decode (unmask)                time: [108.12 ns 108.67 ns 109.23 ns]
ws/encode (permessage-deflate)    time: [1.6525 µs 1.6568 µs 1.6613 µs]
permessage-deflate: 538 bytes -> 26 bytes (5% of the original)
```

A repeated `TransactionEvent` frame goes out at **5 % of its size** once the window has warmed
up. On a metered mobile connection carrying a meter value every 60 seconds, that is the
difference the specification is pointing at.

## What it deliberately does not do

* **It does not fragment on send.** One frame per message is legal, simpler, and what every
  OCPP implementation does. Incoming fragments are of course reassembled.
* **It does not narrow the DEFLATE window.** The pure-Rust DEFLATE backend does not expose the
  window size, so an offer whose `server_max_window_bits` this implementation cannot honour is
  *declined* — precisely what RFC 7692 §7.1.2.2 prescribes — rather than accepted and violated.
  A 15-bit decompressor reads any narrower stream, so the receive path is unconstrained.
* **It is not a general-purpose WebSocket crate.** It is text-first, single-purpose and lives
  behind `ocpp_kit::transport`. For WebSockets outside OCPP, use one of the general-purpose
  crates.
