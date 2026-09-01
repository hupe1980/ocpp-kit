+++
title = "Signed meter values"
description = "The record a customer may actually be billed for: where each OCPP version hides it, why it is not the protocol's own number, and how to read the public-key field."
weight = 75
+++

An OCPP meter value is **telemetry**. Under German calibration law (MessEG §33) a customer may
be billed for a measured value only if they can check it, and what they check is the data set
the meter itself signed — an OCMF or EDL record, carried through OCPP as an opaque blob.

`ocpp_kit::metering` is the protocol knowledge around that blob: where each version puts it,
and how to read the public-key field, which is not what its name suggests. Everything above
the blob — parsing OCMF, verifying a signature, assembling a settlement series — is
deliberately out of scope and belongs to whatever bills the customer.

## The signed value is not the protocol's number

They are not even the same quantity. In the Open Charge Alliance's own example message
(*Signed Meter Values in OCPP* v1.0 §5.2) the 1.6 `StopTransaction` reports
`meterStop: 108814` — the meter's **lifetime** register in watt-hours — while the signed data
set in the same message reports the transaction running `0.000 → 0.636` kWh.

A CSMS billing `meterStop − meterStart` is not billing a slightly different number; it is
billing a different register. [`Record::energy_wh`](@/docs/domain.md) is exact, and exact is
not the same as authoritative.

## Where each version puts it

**2.0.1 and 2.1** have a typed field, `SampledValue.signedMeterValue`. Nothing to do.

**1.6 has no such field**, and what it does instead is not guessable from the schema. The OCA
application note (§3.2.1) reuses 2.x's `SignedMeterValueType` by serialising the whole object
into the `value` **string** of a `SampledValue` whose `format` is `SignedData`:

```json
{"format": "SignedData",
 "value": "{\"signedMeterData\":\"T0NNRnx7…\",\"encodingMethod\":\"OCMF\",\"publicKey\":\"MzA1Nj…\"}",
 "context": "Transaction.End", "measurand": "Energy.Active.Import.Register"}
```

A string holding JSON holding Base64 holding the record. A CSMS reading `value` as a
measurement — which is what the field is for everywhere else — finds a JSON document where it
expected kilowatt-hours. `SampledValue::signed_meter_value` reads it:

```rust
use ocpp_kit::v1_6;

let sample = v1_6::SampledValue::new(
    r#"{"signedMeterData":"T0NNRnx7fQ==","encodingMethod":"OCMF"}"#,
)
.with_format(v1_6::ValueFormat::SignedData);

let signed = sample.signed_meter_value().unwrap().unwrap();
assert_eq!(signed.signed_meter_data, "T0NNRnx7fQ==");
assert_eq!(signed.encoding_method.as_deref(), Some("OCMF"));

// A Raw sample is a measurement, and says so rather than half-parsing.
assert!(v1_6::SampledValue::new("12345.6").signed_meter_value().is_none());
```

1.6 also gives `StartTransaction` nowhere to carry a signed record, so a whole transaction's
records — begin *and* end — arrive together in `StopTransaction.transactionData`.

## Getting the record out

`signedMeterData` is specified as Base64 (2.0.1 Part 2 §2.46), and plenty of stations send
the record in plain instead. Refusing those is a quiet failure — the station keeps sending and
its sessions stop being billable — so both shapes are read. They cannot collide: an OCMF
record announces itself with `OCMF|`, and `|` is not in the Base64 alphabet.

```rust
use ocpp_kit::metering::SignedMeterValue;

const RECORD: &str = "OCMF|{\"FV\":\"1.0\"}|{\"SD\":\"3044\"}";

// Base64, as the specification says.
let encoded = SignedMeterValue::new("T0NNRnx7IkZWIjoiMS4wIn18eyJTRCI6IjMwNDQifQ==");
assert_eq!(encoded.decoded_str().unwrap(), RECORD);

// Plain, as a great many stations actually send it.
let plain = SignedMeterValue::new(RECORD);
assert_eq!(plain.decoded_str().unwrap(), RECORD);
```

`decoded()` returns the bytes for a binary format; `decoded_str()` returns text, which is what
OCMF and EDL both are. Nothing is re-encoded on the way out — these are the bytes the
signature covers.

## Both versions reach the same funnel

The [version-agnostic events](@/docs/domain.md) carry every signed meter value a message
carried, in arrival order, untouched:

```rust,no_run
use ocpp_kit::csms::events::{DomainEvent, observe_v16};
use ocpp_kit::v1_6;

# fn example(request: &v1_6::CsRequest) {
if let DomainEvent::TransactionEnded { signed, .. } = observe_v16(request).event {
    for reading in &signed {
        // Verbatim: the signature covers these bytes, so nothing re-encoded them.
        let _ = (&reading.value.signed_meter_data, &reading.context);
    }
}
# }
```

They reach the ledger the same way, as `Record::signed`, with
`Record::signed_with_context("Transaction.End")` to tell a begin record from an end one. A 1.6
`SignedData` document that does not parse raises an
[`Observed::warnings`](@/docs/domain.md) entry rather than vanishing.

## Writing one

A station has to *produce* the shape a CSMS reads, and writing the 1.6 one by hand fails
silently: the message is schema-valid whether or not `format` says `SignedData`, and a CSMS
without it reads the document as a measurement.

```rust
use ocpp_kit::metering::SignedMeterValue;
use ocpp_kit::v1_6::{Measurand, ReadingContext, SampledValue};

let record = SignedMeterValue::new("T0NNRnx7fQ==").with_encoding_method("OCMF");

let sample = SampledValue::signed(&record)
    .with_measurand(Measurand::EnergyActiveImportRegister)
    .with_context(ReadingContext::TransactionEnd);

assert_eq!(sample.signed_meter_value().unwrap().unwrap(), record);
```

The 2.x types convert with `TryFrom`, fallible because the versions disagree about what is
mandatory: 2.0.1 requires all four members, 2.1 requires only the record and its encoding. A
value read from a 2.1 station does not always fit a 2.0.1 message, and the error names the
member that is missing.

```rust
use ocpp_kit::metering::SignedMeterValue;
use ocpp_kit::v2_0_1;

let record = SignedMeterValue::new("T0NNRnx7fQ==").with_encoding_method("OCMF");
let error = v2_0_1::SignedMeterValue::try_from(&record).unwrap_err();
assert_eq!(error.to_string(), "OCPP 2.0.1 requires signedMeterValue.signingMethod");
```

## The public-key field is an envelope, and the spec contradicts its own example

`SignedMeterValueType.publicKey` does not hold key bytes. §3.2.2 specifies Base64 over a
colon-separated envelope:

```text
oca:<encoding>:<content-type>:<printed public key>
```

where the last part is the key **as printed on the certified meter** — the point being that a
customer can compare it against the label on the cabinet — and, for `base16`, "non-hexadecimal
character strings … and a hexadecimal prefix `0x` SHALL be ignored", because a printed key has
spaces in it.

The same document's example message (§5.2) then sends Base64 over plain uppercase hexadecimal
**with no envelope at all**. A reader that implements only §3.2.2 rejects the whitepaper's own
example; one that implements only the example rejects every conformant station. Both shapes
are in the field, so both are read:

```rust
use ocpp_kit::metering::{PublicKeyShape, decode_public_key};

// base64("oca:base16:secp256r1:30 59 30 13 …") — what the specification says.
let enveloped = decode_public_key(
    "b2NhOmJhc2UxNjpzZWNwMjU2cjE6MzAgNTkgMzAgMTMgMDYgMDcgMkEgODYgNDggQ0UgM0QgMDIgMDE=",
).unwrap();
assert_eq!(enveloped.shape, PublicKeyShape::Envelope);
assert_eq!(&enveloped.bytes[..2], &[0x30, 0x59]);
// The printed form is what a customer compares with the meter's label.
assert!(enveloped.printed.unwrap().starts_with("30 59"));

// base64("3059301306072A8648CE3D…") — what the example message sends.
let bare = decode_public_key("MzA1OTMwMTMwNjA3MkE4NjQ4Q0UzRDAyMDEwNjA4MkE4NjQ4Q0UzRDAzMDEwNw==")
    .unwrap();
assert_eq!(bare.shape, PublicKeyShape::PrintedHex);
assert_eq!(&bare.bytes[..2], &[0x30, 0x59]);
```

[`ocpp-cli signed`](@/docs/cli.md) runs the same reader from a terminal, which is how to find
out what a station sends before writing an integration around a guess.

**It is a claim, not a binding.** OCMF requires the public key to reach the customer out of
band — from the certified meter, or a registry — precisely because a key arriving on the same
socket as the record it signs proves only that whoever holds that socket owns some private
key. Verify against a key you obtained elsewhere; use this one to display, and to notice when
a station's key changes.
