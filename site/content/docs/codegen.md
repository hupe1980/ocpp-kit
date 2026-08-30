+++
title = "Code generation"
description = "How src/v1_6, src/v2_0_1 and src/v2_1 are generated from the official OCA JSON schemas, why the output is committed, and the naming rules and schema defects the generator handles."
weight = 140
+++

`src/v1_6`, `src/v2_0_1` and `src/v2_1` are generated from the vendored OCA JSON schemas by
`cargo xtask codegen`, and the output is **committed**.

That is a deliberate trade. It means building `ocpp-kit` needs neither the schemas nor a
`build.rs` nor a network connection, `docs.rs` renders the real types, and a schema change
arrives as a readable diff instead of an invisible behaviour change.

```console
$ cargo xtask codegen
  v1_6: 39 actions, 48 enums, 11 shared types
 v2_0_1: 64 actions, 88 enums, 54 shared types
  v2_1: 91 actions, 110 enums, 121 shared types

$ cargo xtask codegen --check    # what CI runs
$ cargo xtask schema-report      # counts per version and per functional block
```

## What is generated

Per version, five files:

| File | Contents |
|---|---|
| `enums.rs` | every string enumeration, via the `ocpp_enum!` macro |
| `types.rs` | the data types shared between messages |
| `messages.rs` | one struct per request and response payload, with the `Request` / `Response` impls and the `Confirmed` / `Unconfirmed` markers that keep a `CALL` and a `SEND` apart |
| `action.rs` | the `Action` enum, the four dispatch unions, and the transcoding helpers |
| `mod.rs` | the module itself, and its re-exports |

Each struct gets a constructor taking its required fields, `with_…` setters for the optional
ones, a `Validate` impl generated from the schema's constraints, and the schema's own
description as its rustdoc.

## What the generator knows that the schemas do not

Three things, all of which live in `xtask/src/registry.rs`:

* **Direction** — which peer originates each action. The schemas do not say. The table was
  cross-checked against the use-case scenario descriptions in the specification text.
* **Message kind** — `CALL` or `SEND`. Only `NotifyPeriodicEventStream` is a `SEND`, and it is
  the only action with no response schema; the generator asserts that those two facts agree.
* **Functional block** — A–S in 2.x, the feature profiles in 1.6. Used for documentation and
  for `cargo xtask coverage`.

The generator also asserts the action counts (39 / 64 / 91) and fails if a schema file in the
directory is not covered by the registry — so adding a schema without classifying it is a build
failure, not a silent omission.

## Naming rules

The module and feature name for a version is the protocol's own version string with dots
replaced by underscores: `1.6` → `v1_6`, `2.0.1` → `v2_0_1`, `2.1` → `v2_1`. `schemas/` and
`src/` use the same spelling, so a version has one name everywhere in the repository.


* `FooEnumType` → `Foo`; `FooType` → `Foo`.
* …unless the short name is already taken by an object or by an action, in which case the enum
  keeps its `Enum` — the schema's own `javaType`. That gives `IdTokenEnum` (because
  `IdTokenType` is an object) and `ResetEnum` / `TransactionEventEnum` (because `Reset` and
  `TransactionEvent` are actions).
* Field names become `snake_case`, with an explicit `#[serde(rename)]` whenever the two differ
  — never an inferred casing rule, so a name like `dischargeLimit_L2` or `fixedPF` round-trips
  exactly.
* Enumeration values keep internal capitalisation: `Energy.Active.Import.Register` →
  `EnergyActiveImportRegister`, `L1-N` → `L1N`, `SHA256` → `SHA256`. A collision after the
  transformation is a generator error.

## OCPP 1.6's anonymous schemas

The 1.6 schemas name almost nothing: enumerations and nested objects are inline. The generator
carries a table mapping each `<file>:<json pointer>` to the name the 1.6 specification itself
uses — `IdTagInfo`, `ChargePointErrorCode`, `UnitOfMeasure`, `AuthorizationData` — and **fails**
if it meets an inline type the table does not name. The table cannot drift silently.

Two published-schema defects are handled explicitly rather than accidentally:

* `MeterValues` lists `Hertz` in its unit enumeration and `StopTransaction` does not. The
  generated `UnitOfMeasure` is the union, and the merge is declared in a `V16_UNION_ENUMS` list.
* `MeterValue.sampledValue` is `minItems: 1` in one file and unbounded in the other. The
  tighter bound wins, matching the specification's cardinality table.

## The appendix extractor

A second generator, `cargo xtask appendix`, produces `src/standard/` from OCPP 2.1 Part 2 —
Appendices: the security events and their criticality, the standardized components and
variables with their types and units, and the standardized reason codes. See
[Standard catalogues](@/docs/catalogues.md).

Unlike the schemas, the appendix is a PDF this repository does **not** redistribute, so this
generator runs on a developer's machine and its output is committed:

```console
$ pdftotext -layout specs/ocpp-2.1/OCPP-2.1_part2_appendices_v20.pdf specs/ocpp-2.1/appendices.txt
$ cargo xtask appendix
```

Only names, data types and criticality are reproduced — the identifiers two implementations
must agree on, the same category as the action names the schemas already carry. The prose
descriptions are not copied.

## Adding a version or an edition

1. Drop the new schemas into `schemas/<version>/`.
2. Add the actions to the registry with their direction, kind and block.
3. Run `cargo xtask codegen` and read the diff.
4. Run `cargo test` — the [schema conformance suite](@/docs/testing.md) will already be
   checking the new types.

`SPEC_EDITIONS.md` in the repository records exactly which editions and errata the committed
code targets.
