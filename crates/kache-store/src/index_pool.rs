//! Connections to a store's index that the stores of one process share.
//!
//! The daemon runs each unit Cargo hands it on a thread of its own, and each
//! unit opens the store. A unit spends most of its life waiting: for a peer's
//! discovery, a scheduler permit, a peer's commit, its compiler. A connection
//! held through those waits keeps its page cache, its parsed schema and two
//! file descriptors: with 400 to 800 units in flight on a 192-thread machine,
//! as many connections, 156-193 MB of page cache alone.
//!
//! With pooling on ([`pool_index_connections`]: the daemon turns it on, other
//! processes keep a connection per store), a store takes a connection for each
//! statement or transaction and gives it back as soon as nothing uses it: no
//! statement borrows it ([`IndexGuard`]), no transaction is open on it, and
//! its busy timeout is the store's own (a call that waits with another keeps
//! it until it gives the timeout back). The connections in use are then the
//! statements running, not the units in flight. Taking one never waits: when
//! none is idle a new one is opened, so the pool adds no lock wait to
//! SQLite's own, and a statement waiting for a lock can never starve the
//! statement that holds it. Only the idle connections are capped; one given
//! back past the cap is closed.
//!
//! A connection goes out again only while the file it opened is still the
//! index at its path: a quarantined or replaced index never gets a
//! connection to the old file.
//!
//! An idle connection keeps the page cache its statements filled (up to
//! SQLite's 2 MB each), so the idle set stays small, and the daemon closes
//! it when the builds stop ([`close_idle_index_connections`]).

use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

/// Idle connections kept per index; 0 while pooling is off.
static IDLE_CAP: AtomicUsize = AtomicUsize::new(0);

/// Turn pooling on for this process, keeping at most `idle_per_index` idle
/// connections to each index (at least one). Call it once at start, before
/// any store opens: a store opened before keeps a connection of its own.
pub fn pool_index_connections(idle_per_index: usize) {
    IDLE_CAP.store(idle_per_index.max(1), Ordering::Relaxed);
}

fn idle_cap() -> usize {
    #[cfg(test)]
    if let Some(cap) = TEST_IDLE_CAP.get() {
        return cap;
    }
    IDLE_CAP.load(Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    /// Pooling for one test's thread, which [`pool_index_connections`] would
    /// turn on for every test in the process.
    static TEST_IDLE_CAP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Pool with `idle_per_index` idle connections (0: off) on this thread.
#[cfg(test)]
pub(crate) fn pool_on_this_thread(idle_per_index: usize) {
    TEST_IDLE_CAP.set(Some(idle_per_index));
}

/// Which connections may stand in for each other: the same index file,
/// opened the same way.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    pub(crate) path: PathBuf,
    pub(crate) read_only: bool,
    /// The `synchronous` pragma a read-write connection was opened with.
    pub(crate) synchronous: &'static str,
}

/// The idle connections of one [`PoolKey`].
#[derive(Default)]
struct Idle {
    conns: Vec<Pooled>,
}

/// A [`PoolKey`] and its idle connections.
#[derive(Clone)]
pub(crate) struct Pool {
    pub(crate) key: PoolKey,
    idle: Arc<Mutex<Idle>>,
}

impl Pool {
    /// The pool for `path`, or `None` while pooling is off.
    pub(crate) fn of(path: &Path, read_only: bool, synchronous: &'static str) -> Option<Self> {
        if idle_cap() == 0 {
            return None;
        }
        let key = PoolKey {
            path: path.to_path_buf(),
            read_only,
            synchronous,
        };
        let mut pools = lock_counted(pools())?;
        let idle = Arc::clone(pools.entry(key.clone()).or_default());
        Some(Self { key, idle })
    }

    /// An idle connection whose file is still the index at its path, or
    /// `None`: the caller opens one. Never waits for one to come back.
    pub(crate) fn take(&self) -> Option<Pooled> {
        let mut stale = Vec::new();
        let found = {
            let current = kache_fs::file_identity(&self.key.path).ok()?;
            let mut idle = lock_counted(&self.idle)?;
            let mut found = None;
            while let Some(pooled) = idle.conns.pop() {
                if pooled.file == Some(current) {
                    found = Some(pooled);
                    break;
                }
                stale.push(pooled);
            }
            found
        };
        // Closed outside the lock: a close may checkpoint.
        STATS.stale.fetch_add(stale.len() as u64, Ordering::Relaxed);
        drop(stale);
        let pooled = found?;
        STATS.reused.fetch_add(1, Ordering::Relaxed);
        in_use_add();
        Some(pooled)
    }

    /// Take back a connection a store is done with. One inside a
    /// transaction, without a file, or past the idle cap is closed.
    pub(crate) fn give_back(&self, pooled: Pooled) {
        in_use_sub();
        STATS.given_back.fetch_add(1, Ordering::Relaxed);
        if !pooled.db.is_autocommit() || pooled.file.is_none() {
            STATS.stale.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let rejected = match lock_counted(&self.idle) {
            Some(mut idle) if idle.conns.len() < idle_cap() => {
                idle.conns.push(pooled);
                None
            }
            _ => Some(pooled),
        };
        if rejected.is_some() {
            STATS.closed_over_cap.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Close every idle connection: the index was quarantined and recreated.
    pub(crate) fn forget(&self) {
        let gone: Vec<Pooled> = match self.idle.lock() {
            Ok(mut idle) => std::mem::take(&mut idle.conns),
            Err(_) => Vec::new(),
        };
        STATS.stale.fetch_add(gone.len() as u64, Ordering::Relaxed);
    }
}

fn pools() -> &'static Mutex<HashMap<PoolKey, Arc<Mutex<Idle>>>> {
    static POOLS: OnceLock<Mutex<HashMap<PoolKey, Arc<Mutex<Idle>>>>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock one of the pool's mutexes, counting the times a thread had to wait
/// for it and how long ([`IndexPoolStats::lock_waits`]).
fn lock_counted<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => return Some(guard),
        Err(TryLockError::Poisoned(_)) => return None,
        Err(TryLockError::WouldBlock) => {}
    }
    let started = Instant::now();
    let guard = mutex.lock().ok();
    let waited = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    STATS.lock_waits.fetch_add(1, Ordering::Relaxed);
    STATS.lock_wait_us.fetch_add(waited, Ordering::Relaxed);
    STATS.lock_wait_us_max.fetch_max(waited, Ordering::Relaxed);
    guard
}

/// Close every idle connection, to every index: the daemon does once the
/// builds have stopped, so that their page caches can go back with the
/// free heap. Returns how many it closed.
pub fn close_idle_index_connections() -> usize {
    close_idle_where(|_| true)
}

/// Close the idle connections of the pools `close` names.
pub(crate) fn close_idle_where(close: impl Fn(&PoolKey) -> bool) -> usize {
    let idle: Vec<Arc<Mutex<Idle>>> = match lock_counted(pools()) {
        Some(pools) => pools
            .iter()
            .filter(|(key, _)| close(key))
            .map(|(_, idle)| Arc::clone(idle))
            .collect(),
        None => return 0,
    };
    let mut closed = 0;
    for idle in idle {
        // Closed outside the lock: a close may checkpoint.
        let gone = lock_counted(&idle).map(|mut idle| std::mem::take(&mut idle.conns));
        closed += gone.map_or(0, |gone| gone.len());
    }
    STATS
        .closed_idle
        .fetch_add(closed as u64, Ordering::Relaxed);
    closed
}

/// A connection and the identity of the index file it opened.
pub(crate) struct Pooled {
    pub(crate) db: Connection,
    file: Option<kache_fs::InodeId>,
}

impl Pooled {
    /// A connection just opened to `path`. Its identity is read now: the open
    /// and this read see the same file unless it is replaced in between, and
    /// such a connection is then never handed out (the identities differ).
    pub(crate) fn opened(db: Connection, path: &Path, pooled: bool) -> Self {
        if pooled {
            STATS.opened.fetch_add(1, Ordering::Relaxed);
            in_use_add();
        }
        Self {
            db,
            file: kache_fs::file_identity(path).ok(),
        }
    }

    /// An empty in-memory database standing in for an index that could not
    /// be opened again: every statement fails on it. With no file it is
    /// never given back to the pool.
    pub(crate) fn unusable() -> Self {
        Self {
            db: Connection::open_in_memory().expect("an in-memory database needs no file"),
            file: None,
        }
    }
}

struct Stats {
    opened: AtomicU64,
    reused: AtomicU64,
    given_back: AtomicU64,
    closed_over_cap: AtomicU64,
    stale: AtomicU64,
    closed_idle: AtomicU64,
    lock_waits: AtomicU64,
    lock_wait_us: AtomicU64,
    lock_wait_us_max: AtomicU64,
    in_use: AtomicUsize,
    in_use_max: AtomicUsize,
}

static STATS: Stats = Stats {
    opened: AtomicU64::new(0),
    reused: AtomicU64::new(0),
    given_back: AtomicU64::new(0),
    closed_over_cap: AtomicU64::new(0),
    stale: AtomicU64::new(0),
    closed_idle: AtomicU64::new(0),
    lock_waits: AtomicU64::new(0),
    lock_wait_us: AtomicU64::new(0),
    lock_wait_us_max: AtomicU64::new(0),
    in_use: AtomicUsize::new(0),
    in_use_max: AtomicUsize::new(0),
};

fn in_use_add() {
    let now = STATS.in_use.fetch_add(1, Ordering::Relaxed) + 1;
    STATS.in_use_max.fetch_max(now, Ordering::Relaxed);
}

fn in_use_sub() {
    let _ = STATS
        .in_use
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
}

/// What the pool has done since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexPoolStats {
    /// Connections opened because none was idle.
    pub opened: u64,
    /// Times a store took an idle connection instead of opening one.
    pub reused: u64,
    /// Times a store gave a connection back.
    pub given_back: u64,
    /// Connections closed because the idle cap was reached.
    pub closed_over_cap: u64,
    /// Idle connections closed because their index file was replaced.
    pub stale: u64,
    /// Idle connections closed because the builds stopped.
    pub closed_idle: u64,
    /// Times a thread waited for one of the pool's locks, how long in all
    /// and the longest wait, in microseconds.
    pub lock_waits: u64,
    pub lock_wait_us: u64,
    pub lock_wait_us_max: u64,
    /// Connections stores hold now, and the most they held at once.
    pub in_use: usize,
    pub in_use_max: usize,
    /// Idle connections now, over every index.
    pub idle: usize,
}

/// The pool's counters, for the daemon's log.
pub fn index_pool_stats() -> IndexPoolStats {
    let idle = pools()
        .lock()
        .map(|pools| {
            pools
                .values()
                .map(|idle| idle.lock().map_or(0, |idle| idle.conns.len()))
                .sum()
        })
        .unwrap_or(0);
    IndexPoolStats {
        opened: STATS.opened.load(Ordering::Relaxed),
        reused: STATS.reused.load(Ordering::Relaxed),
        given_back: STATS.given_back.load(Ordering::Relaxed),
        closed_over_cap: STATS.closed_over_cap.load(Ordering::Relaxed),
        stale: STATS.stale.load(Ordering::Relaxed),
        closed_idle: STATS.closed_idle.load(Ordering::Relaxed),
        lock_waits: STATS.lock_waits.load(Ordering::Relaxed),
        lock_wait_us: STATS.lock_wait_us.load(Ordering::Relaxed),
        lock_wait_us_max: STATS.lock_wait_us_max.load(Ordering::Relaxed),
        in_use: STATS.in_use.load(Ordering::Relaxed),
        in_use_max: STATS.in_use_max.load(Ordering::Relaxed),
        idle,
    }
}

/// Whether the pool has been used at all (nothing to report otherwise).
pub fn index_pool_used() -> bool {
    STATS.opened.load(Ordering::Relaxed) > 0
}

/// The connection a store uses: its own, or while pooling is on one it takes
/// for each statement or transaction ([`IndexGuard`]) and gives back as soon
/// as nothing uses it.
pub(crate) struct IndexConn {
    slot: std::cell::RefCell<Option<Pooled>>,
    pool: Option<Pool>,
}

impl IndexConn {
    pub(crate) fn new(pooled: Pooled, pool: Option<Pool>) -> Self {
        let conn = Self {
            slot: std::cell::RefCell::new(Some(pooled)),
            pool,
        };
        conn.release_if_idle(None);
        conn
    }

    /// The connection, taken (from the pool, else `open`) when the store
    /// holds none, with the store's busy timeout `busy_wait`.
    pub(crate) fn get(
        &self,
        busy_wait: Duration,
        open: impl FnOnce(&PoolKey) -> Pooled,
    ) -> IndexGuard<'_> {
        if self.slot.borrow().is_none() {
            // Nothing borrows an empty slot: every borrow is of a connection.
            let pool = self
                .pool
                .as_ref()
                .expect("only a pooled store ever gives its connection back");
            let pooled = pool.take().unwrap_or_else(|| open(&pool.key));
            let _ = pooled.db.busy_timeout(busy_wait);
            *self.slot.borrow_mut() = Some(pooled);
        }
        IndexGuard {
            conn: self,
            db: Some(std::cell::Ref::map(self.slot.borrow(), |slot| {
                &slot.as_ref().expect("the slot was filled above").db
            })),
            busy_wait,
        }
    }

    /// Whether the store holds a connection now.
    #[cfg(test)]
    pub(crate) fn holds(&self) -> bool {
        self.slot.borrow().is_some()
    }

    /// Give the connection back when nothing uses it: no borrow, no open
    /// transaction, and (with `busy_wait`) the store's own busy timeout.
    fn release_if_idle(&self, busy_wait: Option<Duration>) {
        let Some(pool) = &self.pool else {
            return;
        };
        let Ok(mut slot) = self.slot.try_borrow_mut() else {
            return;
        };
        let idle = slot.as_ref().is_some_and(|pooled| {
            pooled.db.is_autocommit()
                && busy_wait
                    .is_none_or(|wait| busy_timeout_ms(&pooled.db) == Some(wait.as_millis()))
        });
        if idle && let Some(pooled) = slot.take() {
            drop(slot);
            pool.give_back(pooled);
        }
    }
}

/// The connection's busy timeout in milliseconds.
fn busy_timeout_ms(db: &Connection) -> Option<u128> {
    db.prepare_cached("PRAGMA busy_timeout")
        .and_then(|mut statement| statement.query_row([], |row| row.get::<_, i64>(0)))
        .ok()
        .and_then(|ms| u128::try_from(ms).ok())
}

impl Drop for IndexConn {
    fn drop(&mut self) {
        if let (Some(pool), Some(pooled)) = (&self.pool, self.slot.get_mut().take()) {
            pool.give_back(pooled);
        }
    }
}

/// A store's index connection, lent for one statement or transaction. When
/// the last guard of a pooled store goes, the connection goes back to the
/// pool, unless a transaction is open on it or a call changed its busy
/// timeout and has not given it back yet.
pub struct IndexGuard<'a> {
    conn: &'a IndexConn,
    db: Option<std::cell::Ref<'a, Connection>>,
    busy_wait: Duration,
}

impl std::ops::Deref for IndexGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.db
            .as_ref()
            .expect("a guard holds its connection until dropped")
    }
}

impl Drop for IndexGuard<'_> {
    fn drop(&mut self) {
        drop(self.db.take());
        self.conn.release_if_idle(Some(self.busy_wait));
    }
}
