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
//!
//! # What the energy figures are, and are not
//!
//! [`Record::energy_wh`] is the difference of the station's own start and stop registers,
//! computed exactly: the readings are [`Decimal`]s at the resolution the meter wrote, and the
//! subtraction is the one OCPP defines a session's energy as. Nothing here is rounded and
//! nothing goes through an `f64`, so the figure can be carried into an invoice without
//! having quietly lost exactness on the way.
//!
//! Exact is not the same as *authoritative*, and the distinction is worth keeping:
//!
//! * **The registers may not even be the quantity you want.** In the Open Charge Alliance's
//!   own example message a 1.6 `StopTransaction` reports `meterStop: 108814` — the meter's
//!   *lifetime* total in Wh — while the signed record beside it reports the transaction
//!   running `0.000 → 0.636` kWh. `energy_wh` subtracts what the station reported, exactly;
//!   whether those two readings are the session's is the station's claim, not this ledger's.
//! * **It is what the station said**, over a transport with no integrity guarantee beyond
//!   TLS. Where calibration law requires the billable kWh to be traceable to the meter itself
//!   — Germany's Eichrecht, for one — the basis is the signed record, carried through as
//!   [`Record::signed`] and verified against the meter's public key obtained somewhere other
//!   than this socket. See [`crate::metering`].
//! * A gap ([`Ingested::AppliedWithGap`]) or an event after the end
//!   ([`Ingested::AfterEnd`]) says the record is incomplete, not that the energy is wrong —
//!   but a transaction that ends without a `Started` event has no start register, and
//!   `energy_wh` returns `None` rather than guessing at one.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use super::events::SignedReading;
use crate::types::{DateTime, Decimal, Identity};

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
    /// The EVSE the transaction is at, when the message named one.
    pub evse_id: Option<i32>,
    /// The connector, when the message named one. 1.6 names one on `StartTransaction` and on
    /// `MeterValues`, and none on `StopTransaction`.
    pub connector_id: Option<i32>,
    /// Its position in the transaction.
    pub seq_no: i32,
    /// Which phase it belongs to.
    pub kind: EventKind,
    /// When the station says it happened — not when it arrived.
    pub timestamp: DateTime,
    /// Whether the station was offline when it happened.
    pub offline: bool,
    /// The energy register, in Wh, if the event carried one — exactly as the meter stated
    /// it, trailing zeros and all.
    pub meter_wh: Option<Decimal>,
    /// Every signed meter value the event carried, in the order they arrived.
    ///
    /// Where calibration law applies this — not [`meter_wh`](Self::meter_wh) — is the
    /// billable value. See [`crate::metering`].
    pub signed: Vec<SignedReading>,
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
            evse_id: None,
            connector_id: None,
            seq_no,
            kind,
            timestamp,
            offline: false,
            meter_wh: None,
            signed: Vec::new(),
            id_token: None,
            stopped_reason: None,
        }
    }

    /// Attaches the energy register, in Wh.
    ///
    /// Takes anything that converts into a [`Decimal`] — an integer, or a
    /// [`decimal!`](crate::decimal) literal. It deliberately does not take an `f64`: a
    /// register that has been through a float has already lost the resolution it claimed.
    #[must_use]
    pub fn with_meter(mut self, wh: impl Into<Decimal>) -> Self {
        self.meter_wh = Some(wh.into());
        self
    }

    /// Attaches a signed meter value.
    #[must_use]
    pub fn with_signed(mut self, signed: SignedReading) -> Self {
        self.signed.push(signed);
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
    /// The EVSE the transaction is at, from the first event that named one.
    ///
    /// A CDR names the point the session happened at, and only some of a transaction's
    /// messages carry it: 1.6 puts a `connectorId` on `StartTransaction` and none on
    /// `StopTransaction`, and 2.x's `evse` is optional on every `TransactionEvent`. Keeping
    /// the first one seen spares a CSMS remembering it for the rest of the transaction.
    pub evse_id: Option<i32>,
    /// The connector, from the first event that named one. 1.6 has connectors and no EVSEs,
    /// so this is what it fills in.
    pub connector_id: Option<i32>,
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
    pub meter_start_wh: Option<Decimal>,
    /// The reading the transaction ended at.
    ///
    /// The `Ended` event's once it has arrived; before that, the latest reading seen. A
    /// straggling `Updated` that turns up afterwards does not overwrite it, or the billed
    /// energy would shrink — and could go negative — as late events landed.
    pub meter_stop_wh: Option<Decimal>,
    /// Every signed meter value the transaction's events carried, in arrival order.
    ///
    /// This is what a calibration-law billing chain settles on; the plain registers above are
    /// what the CSMS uses to operate. See the [module documentation](self).
    ///
    /// It is one list rather than a start and a stop because 1.6 does not give the two their
    /// own messages: a `StartTransaction` has nowhere to carry a signed record, so both the
    /// begin and the end record arrive together in `StopTransaction.transactionData`. Tell
    /// them apart with [`signed_with_context`](Self::signed_with_context).
    pub signed: Vec<SignedReading>,
    /// Whether any event was produced while the station was offline.
    pub had_offline_events: bool,
    seen: BTreeSet<i32>,
    /// `(kind, timestamp)` of every event taken through
    /// [`ingest_unsequenced`](Ledger::ingest_unsequenced), which is all 1.6 has to recognise
    /// a retry by.
    unsequenced: BTreeSet<(EventKind, DateTime)>,
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

    /// The signed meter values whose sample named `context` — `Transaction.Begin`,
    /// `Transaction.End`, and the rest of `ReadingContextEnumType`.
    ///
    /// The context is the sample's, not the signed record's own: an OCMF data set states its
    /// own begin/end marking inside the blob, and reading that is the consumer's business.
    pub fn signed_with_context<'a>(
        &'a self,
        context: &'a str,
    ) -> impl Iterator<Item = &'a SignedReading> {
        self.signed
            .iter()
            .filter(move |signed| signed.context.as_deref() == Some(context))
    }

    /// The energy delivered, in Wh, when both a start and a stop reading are known.
    ///
    /// Exact: the difference of two decimal registers, at the finer of their two scales. No
    /// rounding, no `f64`, and no drift of the kind that makes `10.1 - 0.1` come out as
    /// `10.000000000000002`.
    ///
    /// The result is negative if the stop register is below the start one, which means the
    /// meter was replaced or rolled over mid-transaction — a real condition, and one the
    /// caller should see rather than have silently clamped away. Read the [module
    /// documentation](self) before treating this as a billing basis.
    #[must_use]
    pub fn energy_wh(&self) -> Option<Decimal> {
        self.meter_stop_wh?.checked_sub(self.meter_start_wh?)
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
            evse_id: None,
            connector_id: None,
            started_at: None,
            ended_at: None,
            id_token: None,
            stopped_reason: None,
            meter_start_wh: None,
            meter_stop_wh: None,
            signed: Vec::new(),
            had_offline_events: false,
            seen: BTreeSet::new(),
            unsequenced: BTreeSet::new(),
            highest: -1,
        });

        if record.seen.contains(&event.seq_no) {
            return Ingested::Duplicate;
        }
        let after_end = record.ended_at.is_some();

        record.seen.insert(event.seq_no);
        record.highest = record.highest.max(event.seq_no);
        record.had_offline_events |= event.offline;
        // First one seen wins: a transaction happens at one point, and not every message
        // names it.
        if record.evse_id.is_none() {
            record.evse_id = event.evse_id;
        }
        if record.connector_id.is_none() {
            record.connector_id = event.connector_id;
        }
        // Kept whatever the event's kind and whenever it arrives: a signed record is the
        // billable value and there is no version of "too late" that makes it not one. The
        // duplicate check above is what stops a retry appending it twice.
        record.signed.extend(event.signed.iter().cloned());
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
    /// what 1.6 gives us to work with. It applies to a `MeterValues` retry exactly as it does
    /// to a re-sent `StartTransaction`: 1.6 messages carry no identity of their own, so two
    /// readings of the same kind bearing the same instant are one reading sent twice. A
    /// station that genuinely samples twice within the same second reports the same second,
    /// and a ledger that counted both would count a retry too.
    pub fn ingest_unsequenced(&mut self, event: &TransactionEvent) -> Ingested {
        let key = (event.identity.clone(), event.transaction_id.clone());
        let mark = (event.kind, event.timestamp);
        if self
            .records
            .get(&key)
            .is_some_and(|record| record.unsequenced.contains(&mark))
        {
            return Ingested::Duplicate;
        }
        let seq = self.next_seq.entry(key.clone()).or_insert(0);
        let mut event = event.clone();
        event.seq_no = *seq;
        *seq += 1;
        let outcome = self.ingest(&event);
        if let Some(record) = self.records.get_mut(&key) {
            record.unsequenced.insert(mark);
        }
        outcome
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
    use crate::decimal;

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
                .with_meter(decimal!(1000.0))
                .with_id_token("CARD-1"),
        );
        ledger.ingest(
            &event(1, EventKind::Updated)
                .with_meter(decimal!(4500.0))
                .offline(),
        );
        let mut ended = event(2, EventKind::Ended).with_meter(decimal!(7300.0));
        ended.stopped_reason = Some("EVDisconnected".into());
        ledger.ingest(&ended);

        let record = ledger.transaction(&station(), "tx-1").unwrap();
        assert!(!record.is_open());
        assert_eq!(record.energy_wh(), Some(decimal!(6300)));
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
        let start = TransactionEvent::new(station(), "42", 0, EventKind::Started, at(0))
            .with_meter(decimal!(100.0));
        assert_eq!(ledger.ingest_unsequenced(&start), Ingested::Applied);
        // 1.6 has no seqNo, so a re-sent StartTransaction is caught by its timestamp.
        assert_eq!(ledger.ingest_unsequenced(&start), Ingested::Duplicate);

        let meter = TransactionEvent::new(station(), "42", 0, EventKind::Updated, at(60))
            .with_meter(decimal!(2100.0));
        assert_eq!(ledger.ingest_unsequenced(&meter), Ingested::Applied);
        let stop = TransactionEvent::new(station(), "42", 0, EventKind::Ended, at(120))
            .with_meter(decimal!(3400.0));
        assert_eq!(ledger.ingest_unsequenced(&stop), Ingested::Applied);

        let record = ledger.transaction(&station(), "42").unwrap();
        assert_eq!(record.energy_wh(), Some(decimal!(3300)));
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
                .with_meter(decimal!(2100.0)),
        );
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 0, EventKind::Started, at(0))
                .with_meter(decimal!(1000.0)),
        );
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 2, EventKind::Ended, at(120))
                .with_meter(decimal!(3400.0)),
        );
        // And one more straggler lands after the transaction has already ended.
        ledger.ingest(
            &TransactionEvent::new(station(), "tx-1", 3, EventKind::Updated, at(90))
                .with_meter(decimal!(2800.0)),
        );

        let record = ledger.transaction(&station(), "tx-1").unwrap();
        assert_eq!(record.meter_start_wh, Some(decimal!(1000)));
        assert_eq!(record.meter_stop_wh, Some(decimal!(3400)));
        assert_eq!(record.energy_wh(), Some(decimal!(2400)));
    }

    /// 1.6 messages carry no identity of their own, so a retried `MeterValues` is recognised
    /// only by its kind and its instant. Counting it twice appends the same signed record
    /// twice — which is the one thing this ledger exists to stop.
    #[test]
    fn a_retried_16_meter_values_is_recognised_like_any_other_retry() {
        let mut ledger = Ledger::new();
        let start = TransactionEvent::new(station(), "42", 0, EventKind::Started, at(0));
        assert_eq!(ledger.ingest_unsequenced(&start), Ingested::Applied);

        let periodic = TransactionEvent::new(station(), "42", 0, EventKind::Updated, at(60))
            .with_meter(decimal!(2100));
        assert_eq!(ledger.ingest_unsequenced(&periodic), Ingested::Applied);
        assert_eq!(ledger.ingest_unsequenced(&periodic), Ingested::Duplicate);

        // A later reading is a different instant and goes in.
        let later = TransactionEvent::new(station(), "42", 0, EventKind::Updated, at(120))
            .with_meter(decimal!(2600));
        assert_eq!(ledger.ingest_unsequenced(&later), Ingested::Applied);

        let record = ledger.transaction(&station(), "42").unwrap();
        assert_eq!(
            record.events(),
            3,
            "the retry did not become a fourth event"
        );
        assert!(record.missing().is_empty());
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
