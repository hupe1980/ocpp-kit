//! An idempotent transaction ledger for the CSMS side.
//!
//! A Charging Station may legitimately send the same `TransactionEvent` twice: it timed out
//! waiting for the answer, retried, and the first copy arrived after all (1.6 §3.7.1 and the
//! 2.x `MessageAttempts` rules make this normal, not exceptional). A CSMS that treats the
//! second copy as a new event double-bills. `seqNo` is what makes deduplication possible,
//! and the same field makes it possible to *notice* that an offline period lost events
//! entirely.
//!
//! The ledger stores one record per `(charging station, transaction)` and answers three
//! questions: have I seen this event, am I missing any, and is this transaction still open.
//! It works the same for 1.6, where `StartTransaction` / `MeterValues` / `StopTransaction`
//! are folded into the same shape.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use crate::types::{DateTime, Identity};

/// Which phase of a transaction an event belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventKind {
    /// The transaction began.
    Started,
    /// Something changed during the transaction.
    Updated,
    /// The transaction finished.
    Ended,
}

/// A version-neutral transaction event.
///
/// 1.6 has no `seqNo`; use `0` for `StartTransaction`, increasing numbers for each
/// `MeterValues`, and the highest for `StopTransaction`, or let
/// [`Ledger::ingest_unsequenced`] assign them.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionEvent {
    /// Which Charging Station sent it.
    pub identity: Identity,
    /// The transaction id.
    pub transaction_id: String,
    /// Its position in the transaction.
    pub seq_no: i32,
    /// Which phase it belongs to.
    pub kind: EventKind,
    /// When the station says it happened — not when it arrived.
    pub timestamp: DateTime,
    /// Whether the station was offline when it happened.
    pub offline: bool,
    /// The energy register, in Wh, if the event carried one.
    pub meter_wh: Option<f64>,
    /// The token that authorized the transaction.
    pub id_token: Option<String>,
    /// Why the transaction stopped, on an `Ended` event.
    pub stopped_reason: Option<String>,
}

impl TransactionEvent {
    /// A minimal event.
    #[must_use]
    pub fn new(
        identity: Identity,
        transaction_id: impl Into<String>,
        seq_no: i32,
        kind: EventKind,
        timestamp: DateTime,
    ) -> Self {
        Self {
            identity,
            transaction_id: transaction_id.into(),
            seq_no,
            kind,
            timestamp,
            offline: false,
            meter_wh: None,
            id_token: None,
            stopped_reason: None,
        }
    }

    /// Attaches the energy register.
    #[must_use]
    pub fn with_meter(mut self, wh: f64) -> Self {
        self.meter_wh = Some(wh);
        self
    }

    /// Attaches the authorizing token.
    #[must_use]
    pub fn with_id_token(mut self, id_token: impl Into<String>) -> Self {
        self.id_token = Some(id_token.into());
        self
    }

    /// Marks the event as one that happened while the station was offline.
    #[must_use]
    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }
}

/// What the ledger did with an event.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ingested {
    /// Recorded.
    Applied,
    /// Recorded, but sequence numbers are missing before it. Almost always an offline
    /// period whose queue overflowed, or a station that skipped a message after exhausting
    /// its retries.
    AppliedWithGap {
        /// The `seqNo`s that have never arrived.
        missing: Vec<i32>,
    },
    /// This exact `(station, transaction, seqNo)` has already been recorded. The station
    /// retried; do not bill it twice.
    Duplicate,
    /// The transaction had already ended when this arrived. Recorded, but flagged.
    AfterEnd,
}

/// One transaction, as the CSMS knows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// Which Charging Station.
    pub identity: Identity,
    /// The transaction id.
    pub transaction_id: String,
    /// When it started, once a `Started` event has arrived.
    pub started_at: Option<DateTime>,
    /// When it ended.
    pub ended_at: Option<DateTime>,
    /// The token that authorized it.
    pub id_token: Option<String>,
    /// Why it stopped.
    pub stopped_reason: Option<String>,
    /// The reading the transaction started at.
    ///
    /// The `Started` event's, whenever one arrives — not merely the first reading seen. The
    /// two differ exactly when events arrive out of order, which is the case this ledger
    /// exists to survive.
    pub meter_start_wh: Option<f64>,
    /// The reading the transaction ended at.
    ///
    /// The `Ended` event's once it has arrived; before that, the latest reading seen. A
    /// straggling `Updated` that turns up afterwards does not overwrite it, or the billed
    /// energy would shrink — and could go negative — as late events landed.
    pub meter_stop_wh: Option<f64>,
    /// Whether any event was produced while the station was offline.
    pub had_offline_events: bool,
    seen: BTreeSet<i32>,
    highest: i32,
}

impl Record {
    /// The `seqNo`s that have not arrived, below the highest one seen.
    #[must_use]
    pub fn missing(&self) -> Vec<i32> {
        (0..=self.highest)
            .filter(|seq| !self.seen.contains(seq))
            .collect()
    }

    /// Whether the transaction is still open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }

    /// How many events have been recorded.
    #[must_use]
    pub fn events(&self) -> usize {
        self.seen.len()
    }

    /// The energy delivered, when both a start and a stop reading are known.
    #[must_use]
    pub fn energy_wh(&self) -> Option<f64> {
        match (self.meter_start_wh, self.meter_stop_wh) {
            (Some(start), Some(stop)) => Some(stop - start),
            _ => None,
        }
    }
}

/// The ledger.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    records: BTreeMap<(Identity, String), Record>,
    next_seq: BTreeMap<(Identity, String), i32>,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one event, deduplicating on `(station, transaction, seqNo)`.
    pub fn ingest(&mut self, event: &TransactionEvent) -> Ingested {
        let key = (event.identity.clone(), event.transaction_id.clone());
        let record = self.records.entry(key).or_insert_with(|| Record {
            identity: event.identity.clone(),
            transaction_id: event.transaction_id.clone(),
            started_at: None,
            ended_at: None,
            id_token: None,
            stopped_reason: None,
            meter_start_wh: None,
            meter_stop_wh: None,
            had_offline_events: false,
            seen: BTreeSet::new(),
            highest: -1,
        });

        if record.seen.contains(&event.seq_no) {
            return Ingested::Duplicate;
        }
        let after_end = record.ended_at.is_some();

        record.seen.insert(event.seq_no);
        record.highest = record.highest.max(event.seq_no);
        record.had_offline_events |= event.offline;
        if let Some(token) = &event.id_token {
            record.id_token.get_or_insert_with(|| token.clone());
        }
        if let Some(wh) = event.meter_wh {
            // `Started` and `Ended` are authoritative for their own end of the transaction;
            // an `Updated` only fills a gap neither has filled. Letting an `Updated` overwrite
            // either would make the billed energy depend on arrival order, and a straggler
            // after the `Ended` could make it negative.
            match event.kind {
                EventKind::Started => record.meter_start_wh = Some(wh),
                EventKind::Ended => record.meter_stop_wh = Some(wh),
                EventKind::Updated => {
                    if record.meter_start_wh.is_none() {
                        record.meter_start_wh = Some(wh);
                    }
                    if !after_end {
                        record.meter_stop_wh = Some(wh);
                    }
                }
            }
        }
        match event.kind {
            EventKind::Started => record.started_at = Some(event.timestamp),
            EventKind::Ended => {
                record.ended_at = Some(event.timestamp);
                record.stopped_reason.clone_from(&event.stopped_reason);
            }
            EventKind::Updated => {}
        }

        if after_end {
            return Ingested::AfterEnd;
        }
        let missing = record.missing();
        if missing.is_empty() {
            Ingested::Applied
        } else {
            Ingested::AppliedWithGap { missing }
        }
    }

    /// Records an event for a version that has no `seqNo` — OCPP 1.6 — assigning the next
    /// number for the transaction.
    ///
    /// Deduplication then falls back to `(station, transaction, kind, timestamp)`, which is
    /// what 1.6 gives us to work with.
    pub fn ingest_unsequenced(&mut self, event: &TransactionEvent) -> Ingested {
        let key = (event.identity.clone(), event.transaction_id.clone());
        if let Some(record) = self.records.get(&key) {
            let duplicate = match event.kind {
                EventKind::Started => record.started_at == Some(event.timestamp),
                EventKind::Ended => record.ended_at == Some(event.timestamp),
                EventKind::Updated => false,
            };
            if duplicate {
                return Ingested::Duplicate;
            }
        }
        let seq = self.next_seq.entry(key).or_insert(0);
        let mut event = event.clone();
        event.seq_no = *seq;
        *seq += 1;
        self.ingest(&event)
    }

    /// Looks a transaction up.
    #[must_use]
    pub fn transaction(&self, identity: &Identity, transaction_id: &str) -> Option<&Record> {
        self.records.get(&(identity.clone(), transaction_id.into()))
    }

    /// Every transaction still open for one station.
    pub fn open(&self, identity: &Identity) -> impl Iterator<Item = &Record> {
        self.records
            .values()
            .filter(move |record| &record.identity == identity && record.is_open())
    }

    /// Every transaction with missing sequence numbers, across all stations.
    pub fn incomplete(&self) -> impl Iterator<Item = (&Record, Vec<i32>)> {
        self.records.values().filter_map(|record| {
            let missing = record.missing();
            (!missing.is_empty()).then_some((record, missing))
        })
    }

    /// How many transactions are recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the ledger is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Forgets transactions that ended before `cutoff`, for a CSMS that archives elsewhere.
    pub fn prune_ended_before(&mut self, cutoff: DateTime) -> usize {
        let before = self.records.len();
        self.records
            .retain(|_, record| record.ended_at.is_none_or(|ended| ended >= cutoff));
        before - self.records.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station() -> Identity {
        Identity::new("CS-0001").unwrap()
    }

    fn at(seconds: i64) -> DateTime {
        DateTime::from_timestamp(jiff::Timestamp::from_second(1_700_000_000 + seconds).unwrap())
    }

    fn event(seq: i32, kind: EventKind) -> TransactionEvent {
        TransactionEvent::new(station(), "tx-1", seq, kind, at(i64::from(seq)))
    }

    #[test]
    fn a_retried_event_is_recognised_rather_than_billed_twice() {
        let mut ledger = Ledger::new();
        assert_eq!(
            ledger.ingest(&event(0, EventKind::Started)),
            Ingested::Applied
        );
        assert_eq!(
            ledger.ingest(&event(1, EventKind::Updated)),
            Ingested::Applied
        );
        // The station timed out and re-sent seqNo 1.
        assert_eq!(
            ledger.ingest(&event(1, EventKind::Updated)),
            Ingested::Duplicate
        );
        assert_eq!(ledger.transaction(&station(), "tx-1").unwrap().events(), 2);
    }

    #[test]
    fn a_gap_is_reported_and_closes_when_the_missing_event_arrives() {
        let mut ledger = Ledger::new();
        ledger.ingest(&event(0, EventKind::Started));
        assert_eq!(
            ledger.ingest(&event(3, EventKind::Updated)),
            Ingested::AppliedWithGap {
                missing: alloc::vec![1, 2]
            }
        );
        assert_eq!(ledger.incomplete().count(), 1);

        ledger.ingest(&event(1, EventKind::Updated));
        assert_eq!(
            ledger.ingest(&event(2, EventKind::Updated)),
            Ingested::Applied,
            "the gap has closed"
        );
        assert_eq!(ledger.incomplete().count(), 0);
    }

    #[test]
    fn a_transaction_records_its_energy_and_its_reason() {
        let mut ledger = Ledger::new();
        ledger.ingest(
            &event(0, EventKind::Started)
                .with_meter(1000.0)
                .with_id_token("CARD-1"),
        );
        ledger.ingest(&event(1, EventKind::Updated).with_meter(4500.0).offline());
        let mut ended = event(2, EventKind::Ended).with_meter(7300.0);
        ended.stopped_reason = Some("EVDisconnected".into());
        ledger.ingest(&ended);

        let record = ledger.transaction(&station(), "tx-1").unwrap();
        assert!(!record.is_open());
        assert_eq!(record.energy_wh(), Some(6300.0));
        assert_eq!(record.id_token.as_deref(), Some("CARD-1"));
        assert_eq!(record.stopped_reason.as_deref(), Some("EVDisconnected"));
        assert!(record.had_offline_events);
        assert_eq!(ledger.open(&station()).count(), 0);
    }

    #[test]
    fn an_event_after_the_end_is_flagged_but_not_lost() {
        let mut ledger = Ledger::new();
        ledger.ingest(&event(0, EventKind::Started));
        ledger.ingest(&event(1, EventKind::Ended));
        assert_eq!(
            ledger.ingest(&event(2, EventKind::Updated)),
            Ingested::AfterEnd
        );
        assert_eq!(ledger.transaction(&station(), "tx-1").unwrap().events(), 3);
    }

    #[test]
    fn ocpp_16_transactions_are_sequenced_by_the_ledger() {
        let mut ledger = Ledger::new();
        let start =
            TransactionEvent::new(station(), "42", 0, EventKind::Started, at(0)).with_meter(100.0);
        assert_eq!(ledger.ingest_unsequenced(&start), Ingested::Applied);
        // 1.6 has no seqNo, so a re-sent StartTransaction is caught by its timestamp.
        assert_eq!(ledger.ingest_unsequenced(&start), Ingested::Duplicate);

        let meter = TransactionEvent::new(station(), "42", 0, EventKind::Updated, at(60))
            .with_meter(2100.0);
        assert_eq!(ledger.ingest_unsequenced(&meter), Ingested::Applied);
        let stop =
            TransactionEvent::new(station(), "42", 0, EventKind::Ended, at(120)).with_meter(3400.0);
        assert_eq!(ledger.ingest_unsequenced(&stop), Ingested::Applied);

        let record = ledger.transaction(&station(), "42").unwrap();
        assert_eq!(record.energy_wh(), Some(3300.0));
        assert!(record.missing().is_empty());
    }

    /// Out-of-order delivery is the case this ledger exists to survive, so the billed energy
    /// must not depend on the order the events happen to arrive in. Both halves of this
    /// failed before: an `Updated` that arrived first became the start reading and kept it
    /// when the real `Started` turned up, and a straggling `Updated` overwrote the stop
    /// reading after the `Ended` had set it.
    #[test]
    fn meter_readings_come_from_started_and_ended_whatever_the_arrival_order() {
        let mut ledger = Ledger::new();

        // The periodic meter value overtakes the transaction's own start.
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 1, EventKind::Updated, at(60))
                .with_meter(2100.0),
        );
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 0, EventKind::Started, at(0))
                .with_meter(1000.0),
        );
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 2, EventKind::Ended, at(120))
                .with_meter(3400.0),
        );
        // And one more straggler lands after the transaction has already ended.
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 3, EventKind::Updated, at(90))
                .with_meter(2800.0),
        );

        let record = ledger.transaction(&station(), "tx-1").unwrap();
        assert_eq!(record.meter_start_wh, Some(1000.0));
        assert_eq!(record.meter_stop_wh, Some(3400.0));
        assert_eq!(record.energy_wh(), Some(2400.0));
    }

    #[test]
    fn pruning_keeps_open_transactions() {
        let mut ledger = Ledger::new();
        ledger.ingest(&event(0, EventKind::Started));
        ledger.ingest(&event(1, EventKind::Ended));
        let mut other = event(0, EventKind::Started);
        other.transaction_id = "tx-2".into();
        ledger.ingest(&other);

        assert_eq!(ledger.prune_ended_before(at(1000)), 1);
        assert_eq!(ledger.len(), 1);
        assert!(ledger.transaction(&station(), "tx-2").is_some());
    }
}
