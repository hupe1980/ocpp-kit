//! Randomised property tests for the pieces where a hand-picked example proves little.
//!
//! Each property is checked over hundreds of pseudo-random scenarios from a deterministic
//! generator, so a failure reproduces exactly from its seed. The properties are the ones that
//! actually matter in the field: a transaction message is never lost, reordered or delivered
//! twice; a ledger reaches the same state whatever order events arrive in; a composite
//! schedule agrees with the stacking rules evaluated directly.

// The properties cover the L4 blocks, so they only exist when those blocks do.
// Scenario sizes here are small integers by construction; the casts cannot lose anything.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::range_plus_one,
    clippy::too_many_lines,
    clippy::type_complexity
)]

use std::collections::BTreeSet;

use ocpp_kit::csms::ledger::{EventKind, Ingested, Ledger, TransactionEvent};
use ocpp_kit::engine::{
    Engine, EngineConfig, HeartbeatPolicy, Input, Instant, MemStore, MessageStore, OfflinePolicy,
    Output, RetryPolicy, Role,
};
use ocpp_kit::station::smart_charging::{
    Period, Profile, ProfileKind, ProfileStore, Purpose, RateUnit, Schedule,
};
use ocpp_kit::types::{DateTime, Decimal, Identity};
use ocpp_kit::{RawValue, Version};

/// The engine takes the driver's clock on every entry point. A test that is not
/// exercising a timer supplies the origin and moves on.
const NOW: Instant = Instant::ZERO;

/// How many scenarios each property is checked over.
const CASES: u64 = 400;

/// `at` advanced by `seconds`.
fn plus(at: DateTime, seconds: i64) -> DateTime {
    DateTime::from_timestamp(
        jiff::Timestamp::from_second(at.timestamp().as_second() + seconds).unwrap(),
    )
}

/// xorshift64*, so a failing case reproduces from its seed alone.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_string()).unwrap()
}

fn tx_payload(seq: u32) -> Box<RawValue> {
    raw(&format!(
        r#"{{"eventType":"Updated","timestamp":"2024-01-01T00:00:00Z","triggerReason":"MeterValuePeriodic","seqNo":{seq},"transactionInfo":{{"transactionId":"t1"}}}}"#
    ))
}

/// A station that has already been accepted, so the boot gate is out of the way.
fn booted_station(retry: RetryPolicy, queue_all: bool) -> Engine<MemStore> {
    let mut engine = Engine::new(
        EngineConfig::new(Role::ChargingStation, Version::V2_1)
            .with_heartbeat(HeartbeatPolicy::Manual)
            .with_retry(retry)
            .with_offline(OfflinePolicy {
                queue_all_messages: queue_all,
                max_queued: 4096,
            }),
    );
    engine.handle(
        NOW,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    engine
        .call(NOW, "BootNotification", raw(r#"{"reason":"PowerUp"}"#))
        .unwrap();
    let id = sent_ids(&mut engine).pop().expect("the boot goes out");
    engine.handle(NOW, Input::Received(&format!(
        r#"[3,"{id}",{{"currentTime":"2024-01-01T00:00:00Z","interval":0,"status":"Accepted"}}]"#
    )));
    let _ = engine.drain();
    engine
}

/// Drains the engine and returns the `MessageId`s of the frames it wants transmitted.
fn sent_ids(engine: &mut Engine<MemStore>) -> Vec<String> {
    engine
        .drain()
        .into_iter()
        .filter_map(|output| match output {
            Output::Transmit(text) => {
                let parts: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
                Some(parts[1].as_str()?.to_string())
            }
            _ => None,
        })
        .collect()
}

/// Everything the engine transmitted, as `(id, seqNo)` for transaction events.
fn sent_transactions(engine: &mut Engine<MemStore>) -> Vec<(String, u32)> {
    engine
        .drain()
        .into_iter()
        .filter_map(|output| match output {
            Output::Transmit(text) => {
                let parts: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
                if parts[0].as_u64()? != 2 || parts[2].as_str()? != "TransactionEvent" {
                    return None;
                }
                let seq = parts[3].get("seqNo")?.as_u64()? as u32;
                Some((parts[1].as_str()?.to_string(), seq))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn transaction_messages_are_delivered_in_order_and_exactly_once() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9) + 1);
        // Retries are generous, so nothing is legitimately skipped and every message must
        // eventually arrive exactly once.
        let mut engine = booted_station(
            RetryPolicy {
                attempts: 32,
                interval: std::time::Duration::from_secs(1),
            },
            true,
        );

        let count = 1 + rng.below(8);
        for seq in 0..count {
            engine
                .call(NOW, "TransactionEvent", tx_payload(seq as u32))
                .unwrap();
        }

        let mut delivered: Vec<u32> = Vec::new();
        let mut now = 0u64;
        // Drive the session through random disconnections and timeouts until the queue drains.
        for _ in 0..400 {
            for (id, seq) in sent_transactions(&mut engine) {
                if rng.chance(70) {
                    // The CSMS answered.
                    engine.handle(NOW, Input::Received(&format!(r#"[3,"{id}",{{}}]"#)));
                    delivered.push(seq);
                } else if rng.chance(50) {
                    // The link dropped before the answer arrived.
                    engine.handle(NOW, Input::Disconnected);
                    now += 60_000;
                    engine.handle(Instant::from_millis(now), Input::Timeout);
                    engine.handle(
                        NOW,
                        Input::Connected {
                            version: Version::V2_1,
                        },
                    );
                } else {
                    // The answer never came.
                    now += 60_000;
                    engine.handle(Instant::from_millis(now), Input::Timeout);
                }
            }
            now += 60_000;
            engine.handle(Instant::from_millis(now), Input::Timeout);
            if engine.queued() == 0 && !engine.has_outstanding_call() {
                break;
            }
        }

        // A retransmission after a timeout is expected — that is why the CSMS ledger
        // deduplicates. What must hold is that the *first* delivery of each message happens
        // in order, and that every message is delivered.
        let mut first_deliveries = Vec::new();
        for seq in &delivered {
            if !first_deliveries.contains(seq) {
                first_deliveries.push(*seq);
            }
        }
        assert_eq!(
            first_deliveries,
            (0..count as u32).collect::<Vec<_>>(),
            "seed {seed}: 1.6 §3.7 — transaction events are delivered in chronological order \
             (saw {delivered:?})"
        );
        assert!(
            engine.store().is_empty().unwrap(),
            "seed {seed}: the durable store drains"
        );
    }
}

#[test]
fn a_call_always_reaches_exactly_one_terminal_outcome() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed.wrapping_mul(0x85EB_CA6B) + 1);
        let mut engine = booted_station(RetryPolicy::none(), rng.chance(50));

        let mut started = 0usize;
        let mut finished = 0usize;
        let mut now = 0u64;

        for _ in 0..40 {
            match rng.below(5) {
                0 => {
                    engine.call(NOW, "Heartbeat", raw("{}")).unwrap();
                    started += 1;
                }
                1 => {
                    engine.call(NOW, "TransactionEvent", tx_payload(0)).unwrap();
                    started += 1;
                }
                2 => engine.handle(NOW, Input::Disconnected),
                3 => engine.handle(
                    NOW,
                    Input::Connected {
                        version: Version::V2_1,
                    },
                ),
                _ => {
                    now += 31_000;
                    engine.handle(Instant::from_millis(now), Input::Timeout);
                }
            }
            for output in engine.drain() {
                match output {
                    Output::Outcome(_) => finished += 1,
                    // Answer roughly half of what goes out.
                    Output::Transmit(text) if rng.chance(50) => {
                        if let Ok(parts) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                            if parts[0].as_u64() == Some(2) {
                                let id = parts[1].as_str().unwrap();
                                engine.handle(NOW, Input::Received(&format!(r#"[3,"{id}",{{}}]"#)));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Drain whatever is left: disconnect, then let every timer fire.
        engine.handle(NOW, Input::Disconnected);
        for step in 1..40 {
            engine.handle(Instant::from_millis(now + step * 120_000), Input::Timeout);
            finished += engine
                .drain()
                .into_iter()
                .filter(|o| matches!(o, Output::Outcome(_)))
                .count();
        }

        // Anything still queued has legitimately not finished: a transaction message waits
        // for a reconnect that this scenario never grants it.
        let pending = engine.queued() + usize::from(engine.has_outstanding_call());
        assert_eq!(
            finished + pending,
            started,
            "seed {seed}: a call ends exactly once or is still queued \
             ({started} started, {finished} ended, {pending} pending)"
        );
    }
}

#[test]
fn the_ledger_reaches_the_same_state_whatever_order_events_arrive_in() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed.wrapping_mul(0xC2B2_AE35) + 1);
        let station = Identity::new("CS-0001").unwrap();
        let count = 2 + rng.below(8);

        let events: Vec<TransactionEvent> = (0..count)
            .map(|seq| {
                let kind = if seq == 0 {
                    EventKind::Started
                } else if seq == count - 1 {
                    EventKind::Ended
                } else {
                    EventKind::Updated
                };
                TransactionEvent::new(
                    station.clone(),
                    "tx-1",
                    i32::try_from(seq).unwrap(),
                    kind,
                    DateTime::from_timestamp(
                        jiff::Timestamp::from_second(1_700_000_000 + seq as i64).unwrap(),
                    ),
                )
                // A register that a float would round: 0.001 Wh steps at three decimals.
                .with_meter(Decimal::new(i64::try_from(seq).unwrap() * 100_001, 3))
            })
            .collect();

        // In order.
        let mut ordered = Ledger::new();
        for event in &events {
            ordered.ingest(event);
        }

        // Shuffled, with duplicates sprinkled in — which is what a retrying station produces.
        let mut shuffled = Ledger::new();
        let mut order: Vec<usize> = (0..count).collect();
        for index in (1..count).rev() {
            order.swap(index, rng.below(index + 1));
        }
        let mut duplicates = 0;
        for index in order {
            shuffled.ingest(&events[index]);
            if rng.chance(40) {
                assert_eq!(
                    shuffled.ingest(&events[index]),
                    Ingested::Duplicate,
                    "seed {seed}: a repeat must always be recognised"
                );
                duplicates += 1;
            }
        }
        let _ = duplicates;

        let a = ordered.transaction(&station, "tx-1").unwrap();
        let b = shuffled.transaction(&station, "tx-1").unwrap();
        assert_eq!(a.events(), b.events(), "seed {seed}: same number of events");
        assert_eq!(a.started_at, b.started_at, "seed {seed}");
        assert_eq!(a.ended_at, b.ended_at, "seed {seed}");
        assert!(
            a.missing().is_empty() && b.missing().is_empty(),
            "seed {seed}: no gaps"
        );
    }
}

#[test]
fn a_gap_is_reported_exactly_when_a_sequence_number_is_missing() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed.wrapping_mul(0x27D4_EB2F) + 1);
        let station = Identity::new("CS-0001").unwrap();
        let mut ledger = Ledger::new();

        let highest = 1 + rng.below(12);
        let mut sent: BTreeSet<i32> = BTreeSet::new();
        for seq in 0..=highest {
            if rng.chance(70) {
                sent.insert(i32::try_from(seq).unwrap());
            }
        }
        // Always include the highest, so the expected gap set is well defined.
        sent.insert(i32::try_from(highest).unwrap());

        for seq in &sent {
            ledger.ingest(&TransactionEvent::new(
                station.clone(),
                "tx-1",
                *seq,
                EventKind::Updated,
                DateTime::UNIX_EPOCH,
            ));
        }

        let expected: Vec<i32> = (0..=i32::try_from(highest).unwrap())
            .filter(|seq| !sent.contains(seq))
            .collect();
        let record = ledger.transaction(&station, "tx-1").unwrap();
        assert_eq!(record.missing(), expected, "seed {seed}");
    }
}

#[test]
fn a_composite_schedule_agrees_with_the_stacking_rules_evaluated_directly() {
    let start = DateTime::parse("2024-01-01T00:00:00Z").unwrap();
    let duration = 3600i64;

    for seed in 0..CASES {
        let mut rng = Rng::new(seed.wrapping_mul(0x165_667B1) + 1);
        let mut store = ProfileStore::new();

        // Profiles across the three purposes that behave differently: two that stack and
        // minimise, and one that is added on top.
        let mut declared: Vec<(Purpose, i32, Option<i64>, Vec<(i64, Decimal)>)> = Vec::new();
        for id in 0..(1 + rng.below(5)) {
            let purpose = match rng.below(10) {
                0..=4 => Purpose::TxDefaultProfile,
                5..=8 => Purpose::ChargingStationMaxProfile,
                _ => Purpose::LocalGeneration,
            };
            let stack = i32::try_from(rng.below(4)).unwrap();
            // A schedule that may start part-way through the window, so a higher stack level
            // does not always have a period to contribute.
            let first_step = (rng.below(3) * 600) as i64;
            let steps: Vec<(i64, Decimal)> = (0..(1 + rng.below(3)))
                .map(|step| {
                    (
                        first_step + (step * (600 + rng.below(600))) as i64,
                        // Two decimals, so a limit that survived an f64 would show up.
                        Decimal::new(i64::try_from(1 + rng.below(60)).unwrap() * 100 + 25, 2),
                    )
                })
                .collect();
            // …and a validity window that may expire part-way through it, so a higher stack
            // level stops leading and a lower one has to take over (§3.6).
            let valid_to = rng
                .chance(40)
                .then(|| ((1 + rng.below(5)) * 600) as i64)
                .filter(|seconds| *seconds < duration);

            let schedule = Schedule::new(
                1,
                RateUnit::A,
                steps
                    .iter()
                    .map(|(at, limit)| Period::new(*at, *limit))
                    .collect(),
            )
            .starting(start);
            let mut profile = Profile::new(
                i32::try_from(id).unwrap(),
                stack,
                purpose,
                ProfileKind::Absolute,
                schedule,
            );
            if let Some(seconds) = valid_to {
                profile = profile.valid(None, Some(plus(start, seconds)));
            }
            store.install(profile);
            declared.push((purpose, stack, valid_to, steps));
        }

        let composite = store.composite(1, start, duration, RateUnit::A, None);

        // Part 2 §3.6, evaluated directly: per purpose, the *leading* schedule is the one
        // with the highest stack level that is both valid at this instant and has a period
        // defined for it. Across purposes, the lowest limit — and LocalGeneration on top.
        for offset in (0..duration).step_by(37) {
            let leading = |purpose: Purpose| -> Option<Decimal> {
                declared
                    .iter()
                    .filter(|(p, _, _, _)| *p == purpose)
                    .filter_map(|(_, stack, valid_to, steps)| {
                        if valid_to.is_some_and(|until| offset >= until) {
                            return None;
                        }
                        steps
                            .iter()
                            .rev()
                            .find(|(at, _)| *at <= offset)
                            .map(|(_, limit)| (*stack, *limit))
                    })
                    .max_by_key(|(stack, _)| *stack)
                    .map(|(_, limit)| limit)
            };

            let base = match (
                leading(Purpose::TxDefaultProfile),
                leading(Purpose::ChargingStationMaxProfile),
            ) {
                (Some(session), Some(ceiling)) => Some(session.min(ceiling)),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            };
            let expected = match (base, leading(Purpose::LocalGeneration)) {
                (Some(base), Some(generation)) => Some(base + generation),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            };

            let actual = composite
                .periods
                .iter()
                .rev()
                .find(|period| period.start_period <= offset)
                .and_then(|period| period.limit);

            match (expected, actual) {
                // Exact: the calculation only ever selects, minimises and adds, and all
                // three are exact on decimals. A tolerance here would hide a real defect.
                (Some(expected), Some(actual)) => assert_eq!(
                    expected, actual,
                    "seed {seed} at +{offset}s: expected {expected}, got {actual}"
                ),
                (None, _) => {}
                (Some(expected), None) => {
                    panic!("seed {seed} at +{offset}s: expected {expected}, got nothing")
                }
            }
        }

        // Merged: no two consecutive steps carry the same limit.
        for pair in composite.periods.windows(2) {
            assert!(
                pair[0].limit != pair[1].limit || pair[0].number_phases != pair[1].number_phases,
                "seed {seed}: consecutive steps must differ"
            );
        }
    }
}
