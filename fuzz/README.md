# Fuzzing

Four `cargo fuzz` targets over the code that faces hostile input:

| Target | Input | Property |
|---|---|---|
| `frame` | one OCPP-J frame (UTF-8) | parses under 1.6, 2.0.1 and 2.1 without panicking; anything parsed re-serializes and re-parses to the same frame |
| `payload` | selector byte + JSON payload | the selector picks a 2.1 action (`v2_1::Action::ALL[b % len]`); decoding never panics under strict, pedantic or lenient; the output decodes again under the policy that produced it |
| `engine` | newline-separated frames | a 2.1 station engine and CSMS engine survive the sequence and only emit frames they can parse |
| `websocket` | raw bytes off the socket | the RFC 6455 codec (both roles, no deflate) terminates, never panics, round-trips what it decodes |

## Layout

```text
fuzz/
├── fuzz_targets/<target>.rs   the targets
├── corpus/<target>/           committed, minimised seed corpus (starting point for every run)
├── seeds/generate.py          writes the raw seeds the corpus was minimised from
└── artifacts/<target>/        crash inputs libFuzzer writes (gitignored)
```

Crashes are kept as regressions under `tests/data/fuzz-regressions/<target>/`. On stable Rust,
`cargo test --test fuzz_regressions` runs every regression **and** every corpus file through a
copy of the target's body, so a recorded crash fails the normal test suite until it is fixed.

## Running

```sh
cargo +nightly fuzz run websocket -- -max_total_time=60     # starts from fuzz/corpus/websocket
```

A crash lands in `fuzz/artifacts/<target>/crash-<sha1>`. Reproduce with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<sha1>`, then copy the file to
`tests/data/fuzz-regressions/<target>/` (a descriptive name is fine) and fix the bug.

`.github/workflows/fuzz.yml` runs every target weekly for 20 minutes (manual runs take a
budget), uploads crash inputs as an artifact, and offers the grown corpus back, minimised.

## Regenerating the corpus

```sh
python3 -I fuzz/seeds/generate.py                       # -> fuzz/seeds/out/<target>/
for t in engine frame payload websocket; do
  rm -rf fuzz/corpus/$t && mkdir -p fuzz/corpus/$t
  cargo +nightly fuzz run $t fuzz/corpus/$t fuzz/seeds/out/$t -- -merge=1
done
```

The generator is standard-library Python and deterministic. Payloads are synthesised from the
vendored OCA schemas in `schemas/` (required members only, and every member), frames wrap
them for all three versions plus near-valid frames that break one rule each, engine inputs
are short sessions, and WebSocket inputs are masked client and unmasked server frames:
fragmented, interleaved control frames, close codes, all three length forms, and
permessage-deflate (RSV1) frames, which the target's no-extension codec must refuse. Every
identity in it is synthetic (`SYNTH-01`).

`-merge=1` keeps only inputs that add coverage, so the committed set is a subset of the
generator output; libFuzzer names merged files by the SHA-1 of their content.
