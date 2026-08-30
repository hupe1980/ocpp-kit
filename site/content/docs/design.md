+++
title = "Design decisions"
description = "Why one crate rather than five, why generated code is committed, why f64 for numbers and rustls only — and what this crate deliberately does not do."
weight = 150
+++

The trade-offs behind the shape of the crate, and the boundaries of what it is for.

## Decisions

**One crate, many features.** Separate `-types` / `-rpc` / `-tokio` crates drift out of step
with each other, and a version skew between them is the kind of bug that only appears at a
customer site. The cost is compile time for types-only users, kept down by leaving tokio, TLS
and the domain blocks out of the default feature set.

**Generated code is committed; no `build.rs`.** Building needs neither the schemas nor a
network, `docs.rs` works, and a schema change arrives as a readable diff instead of a silent
change in behaviour. CI fails if the committed code and the schemas disagree.

**Schemas vendored, PDFs not.** `schemas/` is redistributed unmodified with a `NOTICE`; the
specification documents are not redistributable and are not in the repository.

**Sans-I/O over async-native.** It costs a little ergonomics and buys testability, `no_std`,
WebAssembly, and one implementation of the protocol rules for every runtime — including the
three roles, which would otherwise be three copies of the same state machine.

**Separate type sets per version.** 26 of the 128 shared 2.0.1 schema files changed in 2.1.
Sharing types between the versions would silently mis-validate 2.0.1.

**`jiff` for time, `f64` for numbers.** `f64` is what the schemas say (`"type": "number"`), and
every OCPP number is a sensor reading — watts, amperes, watt-hours. Money in the 2.1 Tariff
block is the exception: work in the smallest currency unit and convert at the boundary. See
[messages](@/docs/messages.md).

**Validation separate from `serde`.** Running constraints inside `Deserialize` makes every
failure an unstructured `serde` error. A second explicit pass is what makes the
[spec-exact error codes](@/docs/framing.md) possible, and what leaves a cheap parse-only path
for a Local Controller that only needs to relay.

**`rustls` only.** Two TLS stacks means two attack surfaces and two configuration models. No
`native-tls`.

**Leniency is a bounded repair loop.** The strict parse runs first; only on failure is the
offending member rewritten — identified by path — and the parse retried, up to `max_repairs`.
Conforming traffic pays exactly one strict parse. See [interop](@/docs/interop.md).

## What this crate is not

**OCPP-S (SOAP).** Deprecated by the Open Charge Alliance and dead in the field.

**A CSMS product.** Billing, roaming, OCPI, a UI. This is the protocol and the reusable
primitives underneath one; products are built on top.

**ISO 15118.** Its messages are carried opaquely, exactly as OCPP itself does.

**A companion `-macros` crate** for requirement-traceability attributes. `cargo xtask coverage`
reads the citations out of comments and test names, which costs one crate and a proc-macro
dependency less.

**A scenario file format.** With [`testkit::Sim`](@/docs/testing.md) a transcript is a handful
of statements in a `#[test]` — no format to learn and no parser to maintain.

## Feature flags that do not exist yet

`chrono` / `time` conversions, `rust_decimal`, `schemars`, an `axum` integration. Each is a
permanent maintenance surface, none has a user asking for it, and every one can be added later
without a breaking change. A feature flag is a promise; an unused one is a promise for nothing.
