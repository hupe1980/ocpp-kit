+++
title = "The ocpp-cli tool"
description = "Validate a payload, explain an OCPP-J frame, replay a capture in CI, browse the action catalogue, and run a mock CSMS or charging station."
weight = 130
+++

```console
$ cargo install ocpp-kit --features cli
```

A small tool for the things you end up doing by hand: reading a capture, checking a payload,
and standing up a peer to talk to.

## `actions` — the catalogue

```console
$ ocpp-cli actions --version 2.1 --block S
ACTION                               BLOCK                    KIND    DIRECTION
BatterySwap                          S                        CALL    CS  -> CSMS
RequestBatterySwap                   S                        CALL    CSMS -> CS

2 action(s) in OCPP 2.1
```

`--block` filters by functional block (2.x: `A`–`S`) or feature profile (1.6: `Core`,
`Security`, …).

## `validate` — check one payload

```console
$ echo '{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}' \
    | ocpp-cli validate --action BootNotification
{
  "chargingStation": {
    "model": "M1",
    "vendorName": "ACME"
  },
  "reason": "PowerUp"
}
ok: valid BootNotificationRequest for OCPP 2.1
```

What it prints is what the Rust types *modelled*, so a member that came out missing is a member
the types do not carry. When it fails, it names the code you should have answered with:

```console
$ echo '{"reason":"PowerUp","chargingStation":{"model":"a-model-name-that-is-far-too-long","vendorName":"ACME"}}' \
    | ocpp-cli validate --action BootNotification
error: /chargingStation/model: maxLength 20 exceeded (got 33 characters)
  OCPP error code: PropertyConstraintViolation
  path: /chargingStation/model
```

Add `--response` for a response payload, `--version 1.6` for another version, and `--lenient` /
`--pedantic` to change the [decoding policy](@/docs/interop.md).

## `frame` — explain one OCPP-J frame

```console
$ echo '[2,"1","BootNotification",{"reason":"PowerUp","chargingStation":{"model":"M1","vendorName":"ACME"}}]' \
    | ocpp-cli frame
CALL 1 BootNotification [ChargingStation] valid
```

The payload is checked against the types, not merely parsed.

## `replay` — check a whole capture

A capture is one frame per line; a leading `>` or `<` marking direction is ignored, and `#`
starts a comment.

```console
$ ocpp-cli replay session.ocppcap --version 2.1
    1: CALL 19223201 BootNotification [ChargingStation] valid
    2: CALLRESULT 19223201 (78 bytes) — the action is only known from the matching CALL
    3: CALL 19223202 StatusNotification [ChargingStation] INVALID: /timestamp: …

3 frame(s), 1 problem(s)
```

Exit status is non-zero when anything did not check out, so it drops into CI.

## `csms` and `station` — mock peers

```console
$ ocpp-cli csms --bind 127.0.0.1:9000
mock CSMS listening on 127.0.0.1:9000
station CS-0001 connecting from 127.0.0.1:54119 (security profile 1)
+ CS-0001 (OCPP 2.1)

$ ocpp-cli station --url ws://127.0.0.1:9000/ocpp --identity CS-0001
boot: Accepted, interval 300s
```

Both refuse every action they are asked for, which is the point: they exist to check
connectivity, negotiation and authentication, not to pretend to implement the protocol.

An unknown option is an error, as is one given without its value — a mistyped `--pedantic`
would otherwise report a payload as valid under rules you asked not to use.

## In a checkout

The same family of question, answered by `cargo xtask`:

```console
$ cargo xtask coverage                       # requirement IDs cited by the source and tests
$ cargo xtask coverage --block B02           # …just one block
$ cargo xtask coverage --profile core        # how much of a certification profile is driven
$ cargo xtask schema-report                  # actions, enums and types per version and block
```
