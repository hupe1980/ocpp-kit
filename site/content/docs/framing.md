+++
title = "OCPP-J framing"
description = "CALL, CALLRESULT, CALLERROR, and OCPP 2.1's CALLRESULTERROR and SEND — parsed zero-copy, with the error-code spelling each version actually uses."
weight = 30
+++

`ocpp_kit::rpc` knows about message types, message ids and error codes. It knows nothing about
actions, payload schemas, sockets or state: `Frame` in, `Frame` out.

## The frame model

```rust
use ocpp_kit::Version;
use ocpp_kit::rpc::{Frame, MessageTypeId};

let text = r#"[2,"19223201","BootNotification",{"reason":"PowerUp"}]"#;
let frame = Frame::parse(text, Version::V2_1).unwrap();

assert_eq!(frame.message_type(), MessageTypeId::Call);
assert_eq!(frame.id().as_str(), "19223201");
assert_eq!(frame.action(), Some("BootNotification"));
assert_eq!(frame.payload().unwrap().get(), r#"{"reason":"PowerUp"}"#);
assert_eq!(frame.to_json(Version::V2_1).unwrap(), text);
```

Parsing is two-stage and zero-copy: the array skeleton is split into elements while the payload
stays a `RawValue`, and the payload is deserialized only once the action *and* the direction are
known. That is what lets a local controller relay a signed message untouched, and what keeps
the "unknown action" path from paying for a parse it will throw away.

## The five message types

| Type | Number | Shape | Since |
|---|---|---|---|
| `CALL` | 2 | `[2, id, action, payload]` | 1.6 |
| `CALLRESULT` | 3 | `[3, id, payload]` | 1.6 |
| `CALLERROR` | 4 | `[4, id, code, description, details]` | 1.6 |
| `CALLRESULTERROR` | 5 | `[5, id, code, description, details]` | **2.1** |
| `SEND` | 6 | `[6, id, action, payload]` | **2.1** |

`Frame::parse` takes the negotiated version and refuses a message type that version does not
define, so a 2.0.1 peer sending a `SEND` is a protocol error rather than a silent success.

## Error codes are version-aware

```rust
use ocpp_kit::Version;
use ocpp_kit::rpc::ErrorCode;

// 1.6J prints it with a single `r`, and calls a format violation something else entirely.
assert_eq!(ErrorCode::FormatViolation.as_wire(Version::V1_6), "FormationViolation");
assert_eq!(ErrorCode::FormatViolation.as_wire(Version::V2_1), "FormatViolation");
assert_eq!(
    ErrorCode::OccurrenceConstraintViolation.as_wire(Version::V1_6),
    "OccurenceConstraintViolation",
);

// Codes 1.6 does not define degrade rather than being invented.
assert_eq!(ErrorCode::RpcFrameworkError.as_wire(Version::V1_6), "GenericError");
assert!(!ErrorCode::RpcFrameworkError.is_defined_in(Version::V1_6));

// Parsing accepts either spelling on any version, because peers mix them up.
assert_eq!(ErrorCode::parse("FormationViolation"), ErrorCode::FormatViolation);
```

## Malformed frames

`FrameError` carries the three things a responder needs: which code to answer with, under which
id, and *what kind of answer* — because §4.2.3 does not want a `CALLERROR` for everything. A
broken `CALL` gets one; a broken `CALLRESULT` gets a `CALLRESULTERROR` on 2.1 and nothing
before it; a broken `SEND`, or a `CALLERROR` that is itself broken, gets nothing at all.

```rust
use ocpp_kit::Version;
use ocpp_kit::rpc::{ErrorCode, Frame, FrameReply};

// The MessageId is not a string, so it "could not be read" — Part 4 §4.1.1 says to answer
// with the literal id "-1".
let error = Frame::parse(r#"[2,123,"Heartbeat",{}]"#, Version::V2_1).unwrap_err();
assert_eq!(error.reply_id().as_str(), "-1");
assert_eq!(error.error_code(), ErrorCode::RpcFrameworkError);
assert_eq!(error.reply(Version::V2_1), FrameReply::CallError);

// The same defect in a CALLRESULT is answered differently, and only 2.1 can answer at all.
let error = Frame::parse(r#"[3,123,{}]"#, Version::V2_1).unwrap_err();
assert_eq!(error.reply(Version::V2_1), FrameReply::CallResultError);
assert_eq!(error.reply(Version::V2_0_1), FrameReply::Ignore);
```

An unknown message *type* number is the one case where the answer differs by version:

```rust
use ocpp_kit::Version;
use ocpp_kit::rpc::Frame;

let error = Frame::parse(r#"[7,"1","Whatever",{}]"#, Version::V2_1).unwrap_err();
assert!(error.is_ignorable(Version::V1_6));   // 1.6J §4.1.3 — ignore the payload
assert!(!error.is_ignorable(Version::V2_0_1)); // 2.0.1 §4.4 — answer MessageTypeNotSupported
assert!(error.is_ignorable(Version::V2_1));   // 2.1 §4.4 — back to ignoring it
```

## Message ids

An id longer than 36 characters violates the specification, but truncating it would break
correlation — the peer is waiting for its own id back. Parsing keeps it verbatim and flags it:

```rust
use ocpp_kit::types::MessageId;

let id = MessageId::from_wire(&"x".repeat(40));
assert_eq!(id.as_str().len(), 40);
assert!(!id.is_conforming());
```

Ids this peer *generates* go through `MessageId::new` or an `IdGenerator`, both of which
enforce the limit.

Uniqueness is the harder half of the rule. Part 4 §4.1.4 asks for an id that differs from every
id the same sender has used for a `CALL` or a `SEND` **on any connection under the same
Charging Station identity** — not merely within one connection. A counter that restarts at zero
after a power cut breaks that, and it breaks it on exactly the messages a station replays from
its offline queue afterwards. `types::RandomIds` (a version 4 UUID) is the default wherever an
entropy source is available; `types::CounterIds` is for targets without one and needs a prefix
that changes on every boot. Retransmitting a message under its original id is the one reuse
§4.1.4 explicitly allows.

The receiving side has a matching obligation. Part 4 §4.2.3 lists "an existing message with the
same unique identifier is being handled already" among the conditions that call for a
`CALLERROR`, and the [engine](@/docs/engine.md) answers a reused id with one rather than
dispatching the request twice — two answers under one id are indistinguishable to the sender.

## Signed messages

With the `signed-messages` feature, `rpc::signed` implements Part 4 chapter 7: the
`<Action>-Signed` wrapper, the flattened JWS JSON serialization, and the `OCPPAction` /
`OCPPMessageTypedId` / `x5t#S256` protected-header fields. See
[Security](@/docs/security.md#signed-messages).
