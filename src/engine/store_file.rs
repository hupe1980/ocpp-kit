//! A durable [`MessageStore`] backed by an append-only journal.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::queue::{MessageStore, QueuedCall, Seq, StoreError};

/// One line of the journal.
///
/// A record per *change*, not a snapshot per write: appending a line and flushing it is one
/// sequential write, which is what makes the cost of durability bearable on the flash a
/// charging station actually has.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Record {
    /// A message was queued.
    Push {
        seq: Seq,
        #[serde(flatten)]
        entry: StoredCall,
    },
    /// A message was delivered or definitively abandoned.
    Ack { seq: Seq },
    /// A further transmission was attempted.
    Attempts { seq: Seq, attempts: u32 },
}

/// A [`QueuedCall`] in a form that survives a round trip through JSON.
#[derive(Clone, Serialize, Deserialize)]
struct StoredCall {
    action: String,
    /// The payload as its raw JSON text, so it is byte-identical after a reboot — a
    /// re-serialized payload could differ, and a `TransactionEvent` that changes shape between
    /// attempts is one a CSMS cannot deduplicate.
    payload: String,
    send: bool,
    attempts: u32,
    transactional: bool,
}

impl StoredCall {
    fn from_call(entry: &QueuedCall) -> Self {
        Self {
            action: entry.action.clone(),
            payload: entry.payload.get().to_string(),
            send: entry.kind == crate::message::MessageKind::Send,
            attempts: entry.attempts,
            transactional: entry.transactional,
        }
    }

    fn into_call(self) -> Result<QueuedCall, StoreError> {
        let payload = serde_json::value::RawValue::from_string(self.payload)
            .map_err(|error| StoreError::new(format!("stored payload is not JSON: {error}")))?;
        Ok(QueuedCall {
            action: self.action,
            payload,
            kind: if self.send {
                crate::message::MessageKind::Send
            } else {
                crate::message::MessageKind::Call
            },
            attempts: self.attempts,
            transactional: self.transactional,
        })
    }
}

/// How much of the journal may be dead records before it is rewritten.
///
/// A station queues and acknowledges the same handful of messages for years, so without
/// compaction the file grows without bound while the live set stays tiny.
const COMPACT_RATIO: usize = 4;

/// The smallest journal worth rewriting. Below this, compaction costs more than it saves.
const COMPACT_FLOOR: usize = 64;

/// A [`MessageStore`] that survives a power cut, for the station side.
///
/// An append-only journal, one line of JSON per change. A Charging Station must replay the
/// transaction messages an outage interrupted (E04.FR.01–03, E08.FR.05–07, E12.FR.01–02),
/// which [`MemStore`](super::MemStore) cannot.
///
/// ```no_run
/// use ocpp_kit::Version;
/// use ocpp_kit::engine::{Engine, EngineConfig, FileStore, Role};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // Whatever the outage interrupted is back in the queue by the time this returns.
/// let store = FileStore::open("/var/lib/ocpp/queue.jsonl")?;
/// let engine = Engine::with_store(
///     EngineConfig::new(Role::ChargingStation, Version::V2_1),
///     store,
/// )?;
/// # let _ = engine;
/// # Ok(()) }
/// ```
///
/// # Guarantees
///
/// [`push`](MessageStore::push) returns only once the record has reached the device
/// (`sync_data`). A crash mid-append leaves a partial final line, discarded on the next open:
/// it described a message that was never reported as queued.
///
/// Not concurrent — one process, one file — and no defence against a corrupt filesystem. For
/// anything else, implement [`MessageStore`]; it is four synchronous methods.
pub struct FileStore {
    path: PathBuf,
    journal: File,
    live: BTreeMap<Seq, QueuedCall>,
    next_seq: Seq,
    /// Records written since the last compaction, live or not.
    written: usize,
}

impl fmt::Debug for FileStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileStore")
            .field("path", &self.path)
            .field("queued", &self.live.len())
            .finish_non_exhaustive()
    }
}

impl FileStore {
    /// Opens (or creates) the journal at `path`, replaying whatever an outage left behind.
    ///
    /// The parent directory must exist. Replaying compacts: the file that comes back holds
    /// exactly the messages that are still queued.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let (live, next_seq) = Self::replay(&path)?;

        // Replaced immediately by `rewrite`; opening it here keeps the field non-optional.
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| io(&path, &error))?;
        let mut store = Self {
            path,
            journal,
            live,
            next_seq,
            written: 0,
        };
        store.rewrite()?;
        Ok(store)
    }

    /// The journal's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rewrites the journal so it holds only what is still queued.
    ///
    /// Called on open and whenever the dead records outnumber the live ones; exposed because a
    /// long-running station may want to schedule it rather than pay for it mid-transaction.
    pub fn compact(&mut self) -> Result<(), StoreError> {
        self.rewrite()
    }

    /// Replays a journal into the set of messages still queued.
    ///
    /// A line that does not parse ends the replay rather than being skipped. A journal is
    /// append-only, so the only way to reach one is a crash mid-write, and everything after
    /// such a line is by definition not there — treating it as a gap to step over would be
    /// inventing an ordering the file does not have.
    fn replay(path: &Path) -> Result<(BTreeMap<Seq, QueuedCall>, Seq), StoreError> {
        let mut live: BTreeMap<Seq, QueuedCall> = BTreeMap::new();
        let mut next_seq: Seq = 0;

        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((live, next_seq));
            }
            Err(error) => return Err(io(path, &error)),
        };

        for read in BufReader::new(file).lines() {
            let Ok(text) = read else { break };
            if text.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Record>(&text) else {
                break;
            };
            match record {
                Record::Push { seq, entry } => {
                    next_seq = next_seq.max(seq + 1);
                    live.insert(seq, entry.into_call()?);
                }
                Record::Ack { seq } => {
                    next_seq = next_seq.max(seq + 1);
                    live.remove(&seq);
                }
                Record::Attempts { seq, attempts } => {
                    next_seq = next_seq.max(seq + 1);
                    if let Some(entry) = live.get_mut(&seq) {
                        entry.attempts = attempts;
                    }
                }
            }
        }
        Ok((live, next_seq))
    }

    /// Writes a fresh journal containing only the live entries, then swaps it in.
    fn rewrite(&mut self) -> Result<(), StoreError> {
        let temporary = self.path.with_extension("compacting");
        let mut file = File::create(&temporary).map_err(|error| io(&temporary, &error))?;
        for (seq, entry) in &self.live {
            let record = Record::Push {
                seq: *seq,
                entry: StoredCall::from_call(entry),
            };
            write_record(&mut file, &temporary, &record)?;
        }
        file.sync_data().map_err(|error| io(&temporary, &error))?;
        drop(file);

        // `rename` replaces atomically, so a crash here leaves either the old journal or the
        // new one — never a half-written mixture.
        std::fs::rename(&temporary, &self.path).map_err(|error| io(&self.path, &error))?;
        self.journal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| io(&self.path, &error))?;
        self.written = self.live.len();
        Ok(())
    }

    /// Appends one record and flushes it to the device.
    fn append(&mut self, record: &Record) -> Result<(), StoreError> {
        write_record(&mut self.journal, &self.path, record)?;
        // The whole point: a message the engine is told is queued is on the device, not in a
        // page cache that a power cut discards.
        self.journal
            .sync_data()
            .map_err(|error| io(&self.path, &error))?;
        self.written += 1;
        Ok(())
    }

    /// Rewrites the journal once the dead records have come to outweigh the live ones.
    fn compact_if_worthwhile(&mut self) -> Result<(), StoreError> {
        if self.written >= COMPACT_FLOOR && self.written >= self.live.len() * COMPACT_RATIO {
            self.rewrite()?;
        }
        Ok(())
    }
}

impl MessageStore for FileStore {
    fn push(&mut self, entry: &QueuedCall) -> Result<Seq, StoreError> {
        let seq = self.next_seq;
        self.append(&Record::Push {
            seq,
            entry: StoredCall::from_call(entry),
        })?;
        self.next_seq += 1;
        self.live.insert(seq, entry.clone());
        Ok(seq)
    }

    fn pending(&self) -> Result<Vec<(Seq, QueuedCall)>, StoreError> {
        Ok(self
            .live
            .iter()
            .map(|(seq, entry)| (*seq, entry.clone()))
            .collect())
    }

    fn ack(&mut self, seq: Seq) -> Result<(), StoreError> {
        if self.live.remove(&seq).is_none() {
            return Ok(());
        }
        self.append(&Record::Ack { seq })?;
        self.compact_if_worthwhile()
    }

    fn set_attempts(&mut self, seq: Seq, attempts: u32) -> Result<(), StoreError> {
        let Some(entry) = self.live.get_mut(&seq) else {
            return Ok(());
        };
        entry.attempts = attempts;
        self.append(&Record::Attempts { seq, attempts })?;
        self.compact_if_worthwhile()
    }

    fn len(&self) -> Result<usize, StoreError> {
        Ok(self.live.len())
    }
}

fn write_record(file: &mut File, path: &Path, record: &Record) -> Result<(), StoreError> {
    let mut line = serde_json::to_vec(record)
        .map_err(|error| StoreError::new(format!("could not encode a journal record: {error}")))?;
    line.push(b'\n');
    file.write_all(&line).map_err(|error| io(path, &error))
}

fn io(path: &Path, error: &std::io::Error) -> StoreError {
    StoreError::new(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageKind;
    use serde_json::value::RawValue;

    fn call(seq_no: u32) -> QueuedCall {
        QueuedCall {
            action: "TransactionEvent".into(),
            payload: RawValue::from_string(format!(r#"{{"seqNo":{seq_no}}}"#)).unwrap(),
            kind: MessageKind::Call,
            attempts: 0,
            transactional: true,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ocpp-kit-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole reason this type exists: E04/E08/E12 require the messages an outage
    /// interrupted to be replayed, in order, after the station comes back.
    #[test]
    fn a_power_cut_does_not_lose_what_was_queued() {
        let path = temp_dir().join("replay.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut store = FileStore::open(&path).unwrap();
        let first = store.push(&call(0)).unwrap();
        store.push(&call(1)).unwrap();
        store.push(&call(2)).unwrap();
        store.ack(first).unwrap();
        store.set_attempts(1, 2).unwrap();
        // The power goes out here: no close, no flush of our own beyond what `push` did.
        drop(store);

        let store = FileStore::open(&path).unwrap();
        let pending = store.pending().unwrap();
        assert_eq!(
            pending.len(),
            2,
            "the acknowledged one is gone, the rest stay"
        );
        assert_eq!(pending[0].0, 1, "and they come back oldest first");
        assert_eq!(pending[0].1.attempts, 2, "with the attempts they had made");
        assert!(pending[0].1.payload.get().contains(r#""seqNo":1"#));
        assert_eq!(pending[1].0, 2);
        std::fs::remove_file(&path).unwrap();
    }

    /// A crash *during* an append leaves a partial line. It described a message that was never
    /// reported as queued, so discarding it loses nothing anyone was told about — and stopping
    /// there rather than skipping it keeps the replay from inventing an order the file does
    /// not have.
    #[test]
    fn a_torn_final_record_is_discarded_rather_than_poisoning_the_queue() {
        let path = temp_dir().join("torn.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut store = FileStore::open(&path).unwrap();
        store.push(&call(0)).unwrap();
        store.push(&call(1)).unwrap();
        drop(store);

        // Simulate the tear: half a record, as a power cut mid-`write_all` would leave.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"op":"push","seq":2,"action":"Transact"#)
            .unwrap();
        drop(file);

        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.len().unwrap(), 2);
        std::fs::remove_file(&path).unwrap();
    }

    /// A station queues and acknowledges the same few messages for years. Without compaction
    /// the journal grows without bound while the live set stays tiny.
    #[test]
    fn the_journal_does_not_grow_without_bound() {
        let path = temp_dir().join("compact.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut store = FileStore::open(&path).unwrap();
        for _ in 0..200 {
            let seq = store.push(&call(0)).unwrap();
            store.ack(seq).unwrap();
        }
        assert_eq!(store.len().unwrap(), 0);

        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            lines <= COMPACT_FLOOR * 2,
            "400 records should have been compacted away, {lines} lines remain"
        );

        // And the compacted file still replays correctly.
        store.push(&call(9)).unwrap();
        drop(store);
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.len().unwrap(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_send_keeps_its_kind_across_a_reboot() {
        let path = temp_dir().join("kinds.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut store = FileStore::open(&path).unwrap();
        let mut send = call(0);
        send.kind = MessageKind::Send;
        store.push(&send).unwrap();
        drop(store);

        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.pending().unwrap()[0].1.kind, MessageKind::Send);
        std::fs::remove_file(&path).unwrap();
    }
}
