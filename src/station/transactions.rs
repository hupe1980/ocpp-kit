//! The OCPP 2.x transaction state machine (functional block E).
//!
//! What starts and stops a transaction in 2.x is not a message but a *condition*.
//! `TxCtrlr.TxStartPoint` and `TxCtrlr.TxStopPoint` each name a **set** of conditions, and the
//! specification writes one independent `SHALL` per member — six for starting (E01.FR.01–06),
//! seven for stopping (E06.FR.01–07).
//!
//! The set is therefore a **disjunction**: the *first* configured condition to arrive starts
//! the transaction, and the *first* to disappear ends it. That is what makes the
//! specification's own recommended configuration work — start points `EVConnected` and
//! `Authorized` together, "such that upon authorization first, the charger is already seen as
//! 'in use'" — and what the E02 sequence diagram shows: `Started` on cable plug-in, `Updated`
//! when authorization follows.
//!
//! Two subtleties that a level-triggered reading gets wrong:
//!
//! * **Stopping is a transition, not a level.** E06.FR.02 says "connection … *is lost*", not
//!   "no connection". The specification's own warning depends on it: with start point
//!   `ParkingBayOccupancy` and stop point `EVConnected`, "when the user never connects the
//!   EV, but simply drives away, then the transaction will remain open". A condition that
//!   never held cannot stop holding.
//! * **`PowerPathClosed` is derived, not reported.** E01.FR.05 defines its precondition as
//!   *authorized* **and** *connected to the EV*, and E06.FR.06 as *connection lost* **or**
//!   *authorization ended* — exactly the negation. It is computed from the other two, so it
//!   cannot be set inconsistently with them.
//!
//! Getting `seqNo` right matters just as much: it is what lets the CSMS detect a gap after
//! an offline period, and it must be strictly increasing per transaction.
//!
//! This machine owns those rules and nothing else. It emits *descriptions* of the events to
//! send, so the caller builds the version's own `TransactionEventRequest`.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::types::DateTime;

/// One condition that can gate the start or the end of a transaction
/// (`TxStartStopPointEnumType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TxPoint {
    /// A vehicle is detected in the parking bay.
    ParkingBayOccupancy,
    /// The cable is plugged into the EV.
    EVConnected,
    /// An `IdToken` has been authorized.
    Authorized,
    /// Signed meter data is available.
    DataSigned,
    /// The contactor is closed — power can flow.
    PowerPathClosed,
    /// Energy is actually being transferred.
    EnergyTransfer,
}

impl TxPoint {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TxPoint::ParkingBayOccupancy => "ParkingBayOccupancy",
            TxPoint::EVConnected => "EVConnected",
            TxPoint::Authorized => "Authorized",
            TxPoint::DataSigned => "DataSigned",
            TxPoint::PowerPathClosed => "PowerPathClosed",
            TxPoint::EnergyTransfer => "EnergyTransfer",
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "ParkingBayOccupancy" => TxPoint::ParkingBayOccupancy,
            "EVConnected" => TxPoint::EVConnected,
            "Authorized" => TxPoint::Authorized,
            "DataSigned" => TxPoint::DataSigned,
            "PowerPathClosed" => TxPoint::PowerPathClosed,
            "EnergyTransfer" => TxPoint::EnergyTransfer,
            _ => return None,
        })
    }

    /// Parses the comma-separated `MemberList` the device model stores.
    #[must_use]
    pub fn parse_list(value: &str) -> BTreeSet<TxPoint> {
        value
            .split(',')
            .filter_map(|item| TxPoint::from_wire(item.trim()))
            .collect()
    }
}

impl fmt::Display for TxPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the EVSE currently observes.
///
/// Five facts about the physical world, not six: `PowerPathClosed` is *defined* by two of
/// the others (E01.FR.05, E06.FR.06) and so is derived by [`holds`](Self::holds) rather than
/// reported, which removes the possibility of setting it inconsistently with them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Conditions {
    /// A vehicle is in the parking bay.
    pub parking_bay_occupied: bool,
    /// The cable is connected to the EV.
    pub ev_connected: bool,
    /// An `IdToken` is authorized.
    pub authorized: bool,
    /// Signed meter data is available.
    pub data_signed: bool,
    /// Energy is flowing.
    pub energy_transfer: bool,
}

impl Conditions {
    /// Whether the named condition holds.
    #[must_use]
    pub const fn holds(&self, point: TxPoint) -> bool {
        match point {
            TxPoint::ParkingBayOccupancy => self.parking_bay_occupied,
            TxPoint::EVConnected => self.ev_connected,
            TxPoint::Authorized => self.authorized,
            TxPoint::DataSigned => self.data_signed,
            // E01.FR.05: "The EV Driver is authorized AND the Charging Station has connection
            // with the EV". E06.FR.06 is its exact negation: "connection … lost OR
            // authorization has ended".
            TxPoint::PowerPathClosed => self.authorized && self.ev_connected,
            TxPoint::EnergyTransfer => self.energy_transfer,
        }
    }
}

/// What the transaction machine wants sent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxEvent {
    /// `TransactionEvent(Started)`.
    Started {
        /// The transaction id, generated by the station.
        transaction_id: String,
        /// Always `0`.
        seq_no: i32,
        /// Why it started.
        trigger: &'static str,
        /// When.
        timestamp: DateTime,
    },
    /// `TransactionEvent(Updated)`.
    Updated {
        /// The transaction id.
        transaction_id: String,
        /// The next sequence number.
        seq_no: i32,
        /// Why the update is being sent.
        trigger: &'static str,
        /// When.
        timestamp: DateTime,
    },
    /// `TransactionEvent(Ended)`.
    Ended {
        /// The transaction id.
        transaction_id: String,
        /// The final sequence number.
        seq_no: i32,
        /// Why it stopped.
        trigger: &'static str,
        /// The `stoppedReason`.
        stopped_reason: &'static str,
        /// When.
        timestamp: DateTime,
    },
}

impl TxEvent {
    /// The `seqNo` this event carries.
    #[must_use]
    pub const fn seq_no(&self) -> i32 {
        match self {
            TxEvent::Started { seq_no, .. }
            | TxEvent::Updated { seq_no, .. }
            | TxEvent::Ended { seq_no, .. } => *seq_no,
        }
    }

    /// The transaction it belongs to.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        match self {
            TxEvent::Started { transaction_id, .. }
            | TxEvent::Updated { transaction_id, .. }
            | TxEvent::Ended { transaction_id, .. } => transaction_id,
        }
    }
}

/// Generates transaction ids.
///
/// E01.FR.08 is unusually explicit about what "unique" means here: *"unique for each
/// transaction started by that Charging Station, even when the Charging Station is rebooted,
/// repaired, firmware is updated etc., it SHALL ensure that it never generates the same
/// `TransactionId` twice"* — and §1.2 of the E block recommends UUIDs by name. A duplicate
/// not a cosmetic problem: the CSMS bills by transaction id, and two different charging
/// sessions sharing one is a billing error nobody notices until an invoice is disputed.
pub trait TransactionIds: Send {
    /// Produces an id this station has never used and never will again.
    fn next(&mut self) -> String;
}

/// Version 4 UUIDs — what E block §1.2 recommends, and the only generator here that
/// satisfies E01.FR.08 on its own.
///
/// [`next`](TransactionIds::next) cannot fail, so an entropy source that is unavailable is
/// covered by a per-process counter rather than by a constant. That trades "unique for ever"
/// for "unique until the next reboot"; it does not trade it for a duplicate, which is the one
/// outcome E01.FR.08 forbids outright.
#[cfg(feature = "getrandom")]
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomTransactionIds {
    fallback: u64,
}

#[cfg(feature = "getrandom")]
impl RandomTransactionIds {
    /// A generator drawing from the operating system's entropy source.
    #[must_use]
    pub fn new() -> Self {
        Self { fallback: 0 }
    }
}

#[cfg(feature = "getrandom")]
impl TransactionIds for RandomTransactionIds {
    fn next(&mut self) -> String {
        if let Some(id) = crate::types::uuid_v4() {
            return id;
        }
        self.fallback += 1;
        alloc::format!("degraded-{}", self.fallback)
    }
}

/// Ids of the form `<prefix><counter>`, for targets with no entropy source.
///
/// The counter restarts at 1 in every process, so this satisfies E01.FR.08 **only if the
/// prefix is different after every reboot** — a value from persistent storage, a boot
/// counter, anything that does not repeat. With a constant prefix a power cut makes the
/// station re-issue `tx-1`, and the CSMS has two unrelated charging sessions filed under one
/// transaction. Prefer [`RandomTransactionIds`] wherever it is available.
#[derive(Clone, Debug)]
pub struct CounterTransactionIds {
    prefix: String,
    next: u64,
}

impl CounterTransactionIds {
    /// Starts a generator. Read the type's documentation first: the prefix is what makes the
    /// ids unique across reboots.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            next: 1,
        }
    }
}

impl TransactionIds for CounterTransactionIds {
    fn next(&mut self) -> String {
        let id = alloc::format!("{}{}", self.prefix, self.next);
        self.next += 1;
        id
    }
}

/// Tracks one EVSE's transaction.
pub struct TransactionMachine {
    start_points: BTreeSet<TxPoint>,
    stop_points: BTreeSet<TxPoint>,
    ids: alloc::boxed::Box<dyn TransactionIds>,
    conditions: Conditions,
    active: Option<Active>,
}

struct Active {
    id: String,
    next_seq: i32,
}

impl fmt::Debug for TransactionMachine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransactionMachine")
            .field("start_points", &self.start_points)
            .field("stop_points", &self.stop_points)
            .field("conditions", &self.conditions)
            .field(
                "transaction",
                &self.active.as_ref().map(|active| &active.id),
            )
            .finish_non_exhaustive()
    }
}

impl TransactionMachine {
    /// Builds a machine from `TxCtrlr.TxStartPoint` and `TxCtrlr.TxStopPoint`.
    #[must_use]
    pub fn new(
        start_points: BTreeSet<TxPoint>,
        stop_points: BTreeSet<TxPoint>,
        ids: alloc::boxed::Box<dyn TransactionIds>,
    ) -> Self {
        Self {
            start_points,
            stop_points,
            ids,
            conditions: Conditions::default(),
            active: None,
        }
    }

    /// The configuration Table 62 gives for OCPP 1.6-compatible transactions:
    /// `TxStartPoint = PowerPathClosed`, `TxStopPoint = EVConnected, Authorized`.
    ///
    /// A transaction starts once the driver is authorized *and* the cable is in, and ends
    /// when either goes away — which is what 1.6's `StartTransaction` / `StopTransaction`
    /// meant in practice.
    #[must_use]
    pub fn with_defaults(ids: alloc::boxed::Box<dyn TransactionIds>) -> Self {
        Self::new(
            BTreeSet::from([TxPoint::PowerPathClosed]),
            BTreeSet::from([TxPoint::EVConnected, TxPoint::Authorized]),
            ids,
        )
    }

    /// The transaction currently running, if any.
    #[must_use]
    pub fn transaction_id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.id.as_str())
    }

    /// The conditions as last reported.
    #[must_use]
    pub const fn conditions(&self) -> Conditions {
        self.conditions
    }

    /// Reports what the EVSE now observes and returns the events that follow.
    ///
    /// Call it whenever any of the five facts in [`Conditions`] changes; the machine compares
    /// against the previous call, which is what makes both rules transitions rather than
    /// levels:
    ///
    /// * **Start** (E01.FR.01–06) — the *first* configured start point to begin holding
    ///   starts a transaction. A point that was already holding when the previous
    ///   transaction ended does not start another one, so unplugging a cable under
    ///   `TxStopPoint = Authorized` does not silently open a second session.
    /// * **Stop** (E06.FR.01–07) — the *first* configured stop point to stop holding ends
    ///   it. A point that never held cannot stop holding, which is precisely why the
    ///   specification's `ParkingBayOccupancy` / `EVConnected` warning behaves as it says it
    ///   does: the transaction stays open.
    pub fn observe(&mut self, conditions: Conditions, now: DateTime) -> Vec<TxEvent> {
        let previous = self.conditions;
        self.conditions = conditions;
        let mut events = Vec::new();

        match &mut self.active {
            None => {
                let started = self
                    .start_points
                    .iter()
                    .copied()
                    .find(|point| !previous.holds(*point) && conditions.holds(*point));
                if started.is_some() {
                    let id = self.ids.next();
                    events.push(TxEvent::Started {
                        transaction_id: id.clone(),
                        seq_no: 0,
                        trigger: trigger_for(previous, conditions),
                        timestamp: now,
                    });
                    self.active = Some(Active { id, next_seq: 1 });
                }
            }
            Some(active) => {
                let stopped = self
                    .stop_points
                    .iter()
                    .copied()
                    .find(|point| previous.holds(*point) && !conditions.holds(*point));
                if let Some(point) = stopped {
                    let seq_no = active.next_seq;
                    let id = active.id.clone();
                    self.active = None;
                    events.push(TxEvent::Ended {
                        transaction_id: id,
                        seq_no,
                        trigger: trigger_for(previous, conditions),
                        stopped_reason: stopped_reason(point, previous, conditions),
                        timestamp: now,
                    });
                } else if previous != conditions {
                    let seq_no = active.next_seq;
                    active.next_seq += 1;
                    events.push(TxEvent::Updated {
                        transaction_id: active.id.clone(),
                        seq_no,
                        trigger: trigger_for(previous, conditions),
                        timestamp: now,
                    });
                }
            }
        }
        events
    }

    /// Produces an `Updated` event for something the machine does not track itself — a
    /// periodic meter value, a charging-state change, a remote stop request.
    ///
    /// Returns `None` when no transaction is running.
    pub fn update(&mut self, trigger: &'static str, now: DateTime) -> Option<TxEvent> {
        let active = self.active.as_mut()?;
        let seq_no = active.next_seq;
        active.next_seq += 1;
        Some(TxEvent::Updated {
            transaction_id: active.id.clone(),
            seq_no,
            trigger,
            timestamp: now,
        })
    }

    /// Ends the transaction for a reason outside the stop points — `Remote`,
    /// `DeAuthorized`, `EmergencyStop`, a reset.
    pub fn abort(&mut self, stopped_reason: &'static str, now: DateTime) -> Option<TxEvent> {
        let active = self.active.take()?;
        Some(TxEvent::Ended {
            transaction_id: active.id,
            seq_no: active.next_seq,
            // E06.FR.16 names this value exactly; "Abnormal" is not a TriggerReasonEnumType
            // member and a conforming CSMS answers a PropertyConstraintViolation to it.
            trigger: "AbnormalCondition",
            stopped_reason,
            timestamp: now,
        })
    }

    /// Restores a transaction that was running before a reboot, so `seqNo` continues where it
    /// left off instead of restarting and confusing the CSMS's ledger.
    ///
    /// `conditions` are the ones that held when the station went down. They matter: the stop
    /// rule is a transition, so a machine that resumed believing nothing held would never
    /// notice the cable being pulled and would leave the transaction open for ever.
    pub fn resume(
        &mut self,
        transaction_id: impl Into<String>,
        next_seq: i32,
        conditions: Conditions,
    ) {
        self.conditions = conditions;
        self.active = Some(Active {
            id: transaction_id.into(),
            next_seq,
        });
    }
}

/// The `triggerReason` that best explains a condition change.
///
/// Every value here is a `TriggerReasonEnumType` member; `trigger_reasons_are_defined` in the
/// tests below re-checks that against the generated enumeration, because a `&'static str`
/// that is one letter off only fails at the far end, as a `PropertyConstraintViolation` on a
/// transaction message.
fn trigger_for(before: Conditions, after: Conditions) -> &'static str {
    if before.authorized != after.authorized {
        // The two spellings differ, and the specification means them to: `Authorized` when it
        // begins, `Deauthorized` when it ends.
        if after.authorized {
            "Authorized"
        } else {
            "Deauthorized"
        }
    } else if before.ev_connected != after.ev_connected {
        if after.ev_connected {
            "CablePluggedIn"
        } else {
            "EVCommunicationLost"
        }
    } else if before.parking_bay_occupied != after.parking_bay_occupied {
        if after.parking_bay_occupied {
            "EVDetected"
        } else {
            "EVDeparted"
        }
    } else if before.data_signed != after.data_signed && after.data_signed {
        "SignedDataReceived"
    } else if before.energy_transfer != after.energy_transfer {
        "ChargingStateChanged"
    } else {
        "Trigger"
    }
}

/// The `stoppedReason` implied by the stop point that stopped holding.
///
/// `PowerPathClosed` is derived from two conditions (E06.FR.06), so which of them actually
/// went away decides the reason — reporting `StoppedByEV` for a revoked authorization would
/// tell the CSMS the wrong story about why a session ended.
fn stopped_reason(point: TxPoint, before: Conditions, after: Conditions) -> &'static str {
    match point {
        TxPoint::EVConnected | TxPoint::ParkingBayOccupancy => "EVDisconnected",
        TxPoint::Authorized => "DeAuthorized",
        TxPoint::PowerPathClosed => {
            if before.ev_connected && !after.ev_connected {
                "EVDisconnected"
            } else {
                "DeAuthorized"
            }
        }
        TxPoint::EnergyTransfer => "StoppedByEV",
        // No ReasonEnumType member describes "the meter stopped signing", and inventing one
        // would be worse than saying so.
        TxPoint::DataSigned => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    fn now() -> DateTime {
        DateTime::parse("2024-01-01T00:00:00Z").unwrap()
    }

    fn machine() -> TransactionMachine {
        TransactionMachine::with_defaults(Box::new(CounterTransactionIds::new("tx-")))
    }

    fn points(list: &[TxPoint]) -> BTreeSet<TxPoint> {
        list.iter().copied().collect()
    }

    /// The specification's own "time of use" recommendation: *"the start points should be
    /// `EVConnected`, `Authorized` … such that upon authorization first, the charger is
    /// seen as 'in use'"*. Reading the set as a conjunction would hold the transaction back
    /// until both arrived, which is the opposite of what that sentence asks for.
    #[test]
    fn the_first_start_point_to_arrive_starts_the_transaction_e01_fr_02() {
        let mut machine = TransactionMachine::new(
            points(&[TxPoint::EVConnected, TxPoint::Authorized]),
            points(&[TxPoint::EVConnected]),
            Box::new(CounterTransactionIds::new("tx-")),
        );

        // Cable in, nobody authorized yet — E01.FR.02 fires on its own.
        let plugged = Conditions {
            ev_connected: true,
            ..Conditions::default()
        };
        let events = machine.observe(plugged, now());
        assert!(
            matches!(
                &events[0],
                TxEvent::Started {
                    seq_no: 0,
                    trigger: "CablePluggedIn",
                    ..
                }
            ),
            "{events:?}"
        );

        // Authorization follows, exactly as the E02 sequence diagram shows: an Updated, not a
        // second Started.
        let authorized = Conditions {
            authorized: true,
            ..plugged
        };
        let events = machine.observe(authorized, now());
        assert!(
            matches!(
                &events[0],
                TxEvent::Updated {
                    seq_no: 1,
                    trigger: "Authorized",
                    ..
                }
            ),
            "{events:?}"
        );
    }

    /// E01.FR.05 defines `PowerPathClosed` as *authorized AND connected to the EV*, and
    /// E06.FR.06 as the negation of that. Table 62 uses it for 1.6-compatible transactions.
    #[test]
    fn power_path_closed_is_derived_from_authorization_and_connection_e01_fr_05() {
        let mut machine = machine();

        let plugged = Conditions {
            ev_connected: true,
            ..Conditions::default()
        };
        assert!(
            machine.observe(plugged, now()).is_empty(),
            "a cable alone does not close the power path"
        );

        let authorized = Conditions {
            authorized: true,
            ..plugged
        };
        let events = machine.observe(authorized, now());
        assert!(
            matches!(&events[0], TxEvent::Started { seq_no: 0, .. }),
            "{events:?}"
        );

        // E06.FR.06's other half: authorization ending stops it just as unplugging would, and
        // the reason has to say which of the two it was.
        let deauthorized = Conditions {
            authorized: false,
            ..authorized
        };
        let events = machine.observe(deauthorized, now());
        assert!(
            matches!(
                &events[0],
                TxEvent::Ended {
                    stopped_reason: "DeAuthorized",
                    trigger: "Deauthorized",
                    ..
                }
            ),
            "{events:?}"
        );
    }

    /// The specification's warning, verbatim: *"when the start point is
    /// `ParkingBayOccupancy` and the stop point is `EVConnected`, then a transaction starts when an EV occupies the
    /// parking bay, but when the user never connects the EV, but simply drives away, then the
    /// transaction will remain open"*. A level-triggered stop rule would end it immediately,
    /// since `EVConnected` never held.
    #[test]
    fn a_stop_point_that_never_held_cannot_end_the_transaction_e06_fr_02() {
        let mut machine = TransactionMachine::new(
            points(&[TxPoint::ParkingBayOccupancy]),
            points(&[TxPoint::EVConnected]),
            Box::new(CounterTransactionIds::new("tx-")),
        );

        let occupied = Conditions {
            parking_bay_occupied: true,
            ..Conditions::default()
        };
        assert!(matches!(
            machine.observe(occupied, now())[0],
            TxEvent::Started { .. }
        ));

        // The driver leaves without ever plugging in.
        let empty = Conditions::default();
        let events = machine.observe(empty, now());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TxEvent::Ended { .. })),
            "the transaction must remain open: {events:?}"
        );
        assert!(machine.transaction_id().is_some());
    }

    /// A start point still holding after a transaction ends must not open another one — the
    /// cable is in the same socket it was in a moment ago, and nothing new has happened.
    #[test]
    fn a_finished_transaction_does_not_restart_from_a_condition_that_never_changed() {
        let mut machine = TransactionMachine::new(
            points(&[TxPoint::EVConnected]),
            points(&[TxPoint::Authorized]),
            Box::new(CounterTransactionIds::new("tx-")),
        );

        let plugged = Conditions {
            ev_connected: true,
            ..Conditions::default()
        };
        assert!(matches!(
            machine.observe(plugged, now())[0],
            TxEvent::Started { .. }
        ));
        let authorized = Conditions {
            authorized: true,
            ..plugged
        };
        machine.observe(authorized, now());
        let events = machine.observe(plugged, now());
        assert!(matches!(&events[0], TxEvent::Ended { .. }), "{events:?}");
        assert!(machine.transaction_id().is_none());

        // The cable has not moved. Nothing should start.
        assert!(machine.observe(plugged, now()).is_empty());

        // Re-plugging it is a real event, and does.
        machine.observe(Conditions::default(), now());
        assert!(matches!(
            machine.observe(plugged, now())[0],
            TxEvent::Started { .. }
        ));
    }

    #[test]
    fn sequence_numbers_are_strictly_increasing_within_a_transaction() {
        let mut machine = machine();
        let start = Conditions {
            ev_connected: true,
            authorized: true,
            ..Conditions::default()
        };
        let events = machine.observe(start, now());
        assert_eq!(events[0].seq_no(), 0);

        assert_eq!(
            machine
                .update("MeterValuePeriodic", now())
                .unwrap()
                .seq_no(),
            1
        );
        assert_eq!(
            machine
                .update("MeterValuePeriodic", now())
                .unwrap()
                .seq_no(),
            2
        );

        let unplugged = Conditions {
            ev_connected: false,
            ..start
        };
        let events = machine.observe(unplugged, now());
        assert!(
            matches!(
                &events[0],
                TxEvent::Ended {
                    seq_no: 3,
                    stopped_reason: "EVDisconnected",
                    ..
                }
            ),
            "{events:?}"
        );
        assert_eq!(machine.transaction_id(), None);
        assert!(machine.update("MeterValuePeriodic", now()).is_none());
    }

    #[test]
    fn a_reboot_resumes_the_sequence_and_the_conditions_it_left_behind() {
        let mut machine = machine();
        let live = Conditions {
            ev_connected: true,
            authorized: true,
            ..Conditions::default()
        };
        machine.resume("tx-7", 12, live);
        assert_eq!(machine.transaction_id(), Some("tx-7"));

        let event = machine.update("MeterValuePeriodic", now()).unwrap();
        assert_eq!(event.seq_no(), 12);
        assert_eq!(event.transaction_id(), "tx-7");

        // The conditions came back with it, so the cable coming out is still a transition.
        let events = machine.observe(
            Conditions {
                ev_connected: false,
                ..live
            },
            now(),
        );
        assert!(matches!(&events[0], TxEvent::Ended { .. }), "{events:?}");
    }

    #[test]
    fn an_abort_ends_the_transaction_with_its_own_reason() {
        let mut machine = machine();
        machine.observe(
            Conditions {
                ev_connected: true,
                authorized: true,
                ..Conditions::default()
            },
            now(),
        );
        let event = machine.abort("Remote", now()).unwrap();
        assert!(
            matches!(
                event,
                TxEvent::Ended {
                    stopped_reason: "Remote",
                    trigger: "AbnormalCondition",
                    ..
                }
            ),
            "{event:?}"
        );
        assert!(machine.abort("Remote", now()).is_none());
    }

    #[test]
    fn tx_points_parse_from_the_device_model_member_list() {
        let points = TxPoint::parse_list("Authorized, PowerPathClosed");
        assert_eq!(
            points,
            BTreeSet::from([TxPoint::Authorized, TxPoint::PowerPathClosed])
        );
    }

    /// The machine emits `triggerReason` and `stoppedReason` as `&'static str`, so nothing in
    /// the type system stops a typo. This checks every value it can produce against the
    /// generated enumerations, which come from the schemas.
    #[cfg(feature = "v2_1")]
    #[test]
    fn every_trigger_and_reason_is_a_defined_enumeration_value() {
        use crate::v2_1::{Reason, TriggerReason};

        let all = [
            Conditions::default(),
            Conditions {
                parking_bay_occupied: true,
                ..Conditions::default()
            },
            Conditions {
                ev_connected: true,
                ..Conditions::default()
            },
            Conditions {
                ev_connected: true,
                authorized: true,
                ..Conditions::default()
            },
            Conditions {
                data_signed: true,
                ..Conditions::default()
            },
            Conditions {
                energy_transfer: true,
                ..Conditions::default()
            },
        ];
        for before in all {
            for after in all {
                let trigger = trigger_for(before, after);
                assert!(
                    !matches!(
                        serde_json::from_str::<TriggerReason>(&alloc::format!("\"{trigger}\"")),
                        Ok(TriggerReason::UnknownValue(_)) | Err(_)
                    ),
                    "{trigger:?} is not a TriggerReasonEnumType value"
                );
                for point in [
                    TxPoint::ParkingBayOccupancy,
                    TxPoint::EVConnected,
                    TxPoint::Authorized,
                    TxPoint::DataSigned,
                    TxPoint::PowerPathClosed,
                    TxPoint::EnergyTransfer,
                ] {
                    let reason = stopped_reason(point, before, after);
                    assert!(
                        !matches!(
                            serde_json::from_str::<Reason>(&alloc::format!("\"{reason}\"")),
                            Ok(Reason::UnknownValue(_)) | Err(_)
                        ),
                        "{reason:?} is not a ReasonEnumType value"
                    );
                }
            }
        }
        // And the one the machine emits without consulting the conditions (E06.FR.16).
        assert!(!matches!(
            serde_json::from_str::<TriggerReason>("\"AbnormalCondition\""),
            Ok(TriggerReason::UnknownValue(_)) | Err(_)
        ));
    }
}
