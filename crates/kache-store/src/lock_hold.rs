//! A log line for each long hold of the index's write lock.
//!
//! The index is one SQLite file in WAL mode: one writer at a time, and a
//! writer that finds it busy polls rather than queues. In the index storm of
//! 11-21 the logs said who waited for the lock and gave up, but not who held
//! it meanwhile. Every writable index connection reports here
//! ([`watch`]): the hold starts when a statement leaves the connection
//! holding the write lock (`BEGIN IMMEDIATE`, or a deferred transaction's
//! first write) and ends when one leaves it without (`COMMIT`, `ROLLBACK`).
//! A hold of [`LONG_LOCK_HOLD`] or more is logged, once, with who held it:
//! the process, its thread, the transaction's first write, how many
//! statements it ran and how much of the hold those statements took inside
//! SQLite (the rest ran between them: the holder's own work, its file system
//! calls among them). A write outside a transaction holds the lock only
//! while it runs and is not seen.
//!
//! It costs one check of the connection's transaction state per statement,
//! and nothing waits on it.

use std::time::{Duration, Instant};

use rusqlite::Connection;
use rusqlite::trace::{TraceEvent, TraceEventCodes};

/// The shortest hold of the index's write lock that is logged.
pub const LONG_LOCK_HOLD: Duration = Duration::from_secs(1);

/// The log target a wrapper writes to its log file even without file
/// logging turned on (the binary's `ALWAYS_LOGGED`): most index writers in
/// a storm are wrappers.
const ALWAYS_LOGGED: &str = "kache::always_logged";

/// The longest first write a hold's line quotes.
const FIRST_WRITE_CHARS: usize = 72;

/// One long hold of the write lock, as its line reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LongHold {
    /// From the statement that took the lock to the one that let it go.
    pub held: Duration,
    /// The thread that held it.
    pub thread: String,
    /// The transaction's first write statement (its start), or its first
    /// statement when it wrote nothing.
    pub first_write: String,
    /// Statements the transaction ran while it held the lock.
    pub statements: u32,
    /// How long those statements ran inside SQLite, the one that let the
    /// lock go included: `held` minus this ran between them.
    pub in_statements: Duration,
}

/// Log each hold of the write lock on `db` that lasts [`LONG_LOCK_HOLD`],
/// and tell the connection's turn at the lock (when it has one) each time a
/// statement ends.
pub(crate) fn watch(
    db: &mut Connection,
    turn: Option<std::sync::Arc<crate::index_turns::ConnTurn>>,
) -> rusqlite::Result<()> {
    watch_with(db, LONG_LOCK_HOLD, move |event| match event {
        Event::Statement { holds_write_lock } => {
            if let Some(turn) = &turn {
                turn.statement_done(holds_write_lock);
            }
        }
        Event::LongHold(hold) => log(hold),
    })
}

/// What [`watch_with`] reports.
pub(crate) enum Event<'a> {
    /// A statement ended, the connection holding the write lock or not.
    Statement { holds_write_lock: bool },
    /// A hold of the write lock of the threshold or more ended.
    LongHold(&'a LongHold),
}

fn log(hold: &LongHold) {
    tracing::warn!(
        target: ALWAYS_LOGGED,
        statements = hold.statements,
        in_statements_ms = hold.in_statements.as_millis() as u64,
        first_write = %hold.first_write,
        "index write lock held {} ms by pid {} thread {}",
        hold.held.as_millis(),
        std::process::id(),
        hold.thread,
    );
}

/// [`watch`] with its threshold and what it reports to.
pub(crate) fn watch_with(
    db: &mut Connection,
    threshold: Duration,
    report: impl Fn(Event<'_>) + Send + 'static,
) -> rusqlite::Result<()> {
    // SAFETY: the handle is only read, by the trace callback below, which
    // the connection owns: SQLite calls it from inside this connection's own
    // statement calls, so the handle is open and in the calling thread's use
    // whenever it is read, and the callback is dropped with the connection
    // (or when another trace replaces it).
    let handle = Handle(unsafe { db.handle() });
    let mut hold: Option<Hold> = None;
    db.trace_v2(
        TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(move |event: TraceEvent<'_>| {
            let TraceEvent::Profile(statement, ran) = event else {
                return;
            };
            let holds_write_lock = handle.holds_write_lock();
            report(Event::Statement { holds_write_lock });
            match (hold.as_mut(), holds_write_lock) {
                (None, true) => {
                    let mut started = Hold {
                        since: Instant::now(),
                        first_write: None,
                        first: None,
                        statements: 0,
                        in_statements: Duration::ZERO,
                    };
                    started.ran(&statement.sql());
                    hold = Some(started);
                }
                (Some(held), true) => {
                    held.ran(&statement.sql());
                    held.in_statements += ran;
                }
                (Some(_), false) => {
                    let Some(mut held) = hold.take() else { return };
                    held.in_statements += ran;
                    let lasted = held.since.elapsed();
                    if lasted >= threshold {
                        report(Event::LongHold(&LongHold {
                            held: lasted,
                            thread: std::thread::current()
                                .name()
                                .unwrap_or("unnamed")
                                .to_owned(),
                            first_write: held.first_write.or(held.first).unwrap_or_default(),
                            statements: held.statements,
                            in_statements: held.in_statements.min(lasted),
                        }));
                    }
                }
                (None, false) => {}
            }
        }),
    )
}

/// A hold in progress.
struct Hold {
    since: Instant,
    first_write: Option<String>,
    first: Option<String>,
    statements: u32,
    in_statements: Duration,
}

impl Hold {
    fn ran(&mut self, sql: &str) {
        self.statements = self.statements.saturating_add(1);
        if self.first_write.is_some() {
            return;
        }
        let sql = sql.trim_start();
        let verb = sql
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or_default();
        let writes = ["INSERT", "UPDATE", "DELETE", "REPLACE"]
            .iter()
            .any(|write| verb.eq_ignore_ascii_case(write));
        if writes {
            self.first_write = Some(quote(sql));
        } else if self.first.is_none() {
            self.first = Some(quote(sql));
        }
    }
}

/// The start of `sql` on one line.
fn quote(sql: &str) -> String {
    let mut quoted = String::with_capacity(FIRST_WRITE_CHARS);
    for word in sql.split_whitespace() {
        if !quoted.is_empty() {
            quoted.push(' ');
        }
        quoted.push_str(word);
        if quoted.chars().count() >= FIRST_WRITE_CHARS {
            break;
        }
    }
    match quoted.char_indices().nth(FIRST_WRITE_CHARS) {
        Some((end, _)) => format!("{}...", &quoted[..end]),
        None => quoted,
    }
}

/// A connection's handle, read by its own trace callback.
struct Handle(*mut rusqlite::ffi::sqlite3);

// SAFETY: see `watch_with`: the callback that holds it runs only inside the
// owning connection's statement calls, on whichever thread makes them, and
// never outlives the connection.
unsafe impl Send for Handle {}

impl Handle {
    fn holds_write_lock(&self) -> bool {
        // SAFETY: the connection is open (see `watch_with`), and a null
        // schema name asks about every attached database; SQLite takes the
        // connection's own (recursive) mutex.
        let state = unsafe { rusqlite::ffi::sqlite3_txn_state(self.0, std::ptr::null()) };
        state == rusqlite::ffi::SQLITE_TXN_WRITE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn watched(
        path: &std::path::Path,
        threshold: Duration,
    ) -> (Connection, Arc<Mutex<Vec<LongHold>>>) {
        let mut db = Connection::open(path).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER)")
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        watch_with(&mut db, threshold, move |event| {
            if let Event::LongHold(hold) = event {
                sink.lock().unwrap().push(hold.clone());
            }
        })
        .unwrap();
        (db, seen)
    }

    /// Who held the write lock past the threshold is reported once, with
    /// the transaction's first write; a short hold, a deferred transaction's
    /// reads before its first write and a refused `BEGIN IMMEDIATE` are not
    /// holds.
    /// Far above a short transaction's time on a loaded machine.
    const THRESHOLD: Duration = Duration::from_millis(400);
    const LONG: Duration = Duration::from_millis(600);

    #[test]
    fn a_long_hold_is_reported_once_with_its_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let (db, seen) = watched(&path, THRESHOLD);

        db.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (1); COMMIT")
            .unwrap();
        assert!(seen.lock().unwrap().is_empty(), "a short hold");

        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        db.execute("UPDATE t SET x = 2", []).unwrap();
        db.execute("INSERT INTO t VALUES (3)", []).unwrap();
        // A peer that asks meanwhile is refused and holds nothing.
        let (peer, peer_seen) = watched(&path, THRESHOLD);
        peer.busy_timeout(Duration::ZERO).unwrap();
        assert!(peer.execute_batch("BEGIN IMMEDIATE").is_err());
        std::thread::sleep(LONG);
        db.execute_batch("COMMIT").unwrap();
        let _ = peer.execute_batch("ROLLBACK");
        assert!(peer_seen.lock().unwrap().is_empty(), "the refused peer");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "{seen:?}");
            assert!(seen[0].held >= LONG, "{:?}", seen[0]);
            assert_eq!(seen[0].first_write, "UPDATE t SET x = 2");
            assert_eq!(seen[0].statements, 3, "BEGIN, UPDATE, INSERT");
            // The sleep ran between statements, outside SQLite.
            assert!(
                seen[0].held - seen[0].in_statements >= LONG,
                "{:?}",
                seen[0]
            );
            assert_eq!(
                seen[0].thread,
                std::thread::current().name().unwrap_or("unnamed")
            );
        }

        // A deferred transaction holds the lock from its first write.
        db.execute_batch("BEGIN").unwrap();
        db.query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0))
            .unwrap();
        std::thread::sleep(LONG);
        db.execute("DELETE FROM t WHERE x = 1", []).unwrap();
        db.execute_batch("COMMIT").unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "the reads held no lock");

        // Time a statement takes under the lock counts as inside SQLite.
        db.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (4)").unwrap();
        let started = Instant::now();
        db.query_row(
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 1000000)
             SELECT count(*) FROM c",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
        let query = started.elapsed();
        std::thread::sleep(LONG);
        db.execute_batch("COMMIT").unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[1].in_statements * 2 >= query,
            "the query's {query:?} under the lock is counted: {:?}",
            seen[1]
        );
    }

    /// Collects each event's target and message.
    struct Lines(Arc<Mutex<Vec<(String, String)>>>);

    impl tracing::Subscriber for Lines {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Message(String);
            impl tracing::field::Visit for Message {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0
                .lock()
                .unwrap()
                .push((event.metadata().target().to_owned(), message.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A writable index connection logs its long holds where a wrapper
    /// without file logging still writes them.
    #[test]
    fn a_writable_index_connection_logs_its_long_holds() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::open_index_db(&dir.path().join("index.db")).unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::with_default(Lines(Arc::clone(&lines)), || {
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            std::thread::sleep(LONG_LOCK_HOLD + Duration::from_millis(50));
            db.execute_batch("COMMIT").unwrap();
        });
        let lines = lines.lock().unwrap();
        assert!(
            lines.iter().any(|(target, message)| target == ALWAYS_LOGGED
                && message.starts_with("index write lock held ")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_first_write_is_quoted_on_one_short_line() {
        assert_eq!(
            quote("  INSERT INTO t\n   (a, b)\tVALUES (?1, ?2)"),
            "INSERT INTO t (a, b) VALUES (?1, ?2)"
        );
        let long = format!("UPDATE entries SET {}", "a = 1, ".repeat(40));
        let quoted = quote(&long);
        assert!(quoted.ends_with("..."));
        assert_eq!(quoted.chars().count(), FIRST_WRITE_CHARS + 3);
    }
}
