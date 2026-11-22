//! Turns at the index's write lock, in the order writers asked.
//!
//! The index is one SQLite file in WAL mode: one writer at a time. SQLite's
//! own busy handler does not queue: a writer that finds the lock held sleeps
//! and asks again, 1, 2, 5, 10 ... ms at first and 100 ms apart once it has
//! waited a quarter second. Whoever asks right as the lock frees gets it, so
//! a writer that has waited 10 s loses every free moment to writers that
//! just arrived (and to the daemon's batch, whose 20 ms waits always start
//! over at 1 ms). After a long hold (a sweep, a commit on a paged-out index)
//! the pile of waiters is served in no order at all, and the oldest give up
//! after their whole wait while younger ones get through.
//!
//! Here every writable index connection ([`install`]) gets a busy handler
//! that takes a ticket the first time a statement finds the lock held, and
//! waits until its ticket is served before it asks SQLite for the lock
//! again; only the writer whose turn it is polls the lock (every
//! `HEAD_POLL` at first, less often while a long hold keeps it waiting).
//! The turn passes on when that writer gets the lock (the
//! statement's end, seen by [`ConnTurn::statement_done`]), gives up, or
//! stops showing signs of life (`STALE_HEAD`, sooner when its process is
//! gone).
//!
//! The tickets live in a small file beside the index (`index.db-turns`),
//! mapped by every process that writes the index. It only orders the retries:
//! SQLite's lock still decides who writes, so a writer that ignores the
//! turns (an older kache, the `sqlite3` shell, a statement that got the lock
//! at its first try) can still take it out of turn, and a broken or unknown
//! file only turns the queue off (SQLite's own wait then, as before).
//! `KACHE_INDEX_TURNS=0` turns it off too.
//!
//! The waits each caller sets (the daemon's 20 ms tries, a hit stamp's
//! 100 ms, a registration's 5 s) are kept by [`set_index_busy_wait`] instead
//! of SQLite's busy timeout, which would replace the handler. A caller that
//! asks for the lock in several short waits keeps its place between them
//! while it holds an [`IndexPlace`].

pub use imp::{ConnTurn, IndexPlace, busy_wait, hold_index_place, install, set_index_busy_wait};

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::{c_int, c_void};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use rusqlite::{Connection, ffi};

    /// How often the writer whose turn it is asks SQLite for the lock, at
    /// first; after a while as head it asks every sixteenth of the time it
    /// has waited there, up to [`MAX_HEAD_POLL`] (a long hold, a sweep or a
    /// compaction, is not polled a thousand times a second, and the lock
    /// stays free at most a few ms after it).
    pub(crate) const HEAD_POLL: Duration = Duration::from_millis(1);
    const MAX_HEAD_POLL: Duration = Duration::from_millis(8);

    /// How long the writer whose turn it is may show no sign of life (it
    /// asks every [`MAX_HEAD_POLL`] at most) before the next one is served
    /// instead, its process still there: far above a poll, since a thread on
    /// a loaded machine (an index storm comes with load 150-480 on the build
    /// machine) can sleep tens of ms past its time. A writer passed over this
    /// way may still ask for the lock beside the new head; it only lost its
    /// turn of its own. A head whose process is gone is passed over after
    /// [`SILENT_HEAD`].
    pub(crate) const STALE_HEAD: Duration = Duration::from_millis(300);

    /// How long a waiter sleeps between looks at the turns for each writer
    /// ahead of it (every turn takes at least a statement), and at most: a
    /// waiter far back looks less often than the next in line.
    const WAIT_STEP: Duration = Duration::from_micros(500);
    const MAX_WAIT_STEP: Duration = Duration::from_millis(50);

    /// How long the writer whose turn it is may be silent before the next in
    /// line asks whether its process is still there.
    const SILENT_HEAD: Duration = Duration::from_millis(20);

    /// The size of the turns file.
    const FILE_BYTES: usize = 16 * 1024;

    /// The file's layout (64-bit words). Words 6..64 are spare.
    const MAGIC: u64 = 0x6b74_7572_6e73_0001; // "kturns", version 1
    const W_MAGIC: usize = 0;
    const W_NEXT: usize = 1;
    const W_SERVING: usize = 2;
    const W_SERVING_SINCE: usize = 3;
    const W_HEAD_TICKET: usize = 4;
    const W_HEAD_BEAT: usize = 5;
    /// One word per ticket (modulo [`RING`]): `ticket << 1`, with the low bit
    /// set once that ticket is done (it got the lock or gave up).
    const W_SLOTS: usize = 64;
    /// The process holding each slot's ticket.
    const W_PIDS: usize = W_SLOTS + RING;
    /// Tickets the file tells apart. More waiters at once than this only
    /// lose the instant skip of a ticket that is done (the stale-head rule
    /// still passes it over).
    const RING: usize = 976;

    const NO_TICKET: u64 = u64::MAX;

    /// The turns of one index, shared through its mapped file.
    pub(crate) struct Queue {
        words: *const AtomicU64,
    }

    // SAFETY: `words` points into a MAP_SHARED mapping that is never
    // unmapped (the queue lives as long as the process, in `Queue::of`'s
    // map), and it is only ever accessed through atomics, which other
    // threads and processes may use at the same time.
    unsafe impl Send for Queue {}
    // SAFETY: as for `Send`: every access is an atomic operation.
    unsafe impl Sync for Queue {}

    fn turns_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            !std::env::var("KACHE_INDEX_TURNS")
                .is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no"))
        })
    }

    /// The monotonic clock in nanoseconds, the same in every process.
    fn now_ns() -> u64 {
        // SAFETY: clock_gettime writes the zeroed timespec we pass and keeps
        // no pointer to it.
        let ts = unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
            ts
        };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    fn process_gone(pid: u64) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 sends nothing; it only asks whether the process
        // exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    impl Queue {
        /// The turns of the index at `db_path`, mapped once per process.
        pub(crate) fn of(db_path: &Path) -> Option<Arc<Queue>> {
            type Queues = Mutex<HashMap<PathBuf, Option<Arc<Queue>>>>;
            static QUEUES: OnceLock<Queues> = OnceLock::new();
            let mut queues = QUEUES.get_or_init(Default::default).lock().ok()?;
            let mut name = db_path.as_os_str().to_owned();
            name.push("-turns");
            let path = PathBuf::from(name);
            if let Some(queue) = queues.get(&path) {
                return queue.clone();
            }
            let queue = match Queue::map(&path) {
                Ok(queue) => Some(Arc::new(queue)),
                Err(error) => {
                    tracing::debug!(path = %path.display(), "index turns off: {error}");
                    None
                }
            };
            queues.insert(path, queue.clone());
            queue
        }

        fn map(path: &Path) -> std::io::Result<Queue> {
            use std::os::fd::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o666)
                .custom_flags(libc::O_CLOEXEC)
                .open(path)?;
            if file.metadata()?.len() < FILE_BYTES as u64 {
                // Growing only: every opener grows it to the same size before
                // it maps it, so no mapping reaches past the file's end.
                file.set_len(FILE_BYTES as u64)?;
            }
            if let Some(gid) = path.parent().and_then(kache_fs::group_store_gid) {
                kache_fs::share_with_group(path, gid);
            }
            // SAFETY: a new shared mapping of FILE_BYTES of a file at least
            // that long, at an address the kernel picks; the mapping keeps
            // the file, so it may close after.
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    FILE_BYTES,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            if base == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            let queue = Queue {
                words: base.cast::<AtomicU64>(),
            };
            match queue
                .word(W_MAGIC)
                .compare_exchange(0, MAGIC, SeqCst, SeqCst)
            {
                Ok(_) | Err(MAGIC) => Ok(queue),
                Err(other) => {
                    // SAFETY: the mapping made above, which nothing else
                    // refers to yet.
                    unsafe { libc::munmap(base, FILE_BYTES) };
                    Err(std::io::Error::other(format!(
                        "unknown turns file (magic {other:#x})"
                    )))
                }
            }
        }

        fn word(&self, at: usize) -> &AtomicU64 {
            debug_assert!(at < FILE_BYTES / 8);
            // SAFETY: `at` is inside the mapping (FILE_BYTES / 8 words), which
            // is page-aligned and lives as long as the process; AtomicU64 has
            // u64's size and alignment.
            unsafe { &*self.words.add(at) }
        }

        /// A new ticket at the end of the line.
        fn take(&self) -> u64 {
            let ticket = self.word(W_NEXT).fetch_add(1, SeqCst);
            let slot = ticket as usize % RING;
            self.word(W_PIDS + slot)
                .store(u64::from(std::process::id()), SeqCst);
            self.word(W_SLOTS + slot).store(ticket << 1, SeqCst);
            if self.serving() == ticket {
                // First in line: alive from the start, not passed over for a
                // beat it has had no time to give.
                self.beat(ticket);
            }
            ticket
        }

        /// `ticket` got the lock or gave up: the line moves past it.
        fn done(&self, ticket: u64) {
            self.word(W_SLOTS + ticket as usize % RING)
                .store(ticket << 1 | 1, SeqCst);
            self.serving();
        }

        /// The ticket served now, past every ticket already done.
        pub(crate) fn serving(&self) -> u64 {
            loop {
                let serving = self.word(W_SERVING).load(SeqCst);
                if serving >= self.word(W_NEXT).load(SeqCst) {
                    return serving;
                }
                let slot = self.word(W_SLOTS + serving as usize % RING).load(SeqCst);
                if slot != serving << 1 | 1 || !self.pass(serving) {
                    return self.word(W_SERVING).load(SeqCst);
                }
            }
        }

        /// Serve the ticket after `serving`, when no one did meanwhile.
        fn pass(&self, serving: u64) -> bool {
            // Stamped before the move, so whoever sees the new ticket sees a
            // time at least this recent.
            self.word(W_SERVING_SINCE).store(now_ns(), SeqCst);
            self.word(W_SERVING)
                .compare_exchange(serving, serving + 1, SeqCst, SeqCst)
                .is_ok()
        }

        /// The writer holding `ticket`, served now, is alive and asking.
        fn beat(&self, ticket: u64) {
            self.word(W_HEAD_BEAT).store(now_ns(), SeqCst);
            self.word(W_HEAD_TICKET).store(ticket, SeqCst);
        }

        /// Pass over the served ticket when its writer is gone or silent.
        fn skip_stale(&self, serving: u64) {
            if serving >= self.word(W_NEXT).load(SeqCst) {
                return;
            }
            let since = if self.word(W_HEAD_TICKET).load(SeqCst) == serving {
                self.word(W_HEAD_BEAT).load(SeqCst)
            } else {
                self.word(W_SERVING_SINCE).load(SeqCst)
            };
            let silent = now_ns().saturating_sub(since);
            if silent <= SILENT_HEAD.as_nanos() as u64 {
                return;
            }
            let slot = serving as usize % RING;
            let its_slot = self.word(W_SLOTS + slot).load(SeqCst) >> 1 == serving;
            let gone = its_slot && process_gone(self.word(W_PIDS + slot).load(SeqCst));
            if gone || silent > STALE_HEAD.as_nanos() as u64 {
                self.pass(serving);
            }
        }

        /// The next ticket to give and the one served (tests).
        #[cfg(test)]
        pub(crate) fn counters(&self) -> (u64, u64) {
            (
                self.word(W_NEXT).load(SeqCst),
                self.word(W_SERVING).load(SeqCst),
            )
        }
    }

    /// A connection's place in line and its wait, shared by its busy handler,
    /// its trace callback and [`set_index_busy_wait`].
    pub struct ConnTurn {
        queue: Arc<Queue>,
        wait_ms: AtomicU64,
        ticket: AtomicU64,
        started_ns: AtomicU64,
        /// When the connection's ticket was first served (0: not yet).
        head_since_ns: AtomicU64,
        /// [`IndexPlace`]s alive on the connection.
        places: AtomicU32,
        /// The last wait gave up, and the ticket is kept for the next try.
        kept: AtomicBool,
    }

    impl ConnTurn {
        /// The busy handler: wait for the connection's turn, then ask SQLite
        /// for the lock again; give up when the wait is over.
        fn on_busy(&self, count: c_int) -> bool {
            let wait = Duration::from_millis(self.wait_ms.load(SeqCst));
            if wait.is_zero() {
                self.leave();
                return false;
            }
            if count == 0 {
                self.started_ns.store(now_ns(), SeqCst);
                let held = self.ticket.load(SeqCst);
                if held == NO_TICKET || !self.kept.swap(false, SeqCst) {
                    if held != NO_TICKET {
                        self.queue.done(held);
                    }
                    self.head_since_ns.store(0, SeqCst);
                    self.ticket.store(self.queue.take(), SeqCst);
                }
            }
            let ticket = self.ticket.load(SeqCst);
            let deadline = self.started_ns.load(SeqCst) + wait.as_nanos() as u64;
            loop {
                let now = now_ns();
                if now >= deadline {
                    if self.places.load(SeqCst) > 0 {
                        self.kept.store(true, SeqCst);
                    } else {
                        self.leave();
                    }
                    return false;
                }
                let left = Duration::from_nanos(deadline - now);
                let serving = self.queue.serving();
                if serving >= ticket {
                    if serving == ticket {
                        self.queue.beat(ticket);
                    }
                    let since = match self.head_since_ns.load(SeqCst) {
                        0 => {
                            self.head_since_ns.store(now, SeqCst);
                            now
                        }
                        since => since,
                    };
                    let poll =
                        (Duration::from_nanos(now - since) / 16).clamp(HEAD_POLL, MAX_HEAD_POLL);
                    std::thread::sleep(poll.min(left));
                    return true;
                }
                let ahead = (ticket - serving).min(1_000) as u32;
                if ahead == 1 {
                    // The next in line watches the head.
                    self.queue.skip_stale(serving);
                }
                let step = (WAIT_STEP * ahead).min(MAX_WAIT_STEP);
                std::thread::sleep(step.min(left));
            }
        }

        /// A statement on the connection ended: when it waited in line, its
        /// turn is over (it got the lock, or it failed), unless it gave up
        /// keeping its place for the next try.
        pub(crate) fn statement_done(&self, holds_write_lock: bool) {
            if self.ticket.load(SeqCst) == NO_TICKET {
                return;
            }
            if !holds_write_lock && self.kept.load(SeqCst) {
                return;
            }
            self.leave();
        }

        fn leave(&self) {
            self.kept.store(false, SeqCst);
            let ticket = self.ticket.swap(NO_TICKET, SeqCst);
            if ticket != NO_TICKET {
                self.queue.done(ticket);
            }
        }
    }

    const CLIENT_DATA: &std::ffi::CStr = c"kache_index_turn";

    unsafe extern "C" fn busy_callback(arg: *mut c_void, count: c_int) -> c_int {
        // SAFETY: `arg` is the ConnTurn the connection's client data keeps
        // alive until the connection closes, after which SQLite calls no
        // busy handler; SQLite calls it on the thread running the statement.
        let turn = unsafe { &*arg.cast::<ConnTurn>() };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| turn.on_busy(count)))
            .map_or(0, c_int::from)
    }

    unsafe extern "C" fn drop_turn(data: *mut c_void) {
        // SAFETY: `data` is the pointer `install` made with Arc::into_raw;
        // SQLite calls this once, when the connection closes or the key is
        // set again.
        drop(unsafe { Arc::from_raw(data.cast::<ConnTurn>()) });
    }

    fn turn_of(db: &Connection) -> Option<&ConnTurn> {
        // SAFETY: the handle is open while `db` is borrowed; the client data
        // under this key is only ever a ConnTurn `install` stored, which only
        // the connection's close frees, so the reference lives no longer
        // than the borrow of `db`.
        unsafe {
            let data = ffi::sqlite3_get_clientdata(db.handle(), CLIENT_DATA.as_ptr());
            data.cast::<ConnTurn>().as_ref()
        }
    }

    fn attach_handler(db: &Connection, turn: &ConnTurn) -> rusqlite::Result<()> {
        // SAFETY: the handle is open while `db` is borrowed; the handler's
        // argument is the ConnTurn the connection's client data keeps alive
        // for as long as the handler can run.
        let rc = unsafe {
            ffi::sqlite3_busy_handler(
                db.handle(),
                Some(busy_callback),
                std::ptr::from_ref(turn).cast_mut().cast(),
            )
        };
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None))
        }
    }

    /// Give the writable index connection `db` (of the index at `db_path`)
    /// turns at the write lock, waiting `wait` for its turn. None when the
    /// turns are off: the connection keeps SQLite's busy timeout.
    pub fn install(db: &Connection, db_path: &Path, wait: Duration) -> Option<Arc<ConnTurn>> {
        if !turns_enabled() {
            return None;
        }
        let queue = Queue::of(db_path)?;
        let turn = Arc::new(ConnTurn {
            queue,
            wait_ms: AtomicU64::new(wait.as_millis() as u64),
            ticket: AtomicU64::new(NO_TICKET),
            started_ns: AtomicU64::new(0),
            head_since_ns: AtomicU64::new(0),
            places: AtomicU32::new(0),
            kept: AtomicBool::new(false),
        });
        let data = Arc::into_raw(Arc::clone(&turn)).cast_mut().cast::<c_void>();
        // SAFETY: the handle is open while `db` is borrowed; SQLite owns
        // `data` from here and frees it with `drop_turn` when the connection
        // closes (or the key is set again).
        let rc = unsafe {
            ffi::sqlite3_set_clientdata(db.handle(), CLIENT_DATA.as_ptr(), data, Some(drop_turn))
        };
        if rc != ffi::SQLITE_OK || attach_handler(db, &turn).is_err() {
            return None;
        }
        Some(turn)
    }

    /// How long `db` waits for a lock held elsewhere: in line when it has
    /// turns ([`install`]), else SQLite's busy timeout.
    pub fn set_index_busy_wait(db: &Connection, wait: Duration) -> rusqlite::Result<()> {
        match turn_of(db) {
            Some(turn) => {
                turn.wait_ms.store(wait.as_millis() as u64, SeqCst);
                if wait.is_zero() {
                    turn.leave();
                }
                // Again, in case a busy timeout replaced it meanwhile.
                attach_handler(db, turn)
            }
            None => db.busy_timeout(wait),
        }
    }

    /// The wait [`set_index_busy_wait`] last set on `db` (SQLite's busy
    /// timeout on a connection without turns).
    pub fn busy_wait(db: &Connection) -> Option<Duration> {
        let sqlite = super::sqlite_busy_timeout(db);
        match turn_of(db) {
            // SQLite's busy timeout reads 0 while the turns' handler is on;
            // anything else is a busy timeout set over it since.
            Some(turn) if sqlite.is_none_or(|wait| wait.is_zero()) => {
                Some(Duration::from_millis(turn.wait_ms.load(SeqCst)))
            }
            _ => sqlite,
        }
    }

    /// While it lives, a wait on its connection that gives up keeps the
    /// connection's place in line for the next try.
    pub struct IndexPlace {
        turn: Option<Arc<ConnTurn>>,
    }

    /// Keep `db`'s place in line between its waits until the guard drops.
    pub fn hold_index_place(db: &Connection) -> IndexPlace {
        let turn = turn_of(db).map(|turn| {
            turn.places.fetch_add(1, SeqCst);
            let raw = std::ptr::from_ref(turn);
            // SAFETY: `raw` came from Arc::into_raw in `install`, and the
            // connection's client data holds that strong count while `db` is
            // open, so the count is at least one as we add ours.
            unsafe {
                Arc::increment_strong_count(raw);
                Arc::from_raw(raw)
            }
        });
        IndexPlace { turn }
    }

    impl Drop for IndexPlace {
        fn drop(&mut self) {
            if let Some(turn) = &self.turn
                && turn.places.fetch_sub(1, SeqCst) == 1
                && turn.kept.load(SeqCst)
            {
                turn.leave();
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use rusqlite::Connection;

    /// No turns on this platform: SQLite's busy timeout.
    pub struct ConnTurn;

    impl ConnTurn {
        pub(crate) fn statement_done(&self, _holds_write_lock: bool) {}
    }

    pub fn install(_db: &Connection, _db_path: &Path, _wait: Duration) -> Option<Arc<ConnTurn>> {
        None
    }

    pub fn set_index_busy_wait(db: &Connection, wait: Duration) -> rusqlite::Result<()> {
        db.busy_timeout(wait)
    }

    pub fn busy_wait(db: &Connection) -> Option<Duration> {
        super::sqlite_busy_timeout(db)
    }

    pub struct IndexPlace;

    pub fn hold_index_place(_db: &Connection) -> IndexPlace {
        IndexPlace
    }
}

/// SQLite's own busy timeout on `db`.
fn sqlite_busy_timeout(db: &rusqlite::Connection) -> Option<std::time::Duration> {
    db.prepare_cached("PRAGMA busy_timeout")
        .and_then(|mut statement| statement.query_row([], |row| row.get::<_, i64>(0)))
        .ok()
        .and_then(|ms| u64::try_from(ms).ok())
        .map(std::time::Duration::from_millis)
}

#[cfg(all(test, unix))]
mod tests;
