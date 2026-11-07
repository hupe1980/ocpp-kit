//! A [`Ledger`] that survives a CSMS restart.
//!
//! A restart is exactly when a station's retries arrive: it timed out waiting for the
//! answer to a `TransactionEvent` because the CSMS went away, reconnects, and sends the event
//! again. An in-memory ledger has forgotten it ever saw the first copy, so the retry is
//! billed twice and the gap state for every open transaction is gone.
//!
//! The ledger is a deterministic fold over the events it was given, so durability needs no
//! schema of its own: [`DurableLedger`] writes each input to a [`LedgerStore`] **before**
//! applying it, and rebuilding is a replay. A [`checkpoint`](DurableLedger::checkpoint)
//! replaces the journal with a [`LedgerSnapshot`]. Replaying an event the snapshot already
//! contains is harmless — it is a duplicate, and the ledger says so — which is what makes a
//! crash between writing a snapshot and clearing the journal safe.
//!
//! The trait has the shape of [`MessageStore`](crate::engine::MessageStore): three synchronous
//! methods, so a CSMS with a database implements it there and inherits real transactions.
//! [`MemLedgerStore`] is for tests; [`FileLedgerStore`] (with `std`) is an append-only
//! journal and a snapshot file, both written with `sync_data` and atomic renames.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::ledger::{Ingested, Ledger, LedgerSnapshot, TransactionEvent};
use crate::engine::StoreError;
use crate::types::DateTime;

/// One input to the ledger, as journaled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[non_exhaustive]
pub enum JournalEntry {
    /// [`Ledger::ingest`].
    Ingest {
        /// The event.
        event: TransactionEvent,
    },
    /// [`Ledger::ingest_unsequenced`].
    IngestUnsequenced {
        /// The event.
        event: TransactionEvent,
    },
    /// [`Ledger::prune_ended_before`].
    PruneEndedBefore {
        /// The cutoff.
        cutoff: DateTime,
    },
}

impl JournalEntry {
    fn apply(&self, ledger: &mut Ledger) {
        match self {
            JournalEntry::Ingest { event } => {
                let _ = ledger.ingest(event);
            }
            JournalEntry::IngestUnsequenced { event } => {
                let _ = ledger.ingest_unsequenced(event);
            }
            JournalEntry::PruneEndedBefore { cutoff } => {
                ledger.prune_ended_before(*cutoff);
            }
        }
    }
}

/// Where a [`DurableLedger`] keeps its state.
pub trait LedgerStore {
    /// Everything stored: the last checkpoint, if any, and every entry appended since, in
    /// order.
    ///
    /// # Errors
    ///
    /// The store could not be read, or what it holds is not a ledger.
    fn load(&mut self) -> Result<(Option<LedgerSnapshot>, Vec<JournalEntry>), StoreError>;

    /// Appends one entry. Returns only once it is durable.
    ///
    /// # Errors
    ///
    /// The entry could not be made durable; the ledger then does not apply it.
    fn append(&mut self, entry: &JournalEntry) -> Result<(), StoreError>;

    /// Replaces the stored state with `snapshot` and an empty journal.
    ///
    /// A crash part-way may leave the new snapshot *and* the old journal; replaying that is
    /// correct, because every old entry is then a duplicate.
    ///
    /// # Errors
    ///
    /// The snapshot could not be made durable.
    fn checkpoint(&mut self, snapshot: &LedgerSnapshot) -> Result<(), StoreError>;
}

/// A [`LedgerStore`] in memory: durable for exactly as long as the process. For tests, and
/// for showing what a real store has to do.
#[derive(Clone, Debug, Default)]
pub struct MemLedgerStore {
    snapshot: Option<LedgerSnapshot>,
    journal: Vec<JournalEntry>,
}

impl MemLedgerStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries have been appended since the last checkpoint.
    #[must_use]
    pub fn journal_len(&self) -> usize {
        self.journal.len()
    }
}

impl LedgerStore for MemLedgerStore {
    fn load(&mut self) -> Result<(Option<LedgerSnapshot>, Vec<JournalEntry>), StoreError> {
        Ok((self.snapshot.clone(), self.journal.clone()))
    }

    fn append(&mut self, entry: &JournalEntry) -> Result<(), StoreError> {
        self.journal.push(entry.clone());
        Ok(())
    }

    fn checkpoint(&mut self, snapshot: &LedgerSnapshot) -> Result<(), StoreError> {
        self.snapshot = Some(snapshot.clone());
        self.journal.clear();
        Ok(())
    }
}

/// A [`Ledger`] whose every input is made durable before it is applied.
///
/// ```
/// use ocpp_kit::csms::ledger::{EventKind, Ingested, TransactionEvent};
/// use ocpp_kit::csms::ledger_store::{DurableLedger, MemLedgerStore};
/// use ocpp_kit::types::{DateTime, Identity};
///
/// let station = Identity::new("CS-0001").unwrap();
/// let at = DateTime::parse("2026-11-07T10:00:00Z").unwrap();
/// let started = TransactionEvent::new(station, "tx-1", 0, EventKind::Started, at);
///
/// let mut ledger = DurableLedger::open(MemLedgerStore::new()).unwrap();
/// assert_eq!(ledger.ingest(&started).unwrap(), Ingested::Applied);
///
/// // The CSMS restarts. The store is all that survives — and it is enough.
/// let store = ledger.into_store();
/// let mut ledger = DurableLedger::open(store).unwrap();
/// assert_eq!(ledger.ingest(&started).unwrap(), Ingested::Duplicate);
/// ```
#[derive(Debug)]
pub struct DurableLedger<S> {
    ledger: Ledger,
    store: S,
}

impl<S: LedgerStore> DurableLedger<S> {
    /// Rebuilds the ledger from `store`: the last checkpoint, then every entry since.
    ///
    /// # Errors
    ///
    /// The store could not be read.
    pub fn open(mut store: S) -> Result<Self, StoreError> {
        let (snapshot, journal) = store.load()?;
        let mut ledger = snapshot.map_or_else(Ledger::new, Ledger::from_snapshot);
        for entry in &journal {
            entry.apply(&mut ledger);
        }
        Ok(Self { ledger, store })
    }

    /// [`Ledger::ingest`], durably. A duplicate is answered without touching the store: it
    /// adds nothing to the state.
    ///
    /// # Errors
    ///
    /// The store refused the entry. Nothing was applied; answer the station with an error
    /// so it retries, rather than acknowledging an event that a restart would forget.
    pub fn ingest(&mut self, event: &TransactionEvent) -> Result<Ingested, StoreError> {
        if self.ledger.is_duplicate(event) {
            return Ok(Ingested::Duplicate);
        }
        self.store.append(&JournalEntry::Ingest {
            event: event.clone(),
        })?;
        Ok(self.ledger.ingest(event))
    }

    /// [`Ledger::ingest_unsequenced`], durably.
    ///
    /// # Errors
    ///
    /// As for [`ingest`](Self::ingest).
    pub fn ingest_unsequenced(&mut self, event: &TransactionEvent) -> Result<Ingested, StoreError> {
        if self.ledger.is_duplicate_unsequenced(event) {
            return Ok(Ingested::Duplicate);
        }
        self.store.append(&JournalEntry::IngestUnsequenced {
            event: event.clone(),
        })?;
        Ok(self.ledger.ingest_unsequenced(event))
    }

    /// [`Ledger::prune_ended_before`], durably.
    ///
    /// # Errors
    ///
    /// The store refused the entry; nothing was pruned.
    pub fn prune_ended_before(&mut self, cutoff: DateTime) -> Result<usize, StoreError> {
        self.store
            .append(&JournalEntry::PruneEndedBefore { cutoff })?;
        Ok(self.ledger.prune_ended_before(cutoff))
    }

    /// Writes the whole state as one snapshot and empties the journal, so the next
    /// [`open`](Self::open) replays nothing.
    ///
    /// # Errors
    ///
    /// The store could not write the snapshot; the journal is still intact.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.store.checkpoint(&self.ledger.snapshot())
    }

    /// The ledger, for reading.
    #[must_use]
    pub const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The store.
    #[must_use]
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Gives the store back.
    #[must_use]
    pub fn into_store(self) -> S {
        self.store
    }
}

#[cfg(feature = "std")]
pub use file::FileLedgerStore;

#[cfg(feature = "std")]
mod file {
    use alloc::format;
    use alloc::vec::Vec;

    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};

    use super::{JournalEntry, LedgerSnapshot, LedgerStore, StoreError};

    /// A [`LedgerStore`] in a directory: `snapshot.json` and an append-only `journal.jsonl`.
    ///
    /// Every append is flushed with `sync_data` before it returns, and a checkpoint writes
    /// the snapshot to a temporary file and renames it into place before truncating the
    /// journal the same way — so a crash at any point leaves a state [`DurableLedger::open`]
    /// rebuilds correctly. A partial final journal line (a crash mid-append) is discarded:
    /// it was never reported as written. One process per directory.
    ///
    /// [`DurableLedger::open`]: super::DurableLedger::open
    #[derive(Debug)]
    pub struct FileLedgerStore {
        dir: PathBuf,
        journal: File,
    }

    impl FileLedgerStore {
        /// Opens (or creates) the store in `dir`, which must exist.
        ///
        /// # Errors
        ///
        /// The journal could not be opened.
        pub fn open(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
            let dir = dir.as_ref().to_path_buf();
            let journal = Self::open_journal(&dir)?;
            Ok(Self { dir, journal })
        }

        fn journal_path(dir: &Path) -> PathBuf {
            dir.join("journal.jsonl")
        }

        fn snapshot_path(dir: &Path) -> PathBuf {
            dir.join("snapshot.json")
        }

        fn open_journal(dir: &Path) -> Result<File, StoreError> {
            let path = Self::journal_path(dir);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|error| io(&path, &error))
        }

        /// Writes `bytes` to `path` through a temporary file and an atomic rename.
        fn replace(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
            let temporary = path.with_extension("tmp");
            let mut file = File::create(&temporary).map_err(|error| io(&temporary, &error))?;
            file.write_all(bytes)
                .and_then(|()| file.sync_data())
                .map_err(|error| io(&temporary, &error))?;
            drop(file);
            std::fs::rename(&temporary, path).map_err(|error| io(path, &error))
        }
    }

    impl LedgerStore for FileLedgerStore {
        fn load(&mut self) -> Result<(Option<LedgerSnapshot>, Vec<JournalEntry>), StoreError> {
            let snapshot_path = Self::snapshot_path(&self.dir);
            let snapshot = match std::fs::read(&snapshot_path) {
                Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|error| {
                    StoreError::new(format!(
                        "{}: not a ledger snapshot: {error}",
                        snapshot_path.display()
                    ))
                })?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(io(&snapshot_path, &error)),
            };

            let journal_path = Self::journal_path(&self.dir);
            let mut entries = Vec::new();
            let mut torn = false;
            let file = File::open(&journal_path).map_err(|error| io(&journal_path, &error))?;
            for line in BufReader::new(file).lines() {
                let Ok(text) = line else {
                    torn = true;
                    break;
                };
                if text.trim().is_empty() {
                    continue;
                }
                // Append-only: a line that does not parse can only be a crash mid-write, and
                // nothing after it exists.
                let Ok(entry) = serde_json::from_str::<JournalEntry>(&text) else {
                    torn = true;
                    break;
                };
                entries.push(entry);
            }
            if torn {
                // Cut the torn tail off before anything is appended after it: the next entry
                // would otherwise continue the half line, and the replay after the *next*
                // restart would stop there and lose it.
                let mut clean = Vec::new();
                for entry in &entries {
                    serde_json::to_writer(&mut clean, entry).map_err(|error| {
                        StoreError::new(format!("unserializable entry: {error}"))
                    })?;
                    clean.push(b'\n');
                }
                Self::replace(&journal_path, &clean)?;
                self.journal = Self::open_journal(&self.dir)?;
            }
            Ok((snapshot, entries))
        }

        fn append(&mut self, entry: &JournalEntry) -> Result<(), StoreError> {
            let path = Self::journal_path(&self.dir);
            let mut line = serde_json::to_string(entry)
                .map_err(|error| StoreError::new(format!("unserializable entry: {error}")))?;
            line.push('\n');
            self.journal
                .write_all(line.as_bytes())
                .and_then(|()| self.journal.sync_data())
                .map_err(|error| io(&path, &error))
        }

        fn checkpoint(&mut self, snapshot: &LedgerSnapshot) -> Result<(), StoreError> {
            let bytes = serde_json::to_vec(snapshot)
                .map_err(|error| StoreError::new(format!("unserializable snapshot: {error}")))?;
            Self::replace(&Self::snapshot_path(&self.dir), &bytes)?;
            // Only now may the journal go: until the rename, it is what holds the state.
            Self::replace(&Self::journal_path(&self.dir), b"")?;
            self.journal = Self::open_journal(&self.dir)?;
            Ok(())
        }
    }

    fn io(path: &Path, error: &std::io::Error) -> StoreError {
        StoreError::new(format!("{}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csms::ledger::EventKind;
    use crate::decimal;
    use crate::types::Identity;

    fn station() -> Identity {
        Identity::new("CS-0001").unwrap()
    }

    fn at(text: &str) -> DateTime {
        DateTime::parse(text).unwrap()
    }

    fn session() -> [TransactionEvent; 3] {
        [
            TransactionEvent::new(
                station(),
                "tx-1",
                0,
                EventKind::Started,
                at("2026-11-07T10:00:00Z"),
            )
            .with_meter(decimal!(1000.0)),
            TransactionEvent::new(
                station(),
                "tx-1",
                1,
                EventKind::Updated,
                at("2026-11-07T10:30:00Z"),
            )
            .with_meter(decimal!(1500.0)),
            TransactionEvent::new(
                station(),
                "tx-1",
                2,
                EventKind::Ended,
                at("2026-11-07T11:00:00Z"),
            )
            .with_meter(decimal!(2100.0)),
        ]
    }

    #[test]
    fn a_retry_after_a_restart_is_still_a_duplicate() {
        let mut ledger = DurableLedger::open(MemLedgerStore::new()).unwrap();
        for event in &session()[..2] {
            assert_eq!(ledger.ingest(event).unwrap(), Ingested::Applied);
        }
        let mut ledger = DurableLedger::open(ledger.into_store()).unwrap();
        assert_eq!(ledger.ingest(&session()[1]).unwrap(), Ingested::Duplicate);
        assert_eq!(ledger.ingest(&session()[2]).unwrap(), Ingested::Applied);
        let record = ledger.ledger().transaction(&station(), "tx-1").unwrap();
        assert_eq!(record.energy().unwrap().unverified_wh(), decimal!(1100.0));
    }

    #[test]
    fn a_duplicate_is_not_journaled() {
        let mut ledger = DurableLedger::open(MemLedgerStore::new()).unwrap();
        let event = &session()[0];
        let _ = ledger.ingest(event).unwrap();
        let _ = ledger.ingest(event).unwrap();
        assert_eq!(ledger.store().journal_len(), 1);
    }

    #[test]
    fn replaying_the_journal_rebuilds_exactly_the_live_state() {
        let mut ledger = DurableLedger::open(MemLedgerStore::new()).unwrap();
        let unsequenced = TransactionEvent::new(
            station(),
            "tx-16",
            0,
            EventKind::Started,
            at("2026-11-07T09:00:00Z"),
        );
        let _ = ledger.ingest_unsequenced(&unsequenced).unwrap();
        for event in &session() {
            let _ = ledger.ingest(event).unwrap();
        }
        let _ = ledger
            .prune_ended_before(at("2026-11-07T10:30:00Z"))
            .unwrap();
        let live = ledger.ledger().clone();
        let rebuilt = DurableLedger::open(ledger.into_store()).unwrap();
        assert_eq!(rebuilt.ledger(), &live);
    }

    #[test]
    fn a_crash_between_snapshot_and_journal_truncation_replays_correctly() {
        // The new snapshot was written, the old journal was not yet cleared.
        let mut ledger = DurableLedger::open(MemLedgerStore::new()).unwrap();
        for event in &session() {
            let _ = ledger.ingest(event).unwrap();
        }
        let live = ledger.ledger().clone();
        let mut store = ledger.into_store();
        let (_, journal) = store.load().unwrap();
        store.checkpoint(&live.snapshot()).unwrap();
        for entry in &journal {
            store.append(entry).unwrap();
        }
        let rebuilt = DurableLedger::open(store).unwrap();
        assert_eq!(rebuilt.ledger(), &live);
    }

    #[cfg(feature = "std")]
    #[test]
    fn the_file_store_survives_a_restart_a_checkpoint_and_a_torn_line() {
        let dir = std::env::temp_dir().join(format!(
            "ocpp-kit-ledger-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut ledger = DurableLedger::open(FileLedgerStore::open(&dir).unwrap()).unwrap();
        let _ = ledger.ingest(&session()[0]).unwrap();
        ledger.checkpoint().unwrap();
        let _ = ledger.ingest(&session()[1]).unwrap();
        drop(ledger);

        // A crash mid-append leaves half a line; it was never reported as written.
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("journal.jsonl"))
            .unwrap();
        std::io::Write::write_all(&mut journal, br#"{"op":"ingest","event":{"ident"#).unwrap();
        drop(journal);

        let mut ledger = DurableLedger::open(FileLedgerStore::open(&dir).unwrap()).unwrap();
        assert_eq!(ledger.ingest(&session()[0]).unwrap(), Ingested::Duplicate);
        assert_eq!(ledger.ingest(&session()[1]).unwrap(), Ingested::Duplicate);
        assert_eq!(ledger.ingest(&session()[2]).unwrap(), Ingested::Applied);
        drop(ledger);

        // And what was appended after the torn line survives the *next* restart too.
        let mut ledger = DurableLedger::open(FileLedgerStore::open(&dir).unwrap()).unwrap();
        assert_eq!(ledger.ingest(&session()[2]).unwrap(), Ingested::Duplicate);
        let record = ledger.ledger().transaction(&station(), "tx-1").unwrap();
        assert!(!record.is_open());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
