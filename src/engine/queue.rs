//! The outgoing-call queue and its durable backing store.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use serde_json::value::RawValue;

use crate::message::MessageKind;

/// Monotonic sequence number of a stored message.
pub type Seq = u64;

/// One queued outgoing message.
#[derive(Clone, Debug)]
pub struct QueuedCall {
    /// Action name.
    pub action: String,
    /// Serialized payload object.
    pub payload: Box<RawValue>,
    /// `CALL` or `SEND`.
    pub kind: MessageKind,
    /// How many times transmission has already been attempted.
    pub attempts: u32,
    /// Whether the message is transaction-related and therefore durable and retried.
    pub transactional: bool,
}

impl PartialEq for QueuedCall {
    fn eq(&self, other: &Self) -> bool {
        self.action == other.action
            && self.payload.get() == other.payload.get()
            && self.kind == other.kind
            && self.attempts == other.attempts
            && self.transactional == other.transactional
    }
}

/// Durable storage for transaction-related messages.
///
/// A Charging Station must survive a power cut with its queued `TransactionEvent`s intact
/// and replay them in order (E04.FR.01–03, E08.FR.05–07, E12.FR.01–02), and must be able to
/// answer `GetTransactionStatus.messagesInQueue` from it. The trait is deliberately tiny and
/// synchronous so it can sit on flash, SQLite or Postgres alike.
pub trait MessageStore {
    /// Appends a message, returning its sequence number.
    fn push(&mut self, entry: &QueuedCall) -> Result<Seq, StoreError>;

    /// Every un-acknowledged message, oldest first.
    ///
    /// Called once when an engine starts, so a Charging Station replays what a power cut
    /// interrupted.
    fn pending(&self) -> Result<Vec<(Seq, QueuedCall)>, StoreError>;

    /// Removes a message that has been delivered (or definitively abandoned).
    fn ack(&mut self, seq: Seq) -> Result<(), StoreError>;

    /// Records a further transmission attempt for a message that is still queued.
    fn set_attempts(&mut self, seq: Seq, attempts: u32) -> Result<(), StoreError>;

    /// How many messages are waiting.
    ///
    /// Answers `GetTransactionStatus.messagesInQueue`.
    fn len(&self) -> Result<usize, StoreError>;

    /// Whether the store is empty.
    fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.len()? == 0)
    }
}

/// A durable store failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    /// What went wrong, for logging.
    pub reason: String,
}

impl StoreError {
    /// Builds a store failure.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for StoreError {}

/// An in-memory [`MessageStore`]. The default, and all a CSMS ever needs.
///
/// A Charging Station should swap in a store that survives a reboot.
#[derive(Debug, Default)]
pub struct MemStore {
    entries: VecDeque<(Seq, QueuedCall)>,
    next_seq: Seq,
    capacity: Option<usize>,
}

impl MemStore {
    /// An unbounded in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that refuses to grow beyond `capacity` messages.
    #[must_use]
    pub fn bounded(capacity: usize) -> Self {
        Self {
            capacity: Some(capacity),
            ..Self::default()
        }
    }

    /// Every queued message, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<&QueuedCall> {
        self.entries.iter().map(|(_, entry)| entry).collect()
    }
}

impl MessageStore for MemStore {
    fn push(&mut self, entry: &QueuedCall) -> Result<Seq, StoreError> {
        if let Some(capacity) = self.capacity {
            if self.entries.len() >= capacity {
                return Err(StoreError::new("offline queue is full"));
            }
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back((seq, entry.clone()));
        Ok(seq)
    }

    fn pending(&self) -> Result<Vec<(Seq, QueuedCall)>, StoreError> {
        Ok(self.entries.iter().cloned().collect())
    }

    fn ack(&mut self, seq: Seq) -> Result<(), StoreError> {
        self.entries.retain(|(s, _)| *s != seq);
        Ok(())
    }

    fn set_attempts(&mut self, seq: Seq, attempts: u32) -> Result<(), StoreError> {
        if let Some((_, entry)) = self.entries.iter_mut().find(|(s, _)| *s == seq) {
            entry.attempts = attempts;
        }
        Ok(())
    }

    fn len(&self) -> Result<usize, StoreError> {
        Ok(self.entries.len())
    }
}

/// How often, and how far apart, transaction-related messages are retried.
///
/// Both 1.6 (§3.7.1, `TransactionMessageAttempts` / `TransactionMessageRetryInterval`) and
/// 2.x (`OCPPCommCtrlr.MessageAttempts[TransactionEvent]` /
/// `MessageAttemptInterval[TransactionEvent]`) prescribe the same **linear** schedule: wait
/// `interval × number of preceding transmissions` before each retry. It is not exponential,
/// and it applies to transaction messages only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total number of transmissions, including the first. `1` disables retrying.
    pub attempts: u32,
    /// The base interval.
    pub interval: Duration,
}

impl Default for RetryPolicy {
    /// The OCPP 1.6 defaults: three attempts, 60 s apart.
    fn default() -> Self {
        Self {
            attempts: 3,
            interval: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// Retrying disabled.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            attempts: 1,
            interval: Duration::ZERO,
        }
    }

    /// The wait before the retry that follows `attempts_made` transmissions.
    ///
    /// `interval × attempts_made`, per 1.6 §3.7.1 ("wait the retry interval multiplied by
    /// the number of preceding transmissions").
    #[must_use]
    pub fn delay_after(&self, attempts_made: u32) -> Duration {
        self.interval.saturating_mul(attempts_made)
    }

    /// Whether another transmission is allowed after `attempts_made`.
    #[must_use]
    pub const fn may_retry(&self, attempts_made: u32) -> bool {
        attempts_made < self.attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_schedule_is_linear_not_exponential() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay_after(1).as_secs(), 60);
        assert_eq!(policy.delay_after(2).as_secs(), 120);
        assert_eq!(policy.delay_after(3).as_secs(), 180);
        assert!(policy.may_retry(2));
        assert!(!policy.may_retry(3));
    }

    #[test]
    fn bounded_store_refuses_to_grow_without_limit() {
        let mut store = MemStore::bounded(1);
        let entry = QueuedCall {
            action: "TransactionEvent".into(),
            payload: RawValue::from_string("{}".into()).unwrap(),
            kind: MessageKind::Call,
            attempts: 0,
            transactional: true,
        };
        assert!(store.push(&entry).is_ok());
        assert!(store.push(&entry).is_err());
        assert_eq!(store.len().unwrap(), 1);
    }
}
