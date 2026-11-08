//! A `MessageStore` over SQLite that a station process and a metering daemon can share.
//!
//! ```text
//! cargo run --example sqlite_store
//! ```
//!
//! `FileStore` is one process, one file, and refuses a journal a second writer has touched.
//! A station whose OCPP stack and metering daemon are separate processes wants one durable
//! queue both can write to. That is what a database is for, and `MessageStore` is four
//! synchronous methods, so the store below is the whole integration.
//!
//! # The design
//!
//! One table. `seq` is `INTEGER PRIMARY KEY AUTOINCREMENT`, so sequence numbers are global
//! across every process and never reused, even after the newest row is acknowledged. The
//! payload is stored as its raw JSON text, so a replayed `TransactionEvent` is byte-identical
//! to the one first sent.
//!
//! - **WAL mode**, so a reader never blocks a writer, and `synchronous = FULL`, because
//!   `push` must not return until the row would survive a power cut.
//! - **`BEGIN IMMEDIATE`** for every write: the write lock is taken up front, so two
//!   processes queue for it instead of one failing a read-to-write upgrade with
//!   `SQLITE_BUSY` halfway through.
//! - **A busy timeout**, so waiting for the other process's transaction is a short wait,
//!   not an error. If it does expire the store returns a `StoreError`, which the engine
//!   reports as a store failure — loud, never a silent drop.
//!
//! # What "delivered exactly once" means here
//!
//! The trait has no "take". The engine reads `pending()` when it is built and whenever it is
//! told to adopt (below), keeps the messages in memory, sends them oldest first, and calls
//! `ack(seq)` only when the CSMS has answered. So:
//!
//! - every row is in the queue exactly once until it is acknowledged, and then never again;
//! - a crash between sending and acknowledging leaves the row in place, and the next engine
//!   replays it — at-least-once on the wire, which is what the CSMS's deduplication of
//!   `TransactionEvent` by `transactionId` and `seqNo` is for.
//!
//! # Many producers, one consumer
//!
//! Only the station's engine drains the queue. A second engine on the same table would send
//! every message twice, and nothing in the table could tell it not to, so the other process
//! gets a [`Producer`], which can push and nothing else. Wiring it into a station:
//!
//! ```text
//! // station process
//! let station = Station::builder()
//!     .identity("CS-0001")?
//!     .url(url)
//!     .store(SqliteStore::open("/var/lib/ocpp/queue.db")?)
//!     .handler(MyStation)
//!     .build()?;
//!
//! // metering-daemon process
//! let mut queue = Producer::open("/var/lib/ocpp/queue.db")?;
//! queue.push(&transaction_event)?;
//! ```
//!
//! A row the daemon pushes while the station runs is durable at once. It is sent when the
//! station adopts it: `Engine::adopt_pending` queues every row the engine does not hold yet,
//! and a Tokio station built with `.store_poll(interval)` does that on connect and every
//! interval. Adopting twice queues nothing twice.
//!
//! `main` below runs the whole scenario and asserts every invariant, so it fails loudly if
//! any of this stops being true: two real processes enqueue concurrently, the station's
//! engine drains the queue in order, a crash between send and acknowledge redelivers the
//! interrupted message, and a row the daemon pushes while the station runs is sent once,
//! without a restart.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ocpp_kit::engine::{
    Engine, EngineConfig, Input, Instant, MessageStore, Output, QueuedCall, Role, Seq, StoreError,
};
use ocpp_kit::message::MessageKind;
use ocpp_kit::{RawValue, Version};
use rusqlite::{Connection, TransactionBehavior, params};

/// How long a write waits for the other process's transaction before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS outbox (
        seq           INTEGER PRIMARY KEY AUTOINCREMENT,
        action        TEXT    NOT NULL,
        payload       TEXT    NOT NULL,
        kind          TEXT    NOT NULL CHECK (kind IN ('call', 'send')),
        attempts      INTEGER NOT NULL CHECK (attempts >= 0),
        transactional INTEGER NOT NULL CHECK (transactional IN (0, 1))
    ) STRICT;
";

// Taken by value so it slots straight into `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn db(error: rusqlite::Error) -> StoreError {
    StoreError::new(format!("sqlite: {error}"))
}

/// Opens a connection configured for sharing between processes.
fn connect(path: &Path) -> Result<Connection, StoreError> {
    let mut conn = Connection::open(path).map_err(db)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(db)?;
    // `journal_mode` answers with the mode it actually set; anything else is refused rather
    // than silently running without WAL (an in-memory or read-only database, say).
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(db)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::new(format!(
            "{} cannot use WAL (got journal_mode = {mode})",
            path.display()
        )));
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(db)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db)?;
    tx.execute_batch(SCHEMA).map_err(db)?;
    tx.commit().map_err(db)?;
    Ok(conn)
}

/// Appends one message in its own `BEGIN IMMEDIATE` transaction.
fn insert(conn: &mut Connection, entry: &QueuedCall) -> Result<Seq, StoreError> {
    let kind = match entry.kind {
        MessageKind::Call => "call",
        MessageKind::Send => "send",
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db)?;
    let seq: i64 = tx
        .query_row(
            "INSERT INTO outbox (action, payload, kind, attempts, transactional)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING seq",
            params![
                entry.action,
                entry.payload.get(),
                kind,
                entry.attempts,
                entry.transactional
            ],
            |row| row.get(0),
        )
        .map_err(db)?;
    tx.commit().map_err(db)?;
    Seq::try_from(seq).map_err(|_| StoreError::new(format!("negative seq {seq}")))
}

/// The queue as the station's engine sees it: the one consumer.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Opens (or creates) the queue at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Ok(Self {
            conn: connect(path.as_ref())?,
        })
    }

    /// Runs one write statement in a `BEGIN IMMEDIATE` transaction.
    fn write(&mut self, sql: &str, args: impl rusqlite::Params) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        tx.execute(sql, args).map_err(db)?;
        tx.commit().map_err(db)
    }
}

impl MessageStore for SqliteStore {
    fn push(&mut self, entry: &QueuedCall) -> Result<Seq, StoreError> {
        insert(&mut self.conn, entry)
    }

    fn pending(&self) -> Result<Vec<(Seq, QueuedCall)>, StoreError> {
        // One statement is one read snapshot: a push committed by the other process while
        // this runs is either wholly in the result or wholly not.
        let mut statement = self
            .conn
            .prepare(
                "SELECT seq, action, payload, kind, attempts, transactional
                 FROM outbox ORDER BY seq",
            )
            .map_err(db)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, bool>(5)?,
                ))
            })
            .map_err(db)?;
        let mut pending = Vec::new();
        for row in rows {
            let (seq, action, payload, kind, attempts, transactional) = row.map_err(db)?;
            let seq = Seq::try_from(seq).map_err(|_| StoreError::new("negative seq"))?;
            let payload = RawValue::from_string(payload).map_err(|error| {
                StoreError::new(format!("stored payload {seq} is not JSON: {error}"))
            })?;
            let kind = match kind.as_str() {
                "call" => MessageKind::Call,
                "send" => MessageKind::Send,
                other => return Err(StoreError::new(format!("stored kind {other:?}"))),
            };
            pending.push((
                seq,
                QueuedCall {
                    action,
                    payload,
                    kind,
                    attempts,
                    transactional,
                },
            ));
        }
        Ok(pending)
    }

    fn ack(&mut self, seq: Seq) -> Result<(), StoreError> {
        // Idempotent, like `MemStore`: acknowledging what is already gone is not an error.
        self.write(
            "DELETE FROM outbox WHERE seq = ?1",
            params![seq_param(seq)?],
        )
    }

    fn set_attempts(&mut self, seq: Seq, attempts: u32) -> Result<(), StoreError> {
        self.write(
            "UPDATE outbox SET attempts = ?2 WHERE seq = ?1",
            params![seq_param(seq)?, attempts],
        )
    }

    fn len(&self) -> Result<usize, StoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
            .map_err(db)?;
        usize::try_from(count).map_err(|_| StoreError::new(format!("row count {count}")))
    }
}

fn seq_param(seq: Seq) -> Result<i64, StoreError> {
    i64::try_from(seq).map_err(|_| StoreError::new(format!("seq {seq} out of range")))
}

/// The queue as any other process sees it: it may add to it, never drain it.
pub struct Producer {
    conn: Connection,
}

impl Producer {
    /// Opens (or creates) the queue at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Ok(Self {
            conn: connect(path.as_ref())?,
        })
    }

    /// Appends a message; durable when this returns.
    pub fn push(&mut self, entry: &QueuedCall) -> Result<Seq, StoreError> {
        insert(&mut self.conn, entry)
    }
}

// --- The demonstration -----------------------------------------------------------------

/// Messages the daemon pushes while the station is running.
const LIVE: u32 = 5;

/// Messages each process enqueues.
const PER_PRODUCER: u32 = 50;

/// Messages the station delivers before it "crashes" mid-flight.
const DELIVERED_BEFORE_CRASH: usize = 30;

/// A pause between pushes, so the two processes' writes interleave and contend.
const PUSH_PACE: Duration = Duration::from_millis(2);

/// A `TransactionEvent` tagged with which process queued it and its per-transaction `seqNo`.
fn transaction_event(transaction_id: &str, seq_no: u32) -> QueuedCall {
    let payload = format!(
        r#"{{"eventType":"Updated","timestamp":"2026-11-07T12:00:00Z","triggerReason":"MeterValuePeriodic","seqNo":{seq_no},"transactionInfo":{{"transactionId":"{transaction_id}"}}}}"#
    );
    QueuedCall {
        action: "TransactionEvent".into(),
        payload: RawValue::from_string(payload).expect("valid JSON"),
        kind: MessageKind::Call,
        attempts: 0,
        transactional: true,
    }
}

/// What a delivered frame was: its message id and its `(transactionId, seqNo)`.
fn parse_call(frame: &str) -> (String, String, u32) {
    let (kind, id, action, payload): (u8, String, String, serde_json::Value) =
        serde_json::from_str(frame).expect("a CALL frame");
    assert_eq!((kind, action.as_str()), (2, "TransactionEvent"));
    let transaction = payload["transactionInfo"]["transactionId"]
        .as_str()
        .expect("transactionId")
        .to_owned();
    let seq_no = u32::try_from(payload["seqNo"].as_u64().expect("seqNo")).expect("u32");
    (id, transaction, seq_no)
}

/// The station's engine, rebuilt from whatever the queue holds — what a reboot does.
fn boot_station(path: &Path) -> Result<Engine<SqliteStore>, Box<dyn std::error::Error>> {
    let mut engine = Engine::with_store(
        EngineConfig::new(Role::ChargingStation, Version::V2_1),
        SqliteStore::open(path)?,
    )?;
    engine.handle(
        Instant::ZERO,
        Input::Connected {
            version: Version::V2_1,
        },
    );
    // Nothing queued may go out before the CSMS accepts the boot.
    engine.call(
        Instant::ZERO,
        "BootNotification",
        RawValue::from_string(
            r#"{"reason":"PowerUp","chargingStation":{"model":"M","vendorName":"V"}}"#.into(),
        )?,
    )?;
    let boot_id = next_frame(&mut engine).expect("BootNotification is sent");
    let (_, boot_id, _, _): (u8, String, String, serde_json::Value) =
        serde_json::from_str(&boot_id)?;
    engine.handle(
        Instant::ZERO,
        Input::Received(&format!(
            r#"[3,"{boot_id}",{{"currentTime":"2026-11-07T12:00:00Z","interval":300,"status":"Accepted"}}]"#
        )),
    );
    Ok(engine)
}

/// The next frame the engine wants written, if any.
fn next_frame<S: MessageStore>(engine: &mut Engine<S>) -> Option<String> {
    engine.drain().into_iter().find_map(|output| match output {
        Output::Transmit(frame) => Some(frame),
        _ => None,
    })
}

fn produce(path: &Path, transaction_id: &str, count: u32) -> Result<(), StoreError> {
    let mut queue = Producer::open(path)?;
    for seq_no in 0..count {
        queue.push(&transaction_event(transaction_id, seq_no))?;
        std::thread::sleep(PUSH_PACE);
    }
    Ok(())
}

fn remove_database(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(file));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Re-invoked as the metering daemon: push into the shared queue and exit.
    let args: Vec<String> = std::env::args().collect();
    if let [_, role, path] = args.as_slice() {
        if role == "--metering-daemon" {
            produce(Path::new(path), "tx-meter", PER_PRODUCER)?;
            return Ok(());
        }
        if role == "--live-daemon" {
            produce(Path::new(path), "tx-live", LIVE)?;
            return Ok(());
        }
    }

    let path =
        std::env::temp_dir().join(format!("ocpp-kit-sqlite-store-{}.db", std::process::id()));
    remove_database(&path);
    let result = run(&path);
    remove_database(&path);
    result
}

fn run(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Two processes enqueue at the same time: this one (the station) and a child (the
    //    metering daemon). Both contend for the write lock; the busy timeout absorbs it.
    connect(path)?; // create the schema before the race, so neither side sees a half-made file
    let mut daemon = Command::new(std::env::current_exe()?)
        .arg("--metering-daemon")
        .arg(path)
        .spawn()?;
    let mut station_store = SqliteStore::open(path)?;
    for seq_no in 0..PER_PRODUCER {
        station_store.push(&transaction_event("tx-station", seq_no))?;
        std::thread::sleep(PUSH_PACE);
    }
    assert!(daemon.wait()?.success(), "the metering daemon failed");
    drop(station_store);

    let total = usize::try_from(2 * PER_PRODUCER)?;
    let queued = SqliteStore::open(path)?.pending()?;
    assert_eq!(
        queued.len(),
        total,
        "every push from both processes is queued once"
    );
    let order: Vec<(String, u32)> = queued
        .iter()
        .map(|(_, entry)| {
            let (_, transaction, seq_no) = parse_call(&format!(
                r#"[2,"x","TransactionEvent",{}]"#,
                entry.payload.get()
            ));
            (transaction, seq_no)
        })
        .collect();
    for producer in ["tx-station", "tx-meter"] {
        let seq_nos: Vec<u32> = order
            .iter()
            .filter(|(transaction, _)| transaction == producer)
            .map(|(_, seq_no)| *seq_no)
            .collect();
        assert_eq!(
            seq_nos,
            (0..PER_PRODUCER).collect::<Vec<_>>(),
            "{producer}'s messages keep the order it queued them in"
        );
    }
    let interleaved = order.windows(2).filter(|w| w[0].0 != w[1].0).count();
    println!("queued {total} messages from two processes ({interleaved} hand-overs between them)");

    // 2. The station drains the queue, one CALL at a time, acknowledging each answer —
    //    then crashes after sending a message but before the CSMS's answer arrives.
    let mut delivered: Vec<(String, u32)> = Vec::new();
    let mut engine = boot_station(path)?;
    assert_eq!(engine.store().len()?, total);
    let in_flight = loop {
        let frame = next_frame(&mut engine).expect("the queue is not empty yet");
        let (id, transaction, seq_no) = parse_call(&frame);
        if delivered.len() == DELIVERED_BEFORE_CRASH {
            break (transaction, seq_no);
        }
        delivered.push((transaction, seq_no));
        engine.handle(
            Instant::ZERO,
            Input::Received(&format!(r#"[3,"{id}",{{}}]"#)),
        );
    };
    drop(engine); // the crash: sent, never acknowledged
    println!(
        "delivered {DELIVERED_BEFORE_CRASH}, then crashed with {} {} in flight",
        in_flight.0, in_flight.1
    );

    // 3. After the reboot the interrupted message is replayed first, then the rest.
    let mut engine = boot_station(path)?;
    assert_eq!(engine.store().len()?, total - DELIVERED_BEFORE_CRASH);
    let mut first_after_reboot = None;
    while let Some(frame) = next_frame(&mut engine) {
        let (id, transaction, seq_no) = parse_call(&frame);
        first_after_reboot.get_or_insert((transaction.clone(), seq_no));
        delivered.push((transaction, seq_no));
        engine.handle(
            Instant::ZERO,
            Input::Received(&format!(r#"[3,"{id}",{{}}]"#)),
        );
    }
    assert_eq!(
        first_after_reboot.as_ref(),
        Some(&in_flight),
        "the message in flight at the crash is redelivered, first"
    );
    assert_eq!(
        delivered, order,
        "each message acknowledged exactly once, in queue order"
    );
    assert_eq!(
        engine.store().len()?,
        0,
        "every delivered message was acknowledged"
    );
    let unique: BTreeSet<_> = delivered.iter().collect();
    assert_eq!(unique.len(), total);
    println!(
        "rebooted: {} {} redelivered first; all {total} acknowledged once, in order; queue empty",
        in_flight.0, in_flight.1
    );

    live_delivery(path, &mut engine)
}

/// 4. The station keeps running while the daemon pushes more. Adopting picks the rows up
///    without a restart; adopting again finds nothing new.
fn live_delivery(
    path: &Path,
    engine: &mut Engine<SqliteStore>,
) -> Result<(), Box<dyn std::error::Error>> {
    assert!(
        Command::new(std::env::current_exe()?)
            .arg("--live-daemon")
            .arg(path)
            .status()?
            .success(),
        "the live daemon failed"
    );
    assert_eq!(engine.adopt_pending(Instant::ZERO)?, usize::try_from(LIVE)?);
    let mut live = Vec::new();
    while let Some(frame) = next_frame(engine) {
        assert_eq!(
            engine.adopt_pending(Instant::ZERO)?,
            0,
            "nothing is adopted twice"
        );
        let (id, transaction, seq_no) = parse_call(&frame);
        live.push((transaction, seq_no));
        engine.handle(
            Instant::ZERO,
            Input::Received(&format!(r#"[3,"{id}",{{}}]"#)),
        );
    }
    let expected: Vec<(String, u32)> = (0..LIVE).map(|n| ("tx-live".to_owned(), n)).collect();
    assert_eq!(live, expected, "sent live, once each, in push order");
    assert_eq!(engine.store().len()?, 0);
    println!("live: {LIVE} rows pushed while running were adopted and sent once, in order");
    Ok(())
}
