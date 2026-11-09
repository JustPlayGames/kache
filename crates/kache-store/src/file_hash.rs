//! Persistent file fingerprints and opaque compiler memo records.

pub use crate::cc_memo::{CcPreprocessMemo, CcPreprocessMemoInput};
pub use crate::gone_dirs::{GONE_DIR_PRUNE_BATCH, GONE_DIR_PRUNE_BUDGET, GoneDirPrune};
pub use crate::index_compaction::{IndexCompaction, IndexPageStats, index_page_stats};
use crate::prediction_codec as codec;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MIN_PERSISTED_HASH_BYTES: i64 = 64 * 1024;

/// Windows metadata without a handle-derived ID cannot prove unchanged bytes.
/// Other platforms retain their existing file-hash memo policy.
pub fn file_hash_memo_eligible(inode: i64, requires_identity: bool) -> bool {
    !requires_identity || inode != 0
}

/// How long a file must have been left alone, when it was read, before its
/// hash may be memoised.
///
/// A memo row is trusted whenever the stamp matches. A file written, hashed and
/// then written again inside one timestamp tick keeps the same stamp with
/// different bytes, and a row recorded between the two writes would hand the
/// old hash to every later lookup. Filesystems tick coarsely: HFS+ in whole
/// seconds, FAT in two, ext4 at the kernel's coarse clock. Two seconds after
/// the last change, a further write lands on a later tick and changes the
/// stamp, so bytes read after a stat taken then belong to that stamp.
pub const HASH_SETTLE_NS: i64 = 2_000_000_000;

/// Whether `fingerprint`'s file last changed at least [`HASH_SETTLE_NS`]
/// before `observed_ns`, the wall clock read before the stat that produced
/// it. A later time, such as when a row is written, says nothing about the
/// bytes already read. The ctime counts as well as the mtime: tools that
/// restore an old mtime after writing cannot hold the ctime back.
pub fn stamp_is_settled(fingerprint: &FileFingerprint, observed_ns: i64) -> bool {
    let changed = fingerprint.mtime_ns.max(fingerprint.ctime_ns);
    observed_ns.saturating_sub(changed) >= HASH_SETTLE_NS
}

/// How far below [`stamp_clock_ns`], read just before a write, the write's
/// stamp can fall where stamps keep a fraction of a second. Linux stamps
/// files from the coarse clock that function reads there, Windows from the
/// system time it reads there, and macOS from the wall clock, so the margin
/// only covers stamps kept in microseconds. Elsewhere stamps can trail the
/// wall clock by a timer tick.
pub const FINE_STAMP_WINDOW_NS: i64 = if cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)) {
    1_000_000
} else {
    20_000_000
};

/// Added to that for a stamp on a 10 ms boundary: exFAT keeps its times in
/// 10 ms steps, and FAT its creation time, which Windows reports as ctime.
pub const CENTISECOND_STAMP_GRAIN_NS: i64 = 10_000_000;

/// Added for a stamp on a whole second: HFS+, ext3 and ext4 with 128-byte
/// inodes truncate to the second, FAT its write time to two.
pub const WHOLE_SECOND_STAMP_GRAIN_NS: i64 = 2_000_000_000;

/// Whether `fingerprint` shows a write at or after `since_ns`, a time read
/// from [`stamp_clock_ns`]. A write made after `since_ns` can carry a stamp
/// below it, so each of mtime and ctime counts from its own window earlier
/// ([`stamp_window_ns`]); FAT keeps a two-second write time beside a 10 ms
/// creation time. A stamp set by another machine's clock, over a network
/// filesystem or a VM share, is only as close as the two clocks are.
pub fn stamp_written_since(fingerprint: &FileFingerprint, since_ns: i64) -> bool {
    [fingerprint.mtime_ns, fingerprint.ctime_ns]
        .into_iter()
        .any(|stamp| stamp >= since_ns.saturating_sub(stamp_window_ns(stamp)))
}

/// The window for one stamp: [`FINE_STAMP_WINDOW_NS`], plus the grain its
/// value shows it was truncated to.
pub fn stamp_window_ns(stamp_ns: i64) -> i64 {
    let grain = if stamp_ns.rem_euclid(1_000_000_000) == 0 {
        WHOLE_SECOND_STAMP_GRAIN_NS
    } else if stamp_ns.rem_euclid(CENTISECOND_STAMP_GRAIN_NS) == 0 {
        CENTISECOND_STAMP_GRAIN_NS
    } else {
        0
    };
    grain + FINE_STAMP_WINDOW_NS
}

/// Now, on the clock file stamps are taken from: the kernel's coarse
/// realtime clock on Linux and the system time on Windows, which both trail
/// the wall clock by up to a timer tick, and the wall clock elsewhere. A
/// start that stamps are later checked against with [`stamp_written_since`]
/// is read from it.
#[cfg_attr(windows, allow(unreachable_code))]
pub fn stamp_clock_ns() -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: an all-zero timespec is valid, and the call only fills it.
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut now) } == 0 {
            let elapsed = std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
            return i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX);
        }
    }
    #[cfg(windows)]
    {
        return windows_system_time_ns();
    }
    wall_clock_ns()
}

/// The system time NTFS stamps files with. `SystemTime::now` reads the
/// precise clock, which runs up to a timer tick ahead of it, so a file
/// written just after that read could carry a stamp below it.
#[cfg(windows)]
fn windows_system_time_ns() -> i64 {
    let mut now = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: the call only fills the FILETIME it is given.
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime(&mut now) };
    filetime_to_unix_ns(now.dwHighDateTime, now.dwLowDateTime)
}

/// A Windows FILETIME, 100 ns intervals since 1601, in nanoseconds since the
/// Unix epoch, 0 before it.
#[cfg(any(windows, test))]
fn filetime_to_unix_ns(high: u32, low: u32) -> i64 {
    const UNIX_EPOCH_INTERVALS: u64 = 116_444_736_000_000_000;
    let intervals = (u64::from(high) << 32) + u64::from(low);
    intervals
        .checked_sub(UNIX_EPOCH_INTERVALS)
        .map_or(0, |since| {
            i64::try_from(u128::from(since) * 100).unwrap_or(i64::MAX)
        })
}

/// The wall clock in nanoseconds since the Unix epoch, 0 before it.
pub fn wall_clock_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// A fingerprint and the wall clock read before the file was stat'ed and
/// read, so a later check can ask whether its stamp had settled by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFingerprint {
    pub fingerprint: FileFingerprint,
    pub observed_ns: i64,
}

impl ObservedFingerprint {
    /// Read the clock, then stat `path`.
    pub fn from_path(path: &Path) -> Result<Self> {
        let observed_ns = wall_clock_ns();
        Ok(Self {
            fingerprint: FileFingerprint::from_path(path)?,
            observed_ns,
        })
    }

    /// [`stamp_is_settled`] at the time this fingerprint was observed.
    pub fn settled(&self) -> bool {
        stamp_is_settled(&self.fingerprint, self.observed_ns)
    }
}

/// Paths per lookup statement, well under SQLite's bound-parameter limit.
const FILE_HASH_LOOKUP_CHUNK: usize = 256;

/// Where a [`FileHashCache::Store`] borrows its connection from.
pub trait IndexSource {
    /// The connection, for one statement or transaction.
    fn index(&self) -> crate::index_pool::IndexGuard<'_>;

    /// Hand the daily use stamp of the input prediction row `identity`, read
    /// at `now`, to whoever writes such stamps in batches
    /// ([`crate::store::PredictionTouchSink`]). `false`, the default, makes
    /// the read write it itself.
    fn hand_prediction_touch(&self, _identity: &str, _now: i64) -> bool {
        false
    }
}

/// A [`FileHashCache`]'s connection, for as long as one statement needs it.
pub enum IndexRef<'a> {
    Plain(&'a Connection),
    Lent(crate::index_pool::IndexGuard<'a>),
}

impl std::ops::Deref for IndexRef<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Self::Plain(db) => db,
            Self::Lent(db) => db,
        }
    }
}

pub enum FileHashCache<'db> {
    Borrowed(&'db Connection),
    /// [`Self::Borrowed`], writing new input prediction rows as zstd frames
    /// ([`crate::prediction_codec`]): a store opened with
    /// `compress_predictions` hands this out.
    Compressing(&'db Connection),
    /// A store's connection, lent for each statement, so a pooled store can
    /// give it back while the cache lives ([`crate::index_pool`]); `true`
    /// writes as [`Self::Compressing`] does.
    Store(&'db dyn IndexSource, bool),
    #[cfg(any(test, feature = "test-support"))]
    Owned(Connection),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    /// Filesystem inode or Windows file index (0 where unavailable). Folded into the memo key
    /// so an in-place swap that preserves path+size+mtime+ctime but changes the
    /// inode (and content) can't return a stale memoized hash (kunobi-ninja/kache#324).
    pub inode: i64,
}

/// Result of a content-hash cache lookup that does NOT compute a blake3 — the
/// lock-narrowing seam for the daemon's `HashFiles` path (#281). The caller
/// hashes (`hash_file`) outside any store lock on a miss, then records via
/// [`FileHashCache::record_cached`].
pub enum FileHashLookup {
    /// Cached hash found — no hashing needed.
    Hit(String),
    /// Cache miss; hash the file then record under this fingerprint.
    NeedsHash(FileFingerprint),
    /// Too small to persist, or metadata unreadable — hash but don't cache.
    Uncacheable,
}

impl<'db> FileHashCache<'db> {
    #[cfg(any(test, feature = "test-support"))]
    pub fn open(index_db_path: &Path) -> Result<Self> {
        let db = Connection::open(index_db_path)
            .with_context(|| format!("opening file hash cache {}", index_db_path.display()))?;
        db.pragma_update(None, "busy_timeout", "5000")?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        ensure_file_hash_cache_schema(&db)?;
        Ok(Self::Owned(db))
    }

    /// [`IndexSource::hand_prediction_touch`] of the store this cache lends
    /// its connection from; `false` for a plain connection.
    fn hand_prediction_touch(&self, identity: &str, now: i64) -> bool {
        match self {
            Self::Store(store, _) => store.hand_prediction_touch(identity, now),
            _ => false,
        }
    }

    pub fn db(&self) -> IndexRef<'_> {
        match self {
            Self::Borrowed(db) | Self::Compressing(db) => IndexRef::Plain(db),
            Self::Store(store, _) => IndexRef::Lent(store.index()),
            #[cfg(any(test, feature = "test-support"))]
            Self::Owned(db) => IndexRef::Plain(db),
        }
    }

    /// The memoised hash for `fingerprint`: a row whose whole stamp matches,
    /// recorded under [`FILE_HASH_RULE`] or a later rule, in this boot
    /// ([`memo_rows_valid_since`]).
    pub fn get(&self, fingerprint: &FileFingerprint) -> rusqlite::Result<Option<String>> {
        if !file_hash_memo_eligible(fingerprint.inode, cfg!(windows)) {
            return Ok(None);
        }
        self.db()
            .query_row(
                "SELECT hash FROM file_hashes
                 WHERE path = ?1 AND size = ?2 AND mtime_ns = ?3 AND ctime_ns = ?4 AND inode = ?5
                   AND rule >= ?6 AND updated_at >= datetime(?7, 'unixepoch')",
                params![
                    fingerprint.path,
                    fingerprint.size,
                    fingerprint.mtime_ns,
                    fingerprint.ctime_ns,
                    fingerprint.inode,
                    FILE_HASH_RULE,
                    memo_rows_valid_since()
                ],
                |row| row.get(0),
            )
            .optional()
    }

    /// Whether `hash`, just read from the file at `fingerprint` before its
    /// stamp settled, may be memoised all the same: a
    /// [`FILE_HASH_RULE_REDATED`] row holds that hash for the same path,
    /// size, inode and mtime, at a ctime at most [`REDATE_CTIME_WINDOW_NS`]
    /// earlier. The stamp is then Cargo's re-date of bytes kache wrote.
    pub fn redated_pin(&self, fingerprint: &FileFingerprint, hash: &str) -> bool {
        if !file_hash_memo_eligible(fingerprint.inode, cfg!(windows)) {
            return false;
        }
        self.db()
            .query_row(
                "SELECT 1 FROM file_hashes
                 WHERE path = ?1 AND size = ?2 AND mtime_ns = ?3 AND inode = ?4 AND rule = ?5
                   AND hash = ?6 AND ctime_ns <= ?7 AND ?7 - ctime_ns <= ?8",
                params![
                    fingerprint.path,
                    fingerprint.size,
                    fingerprint.mtime_ns,
                    fingerprint.inode,
                    FILE_HASH_RULE_REDATED,
                    hash,
                    fingerprint.ctime_ns,
                    REDATE_CTIME_WINDOW_NS
                ],
                |_| Ok(()),
            )
            .optional()
            .is_ok_and(|row| row.is_some())
    }

    /// Memoised hashes for many files in one statement per chunk, keyed by
    /// path. A row counts only when its whole stamp matches, so a file that
    /// changed since it was recorded reads as absent, exactly as with [`get`].
    ///
    /// [`get`]: Self::get
    pub fn get_many(
        &self,
        fingerprints: &[&FileFingerprint],
    ) -> rusqlite::Result<std::collections::HashMap<String, String>> {
        let wanted: std::collections::HashSet<&FileFingerprint> =
            fingerprints.iter().copied().collect();
        let paths: Vec<&str> = fingerprints.iter().map(|f| f.path.as_str()).collect();
        Ok(self
            .recorded_at(&paths)?
            .into_values()
            .filter(|(stamp, _)| wanted.contains(stamp))
            .map(|(stamp, hash)| (stamp.path, hash))
            .collect())
    }

    /// The memo's row for each of `paths` that has one, keyed by path: the
    /// stamp it was recorded under and the hash, under [`FILE_HASH_RULE`] or
    /// a later rule, in this boot ([`memo_rows_valid_since`]). A row stands for the file's bytes only while the file's
    /// whole stamp is the recorded one; the caller compares the two, as
    /// [`get`](Self::get) does.
    pub fn recorded_at(
        &self,
        paths: &[&str],
    ) -> rusqlite::Result<std::collections::HashMap<String, (FileFingerprint, String)>> {
        let mut found = std::collections::HashMap::new();
        let since = memo_rows_valid_since();
        for chunk in paths.chunks(FILE_HASH_LOOKUP_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let db = self.db();
            let mut stmt = db.prepare_cached(&format!(
                "SELECT path, size, mtime_ns, ctime_ns, inode, hash FROM file_hashes
                 WHERE path IN ({placeholders}) AND rule >= {FILE_HASH_RULE}
                   AND updated_at >= datetime({since}, 'unixepoch')"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((
                    FileFingerprint {
                        path: row.get(0)?,
                        size: row.get(1)?,
                        mtime_ns: row.get(2)?,
                        ctime_ns: row.get(3)?,
                        inode: row.get(4)?,
                    },
                    row.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (stamp, hash) = row?;
                if file_hash_memo_eligible(stamp.inode, cfg!(windows)) {
                    found.insert(stamp.path.clone(), (stamp, hash));
                }
            }
        }
        Ok(found)
    }

    pub fn put(&self, fingerprint: &FileFingerprint, hash: &str) -> rusqlite::Result<()> {
        self.put_under_rule(fingerprint, hash, FILE_HASH_RULE)
    }

    /// [`Self::put`] for an output whose mtime kache has already set to the
    /// time Cargo will re-date it to ([`FILE_HASH_RULE_REDATED`]).
    pub fn put_redated(&self, fingerprint: &FileFingerprint, hash: &str) -> rusqlite::Result<()> {
        if fingerprint.size < MIN_PERSISTED_HASH_BYTES {
            return Ok(());
        }
        self.put_under_rule(fingerprint, hash, FILE_HASH_RULE_REDATED)
    }

    fn put_under_rule(
        &self,
        fingerprint: &FileFingerprint,
        hash: &str,
        rule: i64,
    ) -> rusqlite::Result<()> {
        if !file_hash_memo_eligible(fingerprint.inode, cfg!(windows)) {
            return Ok(());
        }
        self.db().execute(
            "INSERT OR REPLACE INTO file_hashes
             (path, size, mtime_ns, ctime_ns, inode, hash, updated_at, rule)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), ?7)",
            params![
                fingerprint.path,
                fingerprint.size,
                fingerprint.mtime_ns,
                fingerprint.ctime_ns,
                fingerprint.inode,
                hash,
                rule
            ],
        )?;
        Ok(())
    }

    /// The stored classification of how the source with `content_hash` uses
    /// `var`. `scanner` is the caller's scanner version: a row recorded by any
    /// other version reads as absent, so a scanner fix never reuses an answer
    /// the old scanner gave for unchanged source. `use_kind` is opaque here.
    pub fn get_source_env_dep_use(
        &self,
        content_hash: &str,
        var: &str,
        scanner: u32,
    ) -> rusqlite::Result<Option<i64>> {
        self.db()
            .query_row(
                "SELECT use_kind FROM source_env_dep_uses
                 WHERE content_hash = ?1 AND env_var = ?2 AND scanner = ?3",
                params![content_hash, var, scanner],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn put_source_env_dep_use(
        &self,
        content_hash: &str,
        var: &str,
        scanner: u32,
        use_kind: i64,
    ) -> rusqlite::Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO source_env_dep_uses
             (content_hash, env_var, scanner, use_kind, updated_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![content_hash, var, scanner, use_kind],
        )?;
        Ok(())
    }

    /// Whether any entry, committed or not, was stored under `crate_name`
    /// for `unit`, Cargo's `-C metadata` hash, or with no unit recorded.
    ///
    /// Every cache key folds the crate name and that hash in, so `false`
    /// means no key this unit can produce is in the local store. A row with
    /// no unit (stored by an older kache, or by a path that never learns
    /// the unit) counts for every unit of its crate name. An empty `unit`
    /// asks about the crate name alone.
    pub fn has_entry_for_unit(&self, crate_name: &str, unit: &str) -> rusqlite::Result<bool> {
        if unit.is_empty() {
            return self.db().query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE crate_name = ?1)",
                params![crate_name],
                |row| row.get(0),
            );
        }
        self.db().query_row(
            "SELECT EXISTS(SELECT 1 FROM entries
                           WHERE crate_name = ?1 AND unit_id IN (?2, ''))",
            params![crate_name, unit],
            |row| row.get(0),
        )
    }

    /// Return the stored schema and payload for `identity`, or `None` when absent.
    /// The caller validates the schema before interpreting the payload.
    ///
    /// A read is a use: it refreshes the row's `last_used` stamp, at most once
    /// per [`INPUT_PREDICTION_TOUCH_INTERVAL_SECS`] so a build does not issue
    /// one write per unit.
    pub fn get_input_prediction(&self, identity: &str) -> rusqlite::Result<Option<(u32, String)>> {
        self.get_input_prediction_at(identity, unix_now())
    }

    fn get_input_prediction_at(
        &self,
        identity: &str,
        now: i64,
    ) -> rusqlite::Result<Option<(u32, String)>> {
        // Both forms a row can take (`prediction_codec`); one this release
        // cannot read is absent, as a row of an unknown schema is.
        let row: Option<(Option<(u32, String)>, i64)> = self
            .db()
            .query_row(
                "SELECT schema, prediction_json, last_used FROM input_predictions
                 WHERE identity = ?1",
                params![identity],
                |row| Ok((codec::decode(row.get(0)?, row.get_ref(1)?), row.get(2)?)),
            )
            .optional()?;
        let Some((Some((schema, json)), last_used)) = row else {
            return Ok(None);
        };
        // Best-effort: a stamp that cannot be written costs an early prune
        // and a repeated pre-pass, never the hit. A store with a touch sink
        // (the daemon's workers) hands it over: the write would take the
        // index's write lock on the hit path, behind a landing's burst of
        // stores.
        if prediction_touch_due(last_used, now)
            && !self.hand_prediction_touch(identity, now)
            && let Err(error) = self.db().execute(
                "UPDATE input_predictions SET last_used = ?2 WHERE identity = ?1",
                params![identity, now],
            )
        {
            tracing::debug!("input prediction touch failed: {error}");
        }
        Ok(Some((schema, json)))
    }

    /// Record the closure `prediction_json` under `identity`, compressed
    /// when this cache is [`Self::Compressing`] and the row is not a build
    /// script's ([`codec::Codec::for_write`]).
    pub fn put_input_prediction(
        &self,
        identity: &str,
        schema: u32,
        crate_name: Option<&str>,
        prediction_json: &str,
    ) -> rusqlite::Result<()> {
        self.put_input_prediction_as(
            codec::Codec::for_write(
                matches!(self, Self::Compressing(_) | Self::Store(_, true)),
                identity,
            ),
            identity,
            schema,
            crate_name,
            prediction_json,
        )
    }

    /// [`Self::put_input_prediction`] in the form `codec` says.
    pub fn put_input_prediction_as(
        &self,
        codec: codec::Codec,
        identity: &str,
        schema: u32,
        crate_name: Option<&str>,
        prediction_json: &str,
    ) -> rusqlite::Result<()> {
        let (stored_schema, payload) = codec::encode(codec, schema, prediction_json);
        self.db().execute(
            "INSERT OR REPLACE INTO input_predictions
             (identity, schema, crate_name, prediction_json, updated_at, last_used)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), unixepoch())",
            params![identity, stored_schema, crate_name, payload],
        )?;
        Ok(())
    }

    /// [`Self::put_input_prediction`] for one of several rows of a unit, all
    /// named `prefix` and something after it: the row is written, and of the
    /// rows under `prefix` only the `keep` most recently used or written stay.
    /// The unit's other rows, for trees no build has used since, go at once
    /// instead of at the retention prune.
    pub fn put_input_prediction_keeping(
        &self,
        prefix: &str,
        identity: &str,
        schema: u32,
        crate_name: Option<&str>,
        prediction_json: &str,
        keep: usize,
    ) -> rusqlite::Result<()> {
        // One write transaction for the row and the trim, not two: each
        // takes the index's write lock, which a burst of stores queues on.
        let db = self.db();
        let tx =
            rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)?;
        self.put_input_prediction_trimming(
            prefix,
            identity,
            schema,
            crate_name,
            prediction_json,
            keep,
        )?;
        tx.commit()
    }

    /// [`Self::put_input_prediction_keeping`] inside a write transaction the
    /// caller holds: the daemon writes a batch of handed rows in one.
    pub fn put_input_prediction_trimming(
        &self,
        prefix: &str,
        identity: &str,
        schema: u32,
        crate_name: Option<&str>,
        prediction_json: &str,
        keep: usize,
    ) -> rusqlite::Result<()> {
        debug_assert!(identity.starts_with(prefix) && identity.len() > prefix.len());
        self.put_input_prediction(identity, schema, crate_name, prediction_json)?;
        // Every name longer than `prefix` that starts with it sorts after it
        // and before `prefix` + U+10FFFF, so the primary key serves the range.
        let upper = format!("{prefix}\u{10FFFF}");
        self.db().execute(
            "DELETE FROM input_predictions
             WHERE identity > ?1 AND identity < ?2 AND identity NOT IN (
                 SELECT identity FROM input_predictions
                 WHERE identity > ?1 AND identity < ?2
                 ORDER BY last_used DESC, updated_at DESC, identity = ?3 DESC
                 LIMIT ?4)",
            params![
                prefix,
                upper,
                identity,
                i64::try_from(keep).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// The names of the rows named `prefix` and something after it, most
    /// recently used or written first, at most `limit` of them. Reads only:
    /// the caller reads the row it wants with [`Self::get_input_prediction`].
    pub fn input_prediction_names_under(
        &self,
        prefix: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<String>> {
        let upper = format!("{prefix}\u{10FFFF}");
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT identity FROM input_predictions
             WHERE identity > ?1 AND identity < ?2
             ORDER BY last_used DESC, updated_at DESC
             LIMIT ?3",
        )?;
        statement
            .query_map(
                params![prefix, upper, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| row.get(0),
            )?
            .collect()
    }

    /// Is any row named `prefix` and something after it recorded?
    pub fn has_input_predictions_under(&self, prefix: &str) -> rusqlite::Result<bool> {
        let upper = format!("{prefix}\u{10FFFF}");
        self.db().query_row(
            "SELECT EXISTS(SELECT 1 FROM input_predictions WHERE identity > ?1 AND identity < ?2)",
            params![prefix, upper],
            |row| row.get(0),
        )
    }

    /// Delete predictions no build has read or recorded for
    /// [`INPUT_PREDICTION_RETENTION_SECS`], all of them: `doctor --repair`.
    /// The GC sweep bounds its time with
    /// [`Self::prune_input_predictions_within`]. Freed pages go to the
    /// freelist; index compaction returns them to disk.
    pub fn prune_input_predictions(&self) -> rusqlite::Result<usize> {
        self.prune_input_predictions_at(unix_now())
    }

    /// Delete predictions no build has read or recorded for `retention`
    /// seconds before `now`, for at most `budget`, in write transactions of
    /// [`PREDICTION_PRUNE_BATCH`] rows with a pause as long as each between
    /// them, so that waiting writers get the lock. A prune the budget or a
    /// writer holding the lock cuts short goes on at the next sweep.
    ///
    /// One `DELETE` of every expired row held the index's write lock for as
    /// long as it took: the retention window moves on by a sweep's interval
    /// at a time, so that is little while the GC runs often, but a sweep
    /// after a pause, or a shorter window, meets millions of rows.
    ///
    /// A build script's rows ([`BUILD_SCRIPT_ROW_PREFIXES`]) keep at least
    /// [`INPUT_PREDICTION_RETENTION_SECS`] whatever `retention` says: such a
    /// row names the inputs a run read, and without it the script runs
    /// again, which costs more than a rustc pre-pass, for rows that take
    /// next to no room.
    pub fn prune_input_predictions_within(
        &self,
        now: i64,
        retention: i64,
        budget: Duration,
    ) -> rusqlite::Result<PredictionPrune> {
        let cutoff = now.saturating_sub(retention);
        self.prune_input_predictions_batched(
            cutoff,
            cutoff.min(prediction_prune_cutoff(now)),
            budget,
            PREDICTION_PRUNE_BATCH,
        )
    }

    /// Delete file hash rows not written for [`FILE_HASH_RETENTION_SECS`].
    /// Run from the GC sweep and `doctor --repair`, like
    /// [`Self::prune_input_predictions`]. A lookup never refreshes a row, so
    /// that the hit path stays read-only; the price is one re-hash a month
    /// for a file read that whole time without changing.
    pub fn prune_file_hashes(&self) -> rusqlite::Result<usize> {
        self.prune_file_hashes_at(unix_now())
    }

    /// Rebuild an older store's `file_hashes` as a `WITHOUT ROWID` table,
    /// which keeps each path once instead of in the table and again in its
    /// primary key index: 27% smaller on a real 1.3-million-row table
    /// (kunobi-ninja/kache#1206). Copying takes the write lock for about a
    /// second per million rows, so it runs from the GC sweep after the prune,
    /// never when a build opens the index. Returns whether it rebuilt. A
    /// table without a `rule` column copies its rows at rule 0, which no
    /// lookup serves.
    pub fn rebuild_file_hashes_without_rowid(&self) -> rusqlite::Result<bool> {
        let db = self.db();
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        if !file_hashes_has_rowid(&tx)? {
            return Ok(false);
        }
        let rule = if file_hashes_have_rule(&tx)? {
            "rule"
        } else {
            "0"
        };
        tx.execute_batch(&format!(
            "CREATE TABLE file_hashes_rebuilt ({FILE_HASHES_COLUMNS}) WITHOUT ROWID;
             INSERT INTO file_hashes_rebuilt
                 (path, size, mtime_ns, ctime_ns, inode, hash, updated_at, rule)
                 SELECT path, size, mtime_ns, ctime_ns, inode, hash, updated_at, {rule}
                 FROM file_hashes;
             DROP TABLE file_hashes;
             ALTER TABLE file_hashes_rebuilt RENAME TO file_hashes;
             CREATE INDEX IF NOT EXISTS file_hashes_updated_at ON file_hashes(updated_at);"
        ))?;
        tx.commit()?;
        Ok(true)
    }

    fn prune_file_hashes_at(&self, now: i64) -> rusqlite::Result<usize> {
        // Built here, not when a wrapper opens the index, for the reason
        // given in `prune_input_predictions_at`.
        self.db().execute_batch(
            "CREATE INDEX IF NOT EXISTS file_hashes_updated_at
             ON file_hashes(updated_at)",
        )?;
        self.db().execute(
            "DELETE FROM file_hashes WHERE updated_at < datetime(?1, 'unixepoch')",
            params![now.saturating_sub(FILE_HASH_RETENTION_SECS)],
        )
    }

    fn prune_input_predictions_at(&self, now: i64) -> rusqlite::Result<usize> {
        self.prune_input_predictions_batched(
            prediction_prune_cutoff(now),
            prediction_prune_cutoff(now),
            Duration::MAX,
            PREDICTION_PRUNE_BATCH,
        )
        .map(|pruned| pruned.rows)
    }

    /// Rows unused since `cutoff` go, a build script's since `script_cutoff`.
    fn prune_input_predictions_batched(
        &self,
        cutoff: i64,
        script_cutoff: i64,
        budget: Duration,
        batch: usize,
    ) -> rusqlite::Result<PredictionPrune> {
        let mut done = PredictionPrune::default();
        // Built here, not when a wrapper upgrades the table: indexing a large
        // table takes the write lock for seconds, and this runs off the build.
        match self.db().execute_batch(
            "CREATE INDEX IF NOT EXISTS input_predictions_last_used
             ON input_predictions(last_used)",
        ) {
            Ok(()) => {}
            Err(error) if crate::gone_dirs::is_busy(&error) => {
                done.cut_short = true;
                return Ok(done);
            }
            Err(error) => return Err(error),
        }
        let start = Instant::now();
        loop {
            if start.elapsed() >= budget {
                done.cut_short = true;
                return Ok(done);
            }
            let held = Instant::now();
            let deleted = match self.delete_expired_predictions(cutoff, script_cutoff, batch) {
                Ok(deleted) => deleted,
                Err(error) if crate::gone_dirs::is_busy(&error) => {
                    done.cut_short = true;
                    return Ok(done);
                }
                Err(error) => return Err(error),
            };
            let held = held.elapsed();
            done.longest_batch = done.longest_batch.max(held);
            done.batches += 1;
            done.rows += deleted;
            if deleted < batch {
                return Ok(done);
            }
            std::thread::sleep(held.max(Duration::from_millis(5)));
        }
    }

    fn delete_expired_predictions(
        &self,
        cutoff: i64,
        script_cutoff: i64,
        batch: usize,
    ) -> rusqlite::Result<usize> {
        let [first, second] = BUILD_SCRIPT_ROW_PREFIXES;
        let db = self.db();
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        // `prefix` and every name that starts with it sort below `prefix` +
        // U+10FFFF, as in `put_input_prediction_trimming`.
        let deleted = tx.execute(
            "DELETE FROM input_predictions WHERE rowid IN (
                 SELECT rowid FROM input_predictions
                 WHERE last_used < ?1
                   AND (last_used < ?2
                        OR NOT (identity >= ?3 AND identity < ?3 || char(1114111))
                           AND NOT (identity >= ?4 AND identity < ?4 || char(1114111)))
                 LIMIT ?5)",
            params![
                cutoff,
                script_cutoff,
                first,
                second,
                i64::try_from(batch).unwrap_or(i64::MAX)
            ],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Rewrite plain input prediction rows as zstd frames
    /// ([`crate::prediction_codec`]), for at most `budget`, in write
    /// transactions of [`PREDICTION_CONVERSION_BATCH`] rows with a pause as
    /// long as each between them, as the prediction prune goes. A build
    /// script's rows, and rows a frame would not shrink, stay plain. A row
    /// keeps its name, `last_used` and `updated_at`, so trims and prunes
    /// order it as before; it moves to the end of the table (a new rowid),
    /// so that the pages it leaves are freed.
    ///
    /// Rows are walked in rowid order. Each batch's rows are found by a read
    /// outside the write lock, then read again, compressed and written again
    /// under it: a row a writer replaced meanwhile has another rowid, or is
    /// no longer plain, and is left alone. A conversion the budget or a
    /// writer holding the lock cuts short goes on at the next sweep, from
    /// the first row again; the rows converted by then are passed over by
    /// that read.
    pub fn compress_input_predictions_within(
        &self,
        budget: Duration,
    ) -> rusqlite::Result<PredictionConversion> {
        self.compress_input_predictions_batched(budget, PREDICTION_CONVERSION_BATCH)
    }

    fn compress_input_predictions_batched(
        &self,
        budget: Duration,
        batch: usize,
    ) -> rusqlite::Result<PredictionConversion> {
        let mut done = PredictionConversion::default();
        let start = Instant::now();
        let mut after = 0i64;
        loop {
            if start.elapsed() >= budget {
                done.cut_short = true;
                return Ok(done);
            }
            let rowids = match self.plain_prediction_rowids(after, batch) {
                Ok(rowids) => rowids,
                Err(error) if crate::gone_dirs::is_busy(&error) => {
                    done.cut_short = true;
                    return Ok(done);
                }
                Err(error) => return Err(error),
            };
            let Some(&last) = rowids.last() else {
                return Ok(done);
            };
            after = last;
            let held = Instant::now();
            match self.compress_prediction_rows(&rowids) {
                Ok((converted, kept_plain)) => {
                    done.rows += converted;
                    done.kept_plain += kept_plain;
                }
                Err(error) if crate::gone_dirs::is_busy(&error) => {
                    done.cut_short = true;
                    return Ok(done);
                }
                Err(error) => return Err(error),
            }
            let held = held.elapsed();
            done.longest_batch = done.longest_batch.max(held);
            done.batches += 1;
            if rowids.len() < batch {
                return Ok(done);
            }
            std::thread::sleep(held.max(Duration::from_millis(5)));
        }
    }

    /// The rowids after `after` of up to `batch` plain rows that are not a
    /// build script's, in order. `typeof` reads only the record's header.
    fn plain_prediction_rowids(&self, after: i64, batch: usize) -> rusqlite::Result<Vec<i64>> {
        let [first, second] = BUILD_SCRIPT_ROW_PREFIXES;
        let db = self.db();
        let mut statement = db.prepare_cached(
            "SELECT rowid FROM input_predictions
             WHERE rowid > ?1 AND typeof(prediction_json) = 'text'
               AND NOT (identity >= ?2 AND identity < ?2 || char(1114111))
               AND NOT (identity >= ?3 AND identity < ?3 || char(1114111))
             ORDER BY rowid LIMIT ?4",
        )?;
        statement
            .query_map(
                params![
                    after,
                    first,
                    second,
                    i64::try_from(batch).unwrap_or(i64::MAX)
                ],
                |row| row.get(0),
            )?
            .collect()
    }

    /// Compress the rows `rowids` that are still plain, in one write
    /// transaction. Returns how many were rewritten and how many stayed
    /// plain because their frame would not be smaller.
    fn compress_prediction_rows(&self, rowids: &[i64]) -> rusqlite::Result<(usize, usize)> {
        let db = self.db();
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        let (mut converted, mut kept_plain) = (0, 0);
        {
            let mut read = tx.prepare_cached(
                "SELECT identity, schema, crate_name, prediction_json, updated_at, last_used
                 FROM input_predictions
                 WHERE rowid = ?1 AND typeof(prediction_json) = 'text'",
            )?;
            let mut delete = tx.prepare_cached("DELETE FROM input_predictions WHERE rowid = ?1")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO input_predictions
                 (identity, schema, crate_name, prediction_json, updated_at, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for &rowid in rowids {
                let row: Option<PlainRow> = read
                    .query_row(params![rowid], |row| {
                        Ok(PlainRow {
                            identity: row.get(0)?,
                            schema: row.get(1)?,
                            crate_name: row.get(2)?,
                            json: row.get(3)?,
                            updated_at: row.get(4)?,
                            last_used: row.get(5)?,
                        })
                    })
                    .optional()?;
                let Some(row) = row else {
                    continue;
                };
                if codec::Codec::for_write(true, &row.identity) == codec::Codec::Plain {
                    continue;
                }
                let (stored_schema, payload) =
                    codec::encode(codec::Codec::Zstd, row.schema, &row.json);
                if stored_schema == row.schema {
                    kept_plain += 1;
                    continue;
                }
                // Deleted and inserted again, not updated: a cell that shrinks
                // in place leaves its page as full as SQLite counts it, so
                // the table would keep a page per row of about 4 KB. A delete
                // lets the emptied pages go to the freelist, and the inserts
                // fill new pages at the end of the table.
                delete.execute(params![rowid])?;
                insert.execute(params![
                    row.identity,
                    stored_schema,
                    row.crate_name,
                    payload,
                    row.updated_at,
                    row.last_used
                ])?;
                converted += 1;
            }
        }
        tx.commit()?;
        Ok((converted, kept_plain))
    }
}

/// A plain row as [`FileHashCache::compress_input_predictions_within`]
/// reads it before writing it again compressed.
struct PlainRow {
    identity: String,
    schema: u32,
    crate_name: Option<String>,
    json: String,
    updated_at: String,
    last_used: i64,
}

/// What the rows of build scripts' input predictions are named first
/// (`build_script.rs` in the kache crate checks that it names them so).
pub const BUILD_SCRIPT_ROW_PREFIXES: [&str; 2] = ["build-script:", "unsettled:"];

/// Rows one write transaction of the prediction prune deletes. A row holds
/// the closure of one unit, about 4 KB on a shared build host's index and up
/// to 300 KB for a crate that embeds thousands of files, each its own
/// overflow pages to free.
pub const PREDICTION_PRUNE_BATCH: usize = 100;

/// How long one GC sweep spends on the prediction prune.
pub const PREDICTION_PRUNE_BUDGET: Duration = Duration::from_secs(60);

/// Rows one write transaction of the prediction conversion rewrites
/// ([`FileHashCache::compress_input_predictions_within`]).
pub const PREDICTION_CONVERSION_BATCH: usize = 100;

/// How long one GC sweep spends converting plain prediction rows.
pub const PREDICTION_CONVERSION_BUDGET: Duration = Duration::from_secs(60);

/// What one [`FileHashCache::compress_input_predictions_within`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PredictionConversion {
    /// Rows rewritten as zstd frames.
    pub rows: usize,
    /// Rows left plain because their frame would not be smaller.
    pub kept_plain: usize,
    /// Write transactions the rewrites took.
    pub batches: usize,
    /// The longest any batch held the write lock.
    pub longest_batch: Duration,
    /// Whether the budget, or a writer holding the lock, stopped the
    /// conversion before the last plain row.
    pub cut_short: bool,
}

/// What one [`FileHashCache::prune_input_predictions_within`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PredictionPrune {
    /// Rows deleted.
    pub rows: usize,
    /// Write transactions the deletes took.
    pub batches: usize,
    /// The longest any batch held the write lock.
    pub longest_batch: Duration,
    /// Whether the budget, or a writer holding the lock, stopped the prune
    /// before the last expired row.
    pub cut_short: bool,
}

/// How long a file hash row is kept after it was last written. The memo is
/// only a saving: a row pruned while its file is still in use costs one
/// re-read of that file, at most once per retention window. Without a bound
/// the table kept every path any build ever hashed, so worktrees and
/// checkouts deleted long ago stayed in it for good (kunobi-ninja/kache#1206).
pub const FILE_HASH_RETENTION_SECS: i64 = 30 * 86_400;

/// A prediction hit refreshes `last_used` only when the stamp is this old.
pub const INPUT_PREDICTION_TOUCH_INTERVAL_SECS: i64 = 86_400;

/// How long an unused prediction is kept. The same window as the C/C++
/// preprocess memos: long enough for a branch left alone for a few weeks,
/// short enough that identities no build produces any more stop piling up.
pub const INPUT_PREDICTION_RETENTION_SECS: i64 = 30 * 86_400;

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn prediction_touch_due(last_used: i64, now: i64) -> bool {
    now.saturating_sub(last_used) >= INPUT_PREDICTION_TOUCH_INTERVAL_SECS
}

fn prediction_prune_cutoff(now: i64) -> i64 {
    now.saturating_sub(INPUT_PREDICTION_RETENTION_SECS)
}

fn input_predictions_have_last_used(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('input_predictions') WHERE name = 'last_used')",
        [],
        |row| row.get(0),
    )
}

const INPUT_PREDICTIONS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS input_predictions (
    identity TEXT PRIMARY KEY,
    schema INTEGER NOT NULL,
    crate_name TEXT,
    prediction_json TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used INTEGER NOT NULL DEFAULT (unixepoch())
);";

fn input_predictions_have_current_payload(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('input_predictions')
                       WHERE name = 'prediction_json')
            AND EXISTS(SELECT 1 FROM pragma_table_info('input_predictions')
                       WHERE name = 'crate_name' AND \"notnull\" = 0)",
        [],
        |row| row.get(0),
    )
}

/// Old indexes used `record_json` and required a crate name. Their memo
/// payloads have no supported migration; relearn predictions from source.
/// Compiled entries and blobs are separate tables and stay intact.
fn ensure_input_predictions_payload(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(INPUT_PREDICTIONS_SCHEMA)?;
    if input_predictions_have_current_payload(db)? {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    if !input_predictions_have_current_payload(&tx)? {
        tx.execute_batch("DROP TABLE input_predictions;")?;
        tx.execute_batch(INPUT_PREDICTIONS_SCHEMA)?;
    }
    tx.commit()
}

/// Add `last_used` to a table created before the column existed.
///
/// The default is the migration time as a literal: SQLite refuses an
/// expression default in `ADD COLUMN`, and a constant one is recorded in the
/// schema without rewriting a row, so the upgrade costs the same on an empty
/// table and a 200 MB one. Existing rows therefore read as used now, and the
/// first prune of them comes a full retention window later. `updated_at` is
/// not used as the starting stamp: it records the last write, and a
/// prediction for a dependency that never changes is read daily and written
/// once.
fn ensure_input_predictions_last_used(db: &Connection, now: i64) -> rusqlite::Result<()> {
    if input_predictions_have_last_used(db)? {
        return Ok(());
    }
    // Recheck under the writer lock: many wrappers open the index at once,
    // and a second ALTER fails with a duplicate column.
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    if !input_predictions_have_last_used(&tx)? {
        tx.execute_batch(&format!(
            "ALTER TABLE input_predictions ADD COLUMN last_used INTEGER NOT NULL DEFAULT {now}"
        ))?;
    }
    tx.commit()
}

/// The columns of `file_hashes`, for the table a new store creates and for
/// the rebuild of an older one. `rule` is the [`FILE_HASH_RULE`] the writer
/// followed; a release from before the column leaves it at 0.
const FILE_HASHES_COLUMNS: &str = "
    path       TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    mtime_ns   INTEGER NOT NULL,
    ctime_ns   INTEGER NOT NULL DEFAULT 0,
    inode      INTEGER NOT NULL DEFAULT 0,
    hash       TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    rule       INTEGER NOT NULL DEFAULT 0";

/// The rule a `file_hashes` row was recorded under. Lookups serve only rows
/// at this rule or a later one, which may only add conditions. Rows from a
/// release that predates the `rule` column, written before an upgrade or by
/// that release afterwards, read as 0 and are never served.
///
/// 1: a hash read from a file is memoised only if the file's stamp had
///    settled when it was read ([`stamp_is_settled`]).
pub const FILE_HASH_RULE: i64 = 1;

/// The rule of a row kache recorded for an output Cargo re-dates after the
/// unit ends. It is [`FILE_HASH_RULE`] with a marker bit, so a release that
/// knows only the plain rule serves it as an ordinary exact-stamp row.
///
/// Since Cargo 1.90 a `cargo check` (or `clippy`) unit's outputs get their
/// mtime set to the unit's invocation time once the unit's process has
/// exited (`invoked.timestamp` in the unit's fingerprint folder). That
/// `utimensat` cannot keep the ctime, so a row recorded while the unit ran
/// never matched again, and every dependent that started within the settle
/// window read the `.rmeta` again: the read could not be memoised, since the
/// re-date had just changed the stamp. kache sets the same mtime itself
/// before it records the row, so Cargo's own call changes only the ctime.
///
/// Such a row is never served for another stamp. It lets a reader that hashed
/// the file memoise what it read before the stamp has settled
/// ([`FileHashCache::redated_pin`]): when the bytes hash to the row's hash and
/// the stamp differs from the row's only by a ctime up to
/// [`REDATE_CTIME_WINDOW_NS`] later. The settle window guards against a second
/// write in the same timestamp tick, which keeps the stamp. Here that write
/// would have to come after the read, within the tick of Cargo's re-date, and
/// set the mtime back to the unit's invocation time to the nanosecond: an
/// ordinary write stamps the current time. So the first dependent reads the
/// `.rmeta` and every later one is served its row.
pub const FILE_HASH_RULE_REDATED: i64 = FILE_HASH_RULE | (1 << 16);

/// How long after a [`FILE_HASH_RULE_REDATED`] row was recorded a ctime still
/// counts as Cargo's re-date: the time from the unit's exit to Cargo's
/// `utimensat` (under 1 s measured on a loaded machine), with room to spare.
/// Later, the settle window has passed for that stamp anyway.
pub const REDATE_CTIME_WINDOW_NS: i64 = 10_000_000_000;

fn file_hashes_have_rule(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('file_hashes') WHERE name = 'rule')",
        [],
        |row| row.get(0),
    )
}

/// Add `rule` to a `file_hashes` table from before it, emptying the table:
/// its rows were recorded under no known rule, and some without checking
/// that the file had settled. Checked again under the write lock, so the
/// rows go once however many processes open the index together. Gated on
/// the column rather than the index generation, which an older release
/// stamps back over a newer one each time it opens the index. An older
/// release's GC can drop the column again ([`has_rule_columns`]). The rows
/// it copies then no longer say which rule recorded them, so adding the
/// column back empties the table again.
fn ensure_file_hashes_rule(db: &Connection) -> rusqlite::Result<()> {
    if file_hashes_have_rule(db)? {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    if !file_hashes_have_rule(&tx)? {
        tx.execute_batch(
            "ALTER TABLE file_hashes ADD COLUMN rule INTEGER NOT NULL DEFAULT 0;
             DELETE FROM file_hashes;",
        )?;
    }
    tx.commit()
}

/// Whether `file_hashes` is still the rowid table older stores created. A
/// rowid table keys its rows by a hidden integer, so `path` sits in the table
/// and again in the automatic index that enforces its primary key.
fn file_hashes_has_rowid(db: &Connection) -> rusqlite::Result<bool> {
    let sql: Option<String> = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'file_hashes'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(sql.is_some_and(|sql| !sql.to_ascii_uppercase().contains("WITHOUT ROWID")))
}

/// Whether the memo schemas are current: the rule columns and the
/// prediction payload and retention columns. The index generation
/// cannot vouch for them. GC in releases 0.27 to 1.0 rebuilds a rowid
/// `file_hashes` table without `rule` and leaves the generation alone, so
/// an index this release stamped while that GC ran keeps the stamp and
/// loses the column. Builds made while this release was in development
/// also stamped generation 9 before the columns existed. A memo without
/// its column fails every read and write.
pub(crate) fn has_rule_columns(db: &Connection) -> rusqlite::Result<bool> {
    Ok(file_hashes_have_rule(db)?
        && crate::cc_memo::has_rule_columns(db)?
        && input_predictions_have_current_payload(db)?
        && input_predictions_have_last_used(db)?)
}

pub fn ensure_file_hash_cache_schema(db: &Connection) -> rusqlite::Result<()> {
    crate::cc_memo::ensure_schema(db)?;
    crate::cc_memo::ensure_mapped_hash_schema(db)?;
    // An existing rowid table stays until GC housekeeping rebuilds it, off
    // the build path (`FileHashCache::rebuild_file_hashes_without_rowid`).
    db.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS file_hashes ({FILE_HASHES_COLUMNS}) WITHOUT ROWID;"
    ))?;
    ensure_input_predictions_payload(db)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_env_dep_uses (
            content_hash TEXT NOT NULL,
            env_var      TEXT NOT NULL,
            scanner      INTEGER NOT NULL,
            use_kind     INTEGER NOT NULL,
            updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (content_hash, env_var)
        );
        -- Boolean answers from the first env-use scanner, which missed
        -- computed names and other spellings. Nothing reads them any more.
        DROP TABLE IF EXISTS source_env_runtime_uses;",
    )?;
    for column in [
        "ALTER TABLE file_hashes ADD COLUMN ctime_ns INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE file_hashes ADD COLUMN inode INTEGER NOT NULL DEFAULT 0",
    ] {
        if let Err(e) = db.execute_batch(column)
            && !e.to_string().contains("duplicate column name")
        {
            return Err(e);
        }
    }
    ensure_file_hashes_rule(db)?;
    crate::cc_memo::ensure_rule(db)?;
    ensure_input_predictions_last_used(db, unix_now())
}

impl FileFingerprint {
    /// Identity of a file as the memo sees it. Also the cheapest available
    /// proof that an external tool did NOT rewrite a file across some
    /// operation when `inode != 0`: an in-place write bumps a change stamp
    /// and replacement changes the file ID. Windows uses NTFS handle evidence;
    /// unsupported filesystems or failed queries return weak metadata with
    /// `inode == 0`, which must not authorize a hash-memo hit or publication.
    pub fn from_path(path: &Path) -> Result<Self> {
        #[cfg(windows)]
        match windows_fingerprint(path) {
            Ok(fingerprint) => return Ok(fingerprint),
            Err(error) => tracing::debug!(
                path = %path.display(), %error,
                "strong Windows file identity unavailable; hash memo disabled for this file"
            ),
        }
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading metadata for {}", path.display()))?;
        Ok(Self::from_metadata(path, &metadata))
    }

    /// Identity from an existing stat, including a symlink's own metadata.
    /// Windows metadata has no change time or file ID: its zero inode cannot
    /// authorize a memo hit or publication. Use `from_path` for those proofs.
    pub fn from_metadata(path: &Path, metadata: &std::fs::Metadata) -> Self {
        let absolute_path = absolute_path(path);
        Self {
            path: absolute_path.to_string_lossy().into_owned(),
            size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            mtime_ns: metadata_mtime_ns(metadata),
            ctime_ns: metadata_ctime_ns(metadata),
            inode: metadata_inode(metadata),
        }
    }
}

/// NTFS's change time detects writes whose modification time is restored;
/// its file index detects replacement. Both come from one open handle. Other
/// filesystems stay fresh reads: ReFS needs a 128-bit ID which our memo schema
/// cannot represent, and FAT does not provide this change-time contract.
#[cfg(windows)]
fn windows_fingerprint(path: &Path) -> Result<FileFingerprint> {
    #[cfg(any(test, feature = "test-support"))]
    if WEAK_WINDOWS_FINGERPRINTS.with(std::cell::Cell::get) {
        anyhow::bail!("strong Windows file identity disabled by test fixture");
    }
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FileBasicInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let handle = file.as_raw_handle();
    let mut filesystem = [0_u16; 16];
    // SAFETY: the handle is live and the only non-null output points to the
    // writable filesystem-name buffer with its length in UTF-16 code units.
    if unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut identity = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the output is a correctly sized writable structure, and the
    // handle remains open through both queries.
    if unsafe { GetFileInformationByHandle(handle, &mut identity) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: FileBasicInfo selects FILE_BASIC_INFO and the passed size is
    // exactly the writable structure's size.
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    windows_fingerprint_from_parts(
        path,
        &filesystem,
        (identity.nFileIndexHigh, identity.nFileIndexLow),
        (identity.nFileSizeHigh, identity.nFileSizeLow),
        basic.LastWriteTime,
        basic.ChangeTime,
    )
}

/// Decode handle evidence without querying the OS. Index and size tuples are
/// (high, low) DWORDs. ReFS's wider IDs and filesystems without NTFS change
/// times cannot authorize the current hash memo's identity proof.
#[cfg(any(windows, test))]
fn windows_fingerprint_from_parts(
    path: &Path,
    filesystem: &[u16],
    file_index: (u32, u32),
    file_size: (u32, u32),
    last_write_time: i64,
    change_time: i64,
) -> Result<FileFingerprint> {
    if !filesystem.starts_with(&[b'N' as u16, b'T' as u16, b'F' as u16, b'S' as u16, 0]) {
        anyhow::bail!("filesystem does not provide a supported strong file identity");
    }
    let index = (u64::from(file_index.0) << 32) + u64::from(file_index.1);
    let changed = u64::try_from(change_time).context("invalid NTFS change time")?;
    if index == 0 || windows_filetime_ns(changed) == 0 {
        anyhow::bail!("filesystem returned an unavailable file index or change time");
    }
    Ok(FileFingerprint {
        path: absolute_path(path).to_string_lossy().into_owned(),
        size: i64::try_from((u64::from(file_size.0) << 32) + u64::from(file_size.1))?,
        mtime_ns: windows_filetime_ns(u64::try_from(last_write_time)?),
        ctime_ns: windows_filetime_ns(changed),
        // Preserve every file-ID bit; inode is an opaque equality token.
        inode: index as i64,
    })
}

#[cfg(all(windows, any(test, feature = "test-support")))]
thread_local! {
    static WEAK_WINDOWS_FINGERPRINTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Exercise filesystems or handle queries that cannot provide strong identity.
#[cfg(all(windows, any(test, feature = "test-support")))]
pub fn with_weak_windows_fingerprints_for_tests<T>(run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            WEAK_WINDOWS_FINGERPRINTS.with(|weak| weak.set(self.0));
        }
    }
    let _restore = Restore(WEAK_WINDOWS_FINGERPRINTS.with(|weak| weak.replace(true)));
    run()
}

/// The Unix second from which `file_hashes` rows are served: when this
/// machine booted. A row vouches for a file's bytes by its stamp (size,
/// mtime, ctime, inode), and a hard stop can break that: the stamp is
/// journaled, but data still in the page cache is lost, so a file written in
/// the seconds before the stop comes back at its size, its times and its
/// inode, as zeros or as holes. On 2026-11-07 copies a target seeding made
/// 30 s before such a stop came back zero-filled, and the memo, which
/// vouched for them with the hash of the bytes they never got, let every
/// later seeding copy them on into new targets (Cargo then trusted them and
/// rustc failed with E0786), and let a restore keep them. A row recorded
/// before the boot is therefore not served: the file is read once again and
/// the row recorded then is. A clean restart costs the same single read per
/// file. `0`, every row, where the boot time is unknown.
fn memo_rows_valid_since() -> i64 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(boot) = BOOT_SECS_FOR_TESTS.with(std::cell::Cell::get) {
        return boot;
    }
    static BOOT: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BOOT.get_or_init(|| boot_time_secs().unwrap_or(0))
}

/// When this machine booted, in Unix seconds.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn boot_time_secs() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    boot_time_in_proc_stat(&stat)
}

/// The `btime` line of `/proc/stat`.
#[cfg(any(test, target_os = "linux", target_os = "android"))]
fn boot_time_in_proc_stat(stat: &str) -> Option<i64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|secs| secs.trim().parse().ok())
}

/// When this machine booted, in Unix seconds.
#[cfg(windows)]
fn boot_time_secs() -> Option<i64> {
    // SAFETY: GetTickCount64 takes no arguments and only reads the tick count.
    let up_ms = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
    let up_secs = i64::try_from(up_ms / 1_000).ok()?;
    Some(windows_system_time_ns() / 1_000_000_000 - up_secs)
}

/// When this machine booted, in Unix seconds.
#[cfg(target_os = "macos")]
fn boot_time_secs() -> Option<i64> {
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is a NUL-terminated string, `boot` is a timeval of
    // `size` bytes the call fills, and no new value is passed.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut boot).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (read == 0 && boot.tv_sec > 0).then_some(boot.tv_sec)
}

/// Without a known boot time every row is served, as before.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    windows,
    target_os = "macos"
)))]
fn boot_time_secs() -> Option<i64> {
    None
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static BOOT_SECS_FOR_TESTS: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Run `run` as on a machine that booted at `boot_secs` (Unix seconds), for
/// tests of the memo across an unclean stop ([`memo_rows_valid_since`]).
#[cfg(any(test, feature = "test-support"))]
pub fn with_boot_time_for_tests<T>(boot_secs: i64, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<i64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOOT_SECS_FOR_TESTS.with(|boot| boot.set(self.0));
        }
    }
    let _restore = Restore(BOOT_SECS_FOR_TESTS.with(|boot| boot.replace(Some(boot_secs))));
    run()
}

pub fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Filesystem inode number (0 where unavailable, e.g. non-Unix).
pub fn metadata_inode(metadata: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        i64::try_from(metadata.ino()).unwrap_or(i64::MAX)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

pub fn metadata_mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        metadata_parts_ns(metadata.mtime(), metadata.mtime_nsec())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        windows_filetime_ns(metadata.last_write_time())
    }

    #[cfg(not(any(unix, windows)))]
    {
        system_time_ns(metadata.modified().ok()).unwrap_or_default()
    }
}

pub fn metadata_ctime_ns(metadata: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        metadata_parts_ns(metadata.ctime(), metadata.ctime_nsec())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        windows_filetime_ns(metadata.creation_time())
    }

    #[cfg(not(any(unix, windows)))]
    {
        system_time_ns(metadata.created().ok()).unwrap_or_else(|| metadata_mtime_ns(metadata))
    }
}

#[cfg(unix)]
fn metadata_parts_ns(seconds: i64, nanoseconds: i64) -> i64 {
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(nanoseconds)
}

#[cfg(any(windows, test))]
fn windows_filetime_ns(filetime_100ns: u64) -> i64 {
    const UNIX_EPOCH_FILETIME_100NS: u64 = 116_444_736_000_000_000;

    filetime_100ns
        .saturating_sub(UNIX_EPOCH_FILETIME_100NS)
        .saturating_mul(100)
        .min(i64::MAX as u64) as i64
}

#[cfg(any(test, not(any(unix, windows))))]
fn system_time_ns(time: Option<std::time::SystemTime>) -> Option<i64> {
    let duration = time?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX))
}

/// Hash a file using blake3.
pub fn hash_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(file)
        .with_context(|| format!("reading {} for hashing", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

impl FileHashCache<'_> {
    /// Cache lookup ONLY — reads the persistent hash cache, never computes a
    /// blake3. Lets a caller holding a coarse lock (the daemon's `Mutex<Store>`)
    /// release it before the expensive file read and re-take it only for the
    /// short record (#281). Mirrors `FileHasher::hash`'s fingerprint + min-size +
    /// cache-get logic exactly, so the cache key is identical.
    pub fn lookup_cached(&self, path: &Path) -> FileHashLookup {
        let cache = self;
        let fingerprint = match FileFingerprint::from_path(path) {
            Ok(fp) => fp,
            Err(e) => {
                tracing::debug!(
                    "file hash cache metadata lookup failed for {}: {e}",
                    path.display()
                );
                return FileHashLookup::Uncacheable;
            }
        };
        if fingerprint.size < MIN_PERSISTED_HASH_BYTES {
            return FileHashLookup::Uncacheable;
        }
        match cache.get(&fingerprint) {
            Ok(Some(hash)) => FileHashLookup::Hit(hash),
            Ok(None) => FileHashLookup::NeedsHash(fingerprint),
            Err(e) => {
                // Treat a lookup error as a miss — recompute rather than fail.
                tracing::debug!("file hash cache lookup failed for {}: {e}", path.display());
                FileHashLookup::NeedsHash(fingerprint)
            }
        }
    }
    /// Record a freshly-computed hash for `fingerprint` (the miss arm of
    /// [`Self::lookup_cached`]). Best-effort — a cache write failure is logged,
    /// not propagated.
    pub fn record_cached(&self, fingerprint: &FileFingerprint, hash: &str) {
        if let Err(e) = self.put(fingerprint, hash) {
            tracing::debug!("file hash cache update failed: {e}");
        }
    }
    /// Record a hash the caller already knows for `fingerprint` — without
    /// reading the file (kunobi-ninja/kache#540).
    ///
    /// The caller must have established that hash for THAT fingerprint, not
    /// merely for that path: the row is only ever served back on an exact
    /// fingerprint match, so a fingerprint captured at the moment the content
    /// was known stays a true statement even if the file changes a moment
    /// later — the changed file simply misses and gets hashed. Re-stating the
    /// path here instead would pair the new file's fingerprint with the old
    /// file's hash.
    ///
    /// Honors the same size floor as `FileHasher::hash`, which would not consult
    /// the memo for a smaller file anyway.
    pub fn record_verified(&self, fingerprint: &FileFingerprint, hash: &str) {
        if fingerprint.size < MIN_PERSISTED_HASH_BYTES {
            return;
        }
        self.record_cached(fingerprint, hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_hash_memo_policy_requires_identity_only_when_requested() {
        for (inode, requires_identity, eligible) in [
            (0, false, true),
            (17, false, true),
            (-17, false, true),
            (0, true, false),
            (17, true, true),
            (i64::MIN, true, true),
        ] {
            assert_eq!(file_hash_memo_eligible(inode, requires_identity), eligible);
        }
    }

    #[test]
    fn windows_fingerprint_parts_preserve_full_identity_size_and_times() {
        let path = Path::new(if cfg!(windows) {
            r"C:\kache\runtime.lib"
        } else {
            "/kache/runtime.lib"
        });
        let fingerprint = windows_fingerprint_from_parts(
            path,
            &[78, 84, 70, 83, 0],
            (0x8000_0001, 2),
            (1, 7),
            116_444_736_012_345_678,
            116_444_736_023_456_789,
        )
        .unwrap();
        assert_eq!(
            fingerprint,
            FileFingerprint {
                path: path.to_string_lossy().into_owned(),
                inode: -9_223_372_032_559_808_510,
                size: 4_294_967_303,
                mtime_ns: 1_234_567_800,
                ctime_ns: 2_345_678_900,
            }
        );
        for (index, expected) in [
            ((0, 1), 1),
            ((1, 0), 4_294_967_296),
            ((u32::MAX, u32::MAX), -1),
        ] {
            let fingerprint = windows_fingerprint_from_parts(
                path,
                &[78, 84, 70, 83, 0],
                index,
                (0x7fff_ffff, u32::MAX),
                116_444_736_000_000_000,
                116_444_736_000_000_001,
            )
            .unwrap();
            assert_eq!(fingerprint.inode, expected);
            assert_eq!(fingerprint.size, i64::MAX);
            assert_eq!(fingerprint.mtime_ns, 0);
            assert_eq!(fingerprint.ctime_ns, 100);
        }
        let empty = windows_fingerprint_from_parts(
            path,
            &[78, 84, 70, 83, 0],
            (0, 1),
            (0, 0),
            i64::MAX,
            i64::MAX,
        )
        .unwrap();
        assert_eq!(empty.size, 0);
        assert_eq!(empty.mtime_ns, i64::MAX);
        assert_eq!(empty.ctime_ns, i64::MAX);
    }

    #[test]
    fn windows_fingerprint_parts_reject_unsupported_or_invalid_evidence() {
        let path = Path::new(if cfg!(windows) {
            r"C:\kache\runtime.lib"
        } else {
            "/kache/runtime.lib"
        });
        for filesystem in [
            &[][..],
            &[82, 101, 70, 83, 0][..], // ReFS needs a 128-bit identity.
            &[70, 65, 84, 0][..],
            &[78, 84, 70, 83][..], // Not NUL-terminated.
            &[110, 116, 102, 115, 0][..],
        ] {
            assert!(
                windows_fingerprint_from_parts(
                    path,
                    filesystem,
                    (0, 1),
                    (0, 1),
                    116_444_736_000_000_001,
                    116_444_736_000_000_001,
                )
                .is_err(),
                "filesystem: {filesystem:?}"
            );
        }
        for (index, size, written, changed) in [
            (
                (0, 0),
                (0, 1),
                116_444_736_000_000_001,
                116_444_736_000_000_001,
            ),
            (
                (0, 1),
                (0x8000_0000, 0),
                116_444_736_000_000_001,
                116_444_736_000_000_001,
            ),
            ((0, 1), (0, 1), -1, 116_444_736_000_000_001),
            ((0, 1), (0, 1), 116_444_736_000_000_001, -1),
            ((0, 1), (0, 1), 116_444_736_000_000_001, 0),
            (
                (0, 1),
                (0, 1),
                116_444_736_000_000_001,
                116_444_735_999_999_999,
            ),
            (
                (0, 1),
                (0, 1),
                116_444_736_000_000_001,
                116_444_736_000_000_000,
            ),
        ] {
            assert!(
                windows_fingerprint_from_parts(
                    path,
                    &[78, 84, 70, 83, 0],
                    index,
                    size,
                    written,
                    changed,
                )
                .is_err(),
                "index={index:?}, size={size:?}, written={written}, changed={changed}"
            );
        }
    }

    /// The table as stores before #1206 created it.
    const ROWID_FILE_HASHES: &str = "CREATE TABLE file_hashes (
        path       TEXT PRIMARY KEY,
        size       INTEGER NOT NULL,
        mtime_ns   INTEGER NOT NULL,
        ctime_ns   INTEGER NOT NULL DEFAULT 0,
        inode      INTEGER NOT NULL DEFAULT 0,
        hash       TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    )";

    #[test]
    fn a_new_store_keeps_file_hashes_without_rowid() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        assert!(!file_hashes_has_rowid(&db).unwrap());
        let cache = FileHashCache::Borrowed(&db);
        assert!(!cache.rebuild_file_hashes_without_rowid().unwrap());
    }

    #[test]
    fn an_older_file_hashes_table_is_rebuilt_once_with_its_rows() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(ROWID_FILE_HASHES).unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        assert!(
            file_hashes_has_rowid(&db).unwrap(),
            "opening does not rebuild"
        );
        db.execute(
            "INSERT INTO file_hashes (path, size, mtime_ns, ctime_ns, inode, hash, updated_at)
             VALUES ('/a', 1, 2, 3, 4, 'h', '2026-09-01 00:00:00')",
            [],
        )
        .unwrap();

        let cache = FileHashCache::Borrowed(&db);
        assert!(cache.rebuild_file_hashes_without_rowid().unwrap());
        assert!(!file_hashes_has_rowid(&db).unwrap());
        let row: (String, i64, i64, i64, i64, String, String) = db
            .query_row(
                "SELECT path, size, mtime_ns, ctime_ns, inode, hash, updated_at FROM file_hashes",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "/a".into(),
                1,
                2,
                3,
                4,
                "h".into(),
                "2026-09-01 00:00:00".into()
            )
        );
        let indexed: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                 WHERE type = 'index' AND name = 'file_hashes_updated_at')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(indexed, "the prune's index comes back");
        assert!(!cache.rebuild_file_hashes_without_rowid().unwrap(), "once");
    }

    #[test]
    fn the_file_hash_window_is_thirty_days() {
        assert_eq!(FILE_HASH_RETENTION_SECS, 2_592_000);
    }

    #[test]
    fn file_hash_rows_are_pruned_a_month_after_they_were_written() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        // November 2023: the rows are years old by the real clock.
        let now = 1_700_000_000_i64;
        for (path, age) in [
            ("/kept", FILE_HASH_RETENTION_SECS - 60),
            ("/boundary", FILE_HASH_RETENTION_SECS),
            ("/old", FILE_HASH_RETENTION_SECS + 60),
        ] {
            db.execute(
                "INSERT INTO file_hashes (path, size, mtime_ns, hash, updated_at)
                 VALUES (?1, 1, 1, 'h', datetime(?2, 'unixepoch'))",
                rusqlite::params![path, now - age],
            )
            .unwrap();
        }
        assert_eq!(cache.prune_file_hashes_at(now).unwrap(), 1);
        let mut left: Vec<String> = db
            .prepare("SELECT path FROM file_hashes ORDER BY path")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["/boundary", "/kept"],
            "a row exactly a month old stays"
        );
        // A write refreshes the row: `put` stamps it now.
        assert_eq!(
            cache.prune_file_hashes().unwrap(),
            2,
            "real clock: both are years old"
        );
    }

    fn stamp(path: &str, mtime_ns: i64, ctime_ns: i64) -> FileFingerprint {
        FileFingerprint {
            path: path.to_string(),
            size: 10,
            mtime_ns,
            ctime_ns,
            inode: 7,
        }
    }

    #[test]
    fn a_redated_row_is_served_for_its_own_stamp_only() {
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let mut recorded = stamp("/deps/libdemo.rmeta", 5, 10);
        recorded.size = MIN_PERSISTED_HASH_BYTES;
        cache.put_redated(&recorded, "h").unwrap();
        assert_eq!(cache.get(&recorded).unwrap().as_deref(), Some("h"));
        let mut redated = recorded.clone();
        redated.ctime_ns += 1;
        assert_eq!(
            cache.get(&redated).unwrap(),
            None,
            "Cargo's re-date is never served without a read"
        );
    }

    #[test]
    fn a_redated_row_lets_a_read_of_cargos_re_date_be_memoised() {
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let mut recorded = stamp("/deps/libdemo.rmeta", 5, 10);
        recorded.size = MIN_PERSISTED_HASH_BYTES;
        cache.put_redated(&recorded, "h").unwrap();
        let later = |ctime_ns| {
            let mut fingerprint = recorded.clone();
            fingerprint.ctime_ns = ctime_ns;
            fingerprint
        };
        assert!(cache.redated_pin(&later(11), "h"));
        assert!(cache.redated_pin(&later(10 + REDATE_CTIME_WINDOW_NS), "h"));
        assert!(!cache.redated_pin(&later(11 + REDATE_CTIME_WINDOW_NS), "h"), "window");
        assert!(!cache.redated_pin(&later(9), "h"), "an earlier ctime");
        assert!(!cache.redated_pin(&later(11), "other"), "other bytes");
        for change in [
            |f: &mut FileFingerprint| f.mtime_ns += 1,
            |f: &mut FileFingerprint| f.size += 1,
            |f: &mut FileFingerprint| f.inode += 1,
            |f: &mut FileFingerprint| f.path.push('x'),
        ] {
            let mut other = later(11);
            change(&mut other);
            assert!(!cache.redated_pin(&other, "h"), "{other:?}");
        }
        let ordinary = stamp("/deps/libplain.rmeta", 5, 10);
        cache.put(&ordinary, "h").unwrap();
        let mut ordinary_later = ordinary.clone();
        ordinary_later.ctime_ns = 11;
        assert!(
            !cache.redated_pin(&ordinary_later, "h"),
            "an ordinary row grants nothing"
        );
    }

    #[test]
    fn a_small_redated_row_is_not_recorded() {
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let small = stamp("/deps/libsmall.rmeta", 5, 10);
        cache.put_redated(&small, "h").unwrap();
        assert_eq!(cache.get(&small).unwrap(), None);
    }

    #[test]
    fn a_stamp_settles_exactly_one_window_after_its_last_change() {
        let changed = 1_000_000_000_000;
        let file = stamp("/h.h", changed, changed);
        assert!(!stamp_is_settled(&file, changed));
        assert!(!stamp_is_settled(&file, changed + HASH_SETTLE_NS - 1));
        assert!(stamp_is_settled(&file, changed + HASH_SETTLE_NS));
        // A clock behind the file (network filesystems) is not settled.
        assert!(!stamp_is_settled(&file, changed - 1));
    }

    #[test]
    fn a_write_counts_from_one_stamp_window_before_the_start() {
        let fine = if cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            windows
        )) {
            1_000_000
        } else {
            20_000_000
        };
        assert_eq!(FINE_STAMP_WINDOW_NS, fine);
        let both = |ns| stamp("/s.rs", ns, ns);
        let whole = 1_700_000_000_000_000_000;
        let whole_window = 2_000_000_000 + fine;
        assert!(stamp_written_since(&both(whole), whole + whole_window));
        assert!(!stamp_written_since(&both(whole), whole + whole_window + 1));
        let centi = whole + 120_000_000;
        let centi_window = 10_000_000 + fine;
        assert!(stamp_written_since(&both(centi), centi + centi_window));
        assert!(!stamp_written_since(&both(centi), centi + centi_window + 1));
        let precise = whole + 123_456_789;
        assert!(stamp_written_since(&both(precise), precise + fine));
        assert!(!stamp_written_since(&both(precise), precise + fine + 1));
        // Each time keeps its own window: a two-second FAT write time beside
        // an old 10 ms creation time, and an old whole-second mtime restored
        // over a fresh ctime.
        let long_ago = whole - 60_000_000_000;
        let fat = stamp("/s.rs", whole, long_ago + 30_000_000);
        assert!(stamp_written_since(&fat, whole + whole_window));
        assert!(!stamp_written_since(&fat, whole + whole_window + 1));
        let restored = stamp("/s.rs", long_ago, precise);
        assert!(stamp_written_since(&restored, precise + fine));
        assert!(!stamp_written_since(&restored, precise + fine + 1));
    }

    #[test]
    fn a_filetime_counts_from_the_unix_epoch_in_nanoseconds() {
        let split = |intervals: u64| ((intervals >> 32) as u32, intervals as u32);
        let epoch = 116_444_736_000_000_000_u64;
        let at = |intervals: u64| {
            let (high, low) = split(intervals);
            filetime_to_unix_ns(high, low)
        };
        assert_eq!(at(epoch), 0);
        assert_eq!(at(epoch + 1), 100);
        assert_eq!(at(epoch - 1), 0);
        // 2026-10-10T00:00:00Z: both halves carry bits.
        assert_eq!(
            at(epoch + 1_791_590_400 * 10_000_000),
            1_791_590_400_000_000_000
        );
        assert_eq!(filetime_to_unix_ns(u32::MAX, u32::MAX), i64::MAX);
    }

    /// The start a build reads must not run ahead of the stamp a file written
    /// after it gets: with the precise clock, NTFS stamps trailed it by up to
    /// a timer tick, past the 1 ms window.
    #[cfg(windows)]
    #[test]
    fn a_file_written_after_the_stamp_clock_counts_as_written_since() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("written.rs");
        for round in 0..200 {
            let start = stamp_clock_ns();
            std::fs::write(&path, round.to_string()).unwrap();
            let written = FileFingerprint::from_path(&path).unwrap();
            assert!(
                stamp_written_since(&written, start),
                "round {round}: {written:?} before {start}"
            );
        }
    }

    /// The window covers how far this platform's stamps trail
    /// [`stamp_clock_ns`]: a write made right after reading it counts as
    /// written since then.
    #[test]
    fn a_write_right_after_the_stamp_clock_counts_as_written_since() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        for round in 0..500 {
            let since = stamp_clock_ns();
            std::fs::write(&file, round.to_string()).unwrap();
            let written = FileFingerprint::from_path(&file).unwrap();
            assert!(
                stamp_written_since(&written, since),
                "round {round}: {written:?} since {since}"
            );
        }
    }

    /// Once the stamp clock has moved a window past a write, the write no
    /// longer counts as one made since.
    #[test]
    fn a_write_stops_counting_once_the_stamp_clock_passes_its_window() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let written = FileFingerprint::from_path(&file).unwrap();
        // The widest window, for a whole-second stamp, is about two seconds.
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while stamp_written_since(&written, stamp_clock_ns()) {
            assert!(
                std::time::Instant::now() < give_up,
                "the stamp clock never passed {written:?}: {}",
                stamp_clock_ns()
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_recent_ctime_holds_back_a_file_whose_mtime_was_restored() {
        // `touch -r`, `cp -p` and rsync write new bytes, then put an old mtime
        // back. Only the ctime shows the write.
        let now = 1_000_000_000_000;
        let old = now - 10 * HASH_SETTLE_NS;
        assert!(!stamp_is_settled(&stamp("/h.h", old, now - 1), now));
        assert!(stamp_is_settled(&stamp("/h.h", old, old), now));
    }

    /// An observation carries the wall clock read while it was taken, in
    /// the nanoseconds since the epoch that stamps are kept in.
    #[test]
    fn an_observation_carries_the_wall_clock_it_was_taken_at() {
        let since_epoch = || {
            let elapsed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            i64::try_from(elapsed.as_nanos()).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let before = since_epoch();
        let observed = ObservedFingerprint::from_path(&file).unwrap();
        let after = since_epoch();
        assert!(
            (before..=after).contains(&observed.observed_ns),
            "{observed:?} outside {before}..={after}"
        );
        assert_eq!(
            observed.fingerprint,
            FileFingerprint::from_path(&file).unwrap()
        );
    }

    /// Settled when observed, not when asked: a stamp observed a moment
    /// before its window closed stays unsettled however long ago that was.
    #[test]
    fn an_observation_is_settled_by_the_time_it_was_taken() {
        let changed = 1_000_000_000_000;
        let observed_at = |observed_ns| ObservedFingerprint {
            fingerprint: stamp("/h.h", changed, changed),
            observed_ns,
        };
        assert!(!observed_at(changed + HASH_SETTLE_NS - 1).settled());
        assert!(observed_at(changed + HASH_SETTLE_NS).settled());
    }

    #[test]
    fn many_lookups_return_only_rows_whose_whole_stamp_matches() {
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let same = stamp("/same.h", 1, 1);
        let moved = stamp("/moved.h", 1, 1);
        cache.put(&same, "h-same").unwrap();
        cache.put(&moved, "h-moved").unwrap();
        let mut rewritten = moved.clone();
        rewritten.mtime_ns = 2;
        let absent = stamp("/absent.h", 1, 1);

        let found = cache.get_many(&[&same, &rewritten, &absent]).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found["/same.h"], "h-same");
        assert!(
            !found.contains_key("/moved.h"),
            "a changed stamp reads as absent"
        );
    }

    /// How 1.0.0 records a row: it names no `rule`.
    fn record_as_older_release(db: &Connection, fingerprint: &FileFingerprint, hash: &str) {
        db.execute(
            "INSERT OR REPLACE INTO file_hashes
             (path, size, mtime_ns, ctime_ns, inode, hash, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            params![
                fingerprint.path,
                fingerprint.size,
                fingerprint.mtime_ns,
                fingerprint.ctime_ns,
                fingerprint.inode,
                hash
            ],
        )
        .unwrap();
    }

    #[test]
    fn a_row_an_older_release_records_is_never_served() {
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let current = stamp("/current.h", 1, 1);
        let older = stamp("/older.h", 1, 1);
        cache.put(&current, "h-current").unwrap();
        record_as_older_release(&db, &older, "h-older");

        assert_eq!(cache.get(&current).unwrap().as_deref(), Some("h-current"));
        assert_eq!(cache.get(&older).unwrap(), None);
        let found = cache.get_many(&[&current, &older]).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found["/current.h"], "h-current");

        // Rewritten by the older release, the current row is gone too.
        record_as_older_release(&db, &current, "h-older");
        assert_eq!(cache.get(&current).unwrap(), None);
    }

    #[test]
    fn the_rule_column_arrives_once_and_takes_the_older_rows_with_it() {
        for table in [
            ROWID_FILE_HASHES.to_string(),
            format!("{ROWID_FILE_HASHES} WITHOUT ROWID"),
        ] {
            let db = Connection::open_in_memory().unwrap();
            db.execute_batch(&table).unwrap();
            record_as_older_release(&db, &stamp("/older.h", 1, 1), "h-older");
            ensure_file_hash_cache_schema(&db).unwrap();
            assert!(file_hashes_have_rule(&db).unwrap(), "{table}");
            let rows = || -> i64 {
                db.query_row("SELECT count(*) FROM file_hashes", [], |row| row.get(0))
                    .unwrap()
            };
            assert_eq!(rows(), 0, "{table}");

            let cache = FileHashCache::Borrowed(&db);
            cache.put(&stamp("/current.h", 1, 1), "h-current").unwrap();
            ensure_file_hash_cache_schema(&db).unwrap();
            assert_eq!(rows(), 1, "{table}: once");
        }
    }

    #[test]
    fn rebuilding_without_rowid_keeps_each_rows_rule() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(ROWID_FILE_HASHES).unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let current = stamp("/current.h", 1, 1);
        let older = stamp("/older.h", 1, 1);
        cache.put(&current, "h-current").unwrap();
        record_as_older_release(&db, &older, "h-older");

        assert!(cache.rebuild_file_hashes_without_rowid().unwrap());
        assert_eq!(cache.get(&current).unwrap().as_deref(), Some("h-current"));
        assert_eq!(cache.get(&older).unwrap(), None);
    }

    #[test]
    fn many_lookups_span_more_paths_than_one_statement_binds() {
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        let cache = FileHashCache::Borrowed(&db);
        let stamps: Vec<FileFingerprint> = (0..FILE_HASH_LOOKUP_CHUNK * 2 + 3)
            .map(|i| stamp(&format!("/h{i}.h"), 1, 1))
            .collect();
        for (i, s) in stamps.iter().enumerate() {
            cache.put(s, &format!("hash{i}")).unwrap();
        }
        let refs: Vec<&FileFingerprint> = stamps.iter().collect();
        let found = cache.get_many(&refs).unwrap();
        assert_eq!(found.len(), stamps.len());
        assert_eq!(found["/h0.h"], "hash0");
        let last = stamps.len() - 1;
        assert_eq!(found[&format!("/h{last}.h")], format!("hash{last}"));
    }

    #[test]
    fn has_entry_for_unit_sees_only_that_crate_and_unit() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE entries (cache_key TEXT PRIMARY KEY, crate_name TEXT NOT NULL,
                                   unit_id TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        let cache = FileHashCache::Borrowed(&db);
        assert!(!cache.has_entry_for_unit("gpui", "").unwrap());
        assert!(!cache.has_entry_for_unit("gpui", "u1").unwrap());
        db.execute(
            "INSERT INTO entries (cache_key, crate_name, unit_id) VALUES ('k', 'gpui_base', 'u1')",
            [],
        )
        .unwrap();
        assert!(!cache.has_entry_for_unit("gpui", "u1").unwrap());
        assert!(cache.has_entry_for_unit("gpui_base", "u1").unwrap());
        assert!(
            cache.has_entry_for_unit("gpui_base", "").unwrap(),
            "any unit"
        );
        assert!(
            !cache.has_entry_for_unit("gpui_base", "u2").unwrap(),
            "another unit of the same crate name is absent"
        );
        db.execute(
            "INSERT INTO entries (cache_key, crate_name) VALUES ('k2', 'build_script_build')",
            [],
        )
        .unwrap();
        assert!(
            cache
                .has_entry_for_unit("build_script_build", "u9")
                .unwrap(),
            "a row with no unit stands for every unit of its name"
        );
    }

    #[test]
    fn fingerprint_preserves_the_filesystem_identity() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("artifact.rlib");
        std::fs::write(&file, vec![7; 65_537]).unwrap();
        #[cfg(any(unix, windows))]
        let metadata = std::fs::metadata(&file).unwrap();
        let fingerprint = FileFingerprint::from_path(&file).unwrap();
        assert_eq!(fingerprint.path, file.to_string_lossy());
        assert_eq!(fingerprint.size, 65_537);

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fingerprint.inode, metadata.ino() as i64);
            assert_eq!(
                fingerprint.mtime_ns,
                metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec()
            );
            assert_eq!(
                fingerprint.ctime_ns,
                metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec()
            );
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            assert_ne!(fingerprint.inode, 0);
            assert_eq!(
                fingerprint.mtime_ns,
                ((metadata.last_write_time() - 116_444_736_000_000_000) * 100) as i64
            );
            assert!(fingerprint.ctime_ns > 0);
            let weak = FileFingerprint::from_metadata(&file, &metadata);
            assert_eq!(weak.inode, 0);
            assert_ne!(fingerprint, weak);
        }
        assert_eq!(
            absolute_path(Path::new("relative.rlib")),
            std::env::current_dir().unwrap().join("relative.rlib")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_fingerprint_detects_restored_mtime_edits_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.lib");
        std::fs::write(&path, vec![b'a'; 70_000]).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&metadata);
        let first = FileFingerprint::from_path(&path).unwrap();
        assert_ne!(first.inode, 0, "native CI must provide NTFS evidence");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while stamp_clock_ns() <= first.ctime_ns.saturating_add(20_000_000) {
            assert!(std::time::Instant::now() < deadline, "stamp clock stalled");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::fs::write(&path, vec![b'b'; 70_000]).unwrap();
        filetime::set_file_mtime(&path, mtime).unwrap();
        let edited = FileFingerprint::from_path(&path).unwrap();
        assert_eq!(edited.size, first.size);
        assert_eq!(edited.mtime_ns, first.mtime_ns);
        assert_eq!(edited.inode, first.inode);
        assert_ne!(edited.ctime_ns, first.ctime_ns);
        assert_eq!(
            FileFingerprint::from_metadata(&path, &std::fs::metadata(&path).unwrap()),
            FileFingerprint::from_metadata(&path, &metadata),
            "creation time alone hides this edit"
        );

        let replacement = dir.path().join("replacement.lib");
        std::fs::write(&replacement, vec![b'c'; 70_000]).unwrap();
        filetime::set_file_mtime(&replacement, mtime).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let replaced = FileFingerprint::from_path(&path).unwrap();
        assert_eq!(replaced.size, first.size);
        assert_eq!(replaced.mtime_ns, first.mtime_ns);
        assert_ne!(replaced.inode, edited.inode);
    }

    #[cfg(windows)]
    #[test]
    fn weak_windows_fingerprints_never_reuse_or_publish_hash_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.lib");
        std::fs::write(&path, vec![b'a'; 70_000]).unwrap();
        let weak =
            with_weak_windows_fingerprints_for_tests(|| FileFingerprint::from_path(&path).unwrap());
        assert_eq!(weak.inode, 0);
        assert_eq!(weak.size, 70_000);
        let db = Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        db.execute(
            "INSERT INTO file_hashes (path, size, mtime_ns, ctime_ns, inode, hash, rule)
             VALUES (?1, ?2, ?3, ?4, 0, 'legacy', ?5)",
            params![
                weak.path,
                weak.size,
                weak.mtime_ns,
                weak.ctime_ns,
                FILE_HASH_RULE
            ],
        )
        .unwrap();
        let cache = FileHashCache::Borrowed(&db);
        assert_eq!(cache.get(&weak).unwrap(), None);
        assert!(cache.get_many(&[&weak]).unwrap().is_empty());
        cache.put(&weak, "replacement").unwrap();
        assert_eq!(
            db.query_row("SELECT hash FROM file_hashes", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "legacy"
        );
        db.execute("DELETE FROM file_hashes", []).unwrap();
        cache.record_verified(&weak, "new");
        assert_eq!(
            db.query_row("SELECT count(*) FROM file_hashes", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            hash_file(&path).unwrap(),
            blake3::hash(&vec![b'a'; 70_000]).to_hex().to_string()
        );
    }

    #[test]
    fn windows_filetime_converts_epoch_units_and_saturates() {
        assert_eq!(windows_filetime_ns(0), 0);
        assert_eq!(windows_filetime_ns(116_444_735_999_999_999), 0);
        assert_eq!(windows_filetime_ns(116_444_736_000_000_000), 0);
        assert_eq!(windows_filetime_ns(116_444_736_000_000_001), 100);
        assert_eq!(windows_filetime_ns(116_444_736_012_345_678), 1_234_567_800);
        assert_eq!(windows_filetime_ns(u64::MAX), i64::MAX);
    }

    #[test]
    fn system_time_conversion_rejects_missing_or_pre_epoch_values() {
        use std::time::{Duration, UNIX_EPOCH};

        // Windows represents SystemTime in 100 ns ticks.
        assert_eq!(system_time_ns(None), None);
        assert_eq!(
            system_time_ns(Some(UNIX_EPOCH - Duration::from_nanos(100))),
            None
        );
        assert_eq!(system_time_ns(Some(UNIX_EPOCH)), Some(0));
        assert_eq!(
            system_time_ns(Some(UNIX_EPOCH + Duration::new(1, 234_567_800))),
            Some(1_234_567_800)
        );
        assert_eq!(
            system_time_ns(Some(UNIX_EPOCH + Duration::from_secs(9_223_372_037))),
            Some(i64::MAX)
        );
    }

    #[test]
    fn env_dep_use_memo_persists_each_answer_per_content_variable_and_scanner() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let cache = FileHashCache::open(&db_path).unwrap();
        assert_eq!(
            cache
                .get_source_env_dep_use("source-a", "OUT_DIR", 2)
                .unwrap(),
            None
        );
        cache
            .put_source_env_dep_use("source-a", "OUT_DIR", 2, 1)
            .unwrap();
        cache
            .put_source_env_dep_use("source-a", "OTHER", 2, 0)
            .unwrap();
        cache
            .put_source_env_dep_use("source-b", "OUT_DIR", 2, 2)
            .unwrap();
        drop(cache);

        let cache = FileHashCache::open(&db_path).unwrap();
        let get = |content: &str, var: &str, scanner: u32| {
            cache.get_source_env_dep_use(content, var, scanner).unwrap()
        };
        assert_eq!(get("source-a", "OUT_DIR", 2), Some(1));
        assert_eq!(get("source-a", "OTHER", 2), Some(0));
        assert_eq!(get("source-b", "OUT_DIR", 2), Some(2));
        assert_eq!(get("source-b", "OTHER", 2), None);
        // Another scanner version never sees these answers.
        assert_eq!(get("source-a", "OUT_DIR", 1), None);
        assert_eq!(get("source-a", "OUT_DIR", 3), None);

        // A newer scanner's answer replaces the row for the same content.
        cache
            .put_source_env_dep_use("source-a", "OUT_DIR", 3, 2)
            .unwrap();
        assert_eq!(get("source-a", "OUT_DIR", 3), Some(2));
        assert_eq!(get("source-a", "OUT_DIR", 2), None);
    }

    #[test]
    fn schema_drops_the_boolean_env_use_memo() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE source_env_runtime_uses (
                content_hash    TEXT NOT NULL,
                env_var         TEXT NOT NULL,
                has_runtime_use INTEGER NOT NULL,
                updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (content_hash, env_var)
            );
            INSERT INTO source_env_runtime_uses (content_hash, env_var, has_runtime_use)
            VALUES ('source-a', 'OUT_DIR', 0);",
        )
        .unwrap();
        ensure_file_hash_cache_schema(&conn).unwrap();
        let tables = |name: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![name],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(tables("source_env_runtime_uses"), 0);
        assert_eq!(tables("source_env_dep_uses"), 1);
    }

    #[test]
    fn input_prediction_preserves_the_callers_schema_and_opaque_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let cache = FileHashCache::open(&db_path).unwrap();
        assert_eq!(cache.get_input_prediction("first").unwrap(), None);
        cache
            .put_input_prediction("first", 17, Some("example"), "payload-a")
            .unwrap();
        cache
            .put_input_prediction("second", 42, None, "payload-b")
            .unwrap();
        drop(cache);

        let cache = FileHashCache::open(&db_path).unwrap();
        assert_eq!(
            cache.get_input_prediction("first").unwrap(),
            Some((17, "payload-a".into()))
        );
        assert_eq!(cache.get_input_prediction("missing").unwrap(), None);
        cache
            .put_input_prediction("first", 18, None, "payload-c")
            .unwrap();
        assert_eq!(
            cache.get_input_prediction("first").unwrap(),
            Some((18, "payload-c".into()))
        );
        assert_eq!(
            cache.get_input_prediction("second").unwrap(),
            Some((42, "payload-b".into()))
        );
    }

    fn prediction_last_used(cache: &FileHashCache<'_>, identity: &str) -> i64 {
        cache
            .db()
            .query_row(
                "SELECT last_used FROM input_predictions WHERE identity = ?1",
                params![identity],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn set_prediction_last_used(cache: &FileHashCache<'_>, identity: &str, last_used: i64) {
        cache
            .db()
            .execute(
                "UPDATE input_predictions SET last_used = ?2 WHERE identity = ?1",
                params![identity, last_used],
            )
            .unwrap();
    }

    /// The table as every release before `last_used` created it.
    fn create_legacy_input_predictions(db: &Connection) {
        db.execute_batch(
            "CREATE TABLE input_predictions (
                identity        TEXT PRIMARY KEY,
                schema          INTEGER NOT NULL,
                crate_name      TEXT,
                prediction_json TEXT NOT NULL,
                updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO input_predictions (identity, schema, crate_name, prediction_json, updated_at)
            VALUES ('old-a', 3, 'alpha', 'payload-a', '2020-01-01 00:00:00'),
                   ('old-b', 4, NULL, 'payload-b', '2020-01-02 00:00:00');",
        )
        .unwrap();
    }

    #[test]
    fn input_prediction_windows_are_pinned() {
        assert_eq!(INPUT_PREDICTION_TOUCH_INTERVAL_SECS, 86_400);
        assert_eq!(INPUT_PREDICTION_RETENTION_SECS, 2_592_000);
        assert!(!prediction_touch_due(1_000_000, 1_000_000 + 86_399));
        assert!(prediction_touch_due(1_000_000, 1_000_000 + 86_400));
        // A stamp ahead of the clock is never due, and never overflows.
        assert!(!prediction_touch_due(2_000_000, 1_000_000));
        assert!(!prediction_touch_due(i64::MAX, i64::MIN));
        assert_eq!(prediction_prune_cutoff(3_000_000), 408_000);
        assert_eq!(prediction_prune_cutoff(i64::MIN), i64::MIN);
    }

    #[test]
    fn input_prediction_hit_refreshes_last_used_at_most_once_per_interval() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        cache
            .put_input_prediction("unit", 1, None, "payload")
            .unwrap();
        let now = 1_900_000_000;
        set_prediction_last_used(&cache, "unit", now - 86_399);
        assert_eq!(
            cache.get_input_prediction_at("unit", now).unwrap(),
            Some((1, "payload".into()))
        );
        assert_eq!(prediction_last_used(&cache, "unit"), now - 86_399);

        set_prediction_last_used(&cache, "unit", now - 86_400);
        assert_eq!(
            cache.get_input_prediction_at("unit", now).unwrap(),
            Some((1, "payload".into()))
        );
        assert_eq!(prediction_last_used(&cache, "unit"), now);

        // A second hit inside the interval writes nothing.
        cache.get_input_prediction_at("unit", now + 5).unwrap();
        assert_eq!(prediction_last_used(&cache, "unit"), now);
        // A miss touches nothing and other rows are left alone.
        cache
            .put_input_prediction("other", 1, None, "payload")
            .unwrap();
        set_prediction_last_used(&cache, "other", 7);
        assert_eq!(cache.get_input_prediction_at("absent", now).unwrap(), None);
        cache.get_input_prediction_at("unit", now + 86_400).unwrap();
        assert_eq!(prediction_last_used(&cache, "other"), 7);
    }

    #[test]
    fn input_prediction_read_and_record_stamp_the_wall_clock() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        // SQLite writes use Windows' coarse clock; Rust reads use its precise
        // clock. Compare each stamp with a sample from the clock that wrote it.
        let before_sqlite: i64 = cache
            .db()
            .query_row("SELECT unixepoch()", [], |row| row.get(0))
            .unwrap();
        assert!(before_sqlite > 1_700_000_000);
        cache
            .put_input_prediction("unit", 1, None, "payload")
            .unwrap();
        assert!(prediction_last_used(&cache, "unit") >= before_sqlite);

        let before = unix_now();
        set_prediction_last_used(&cache, "unit", 1);
        cache.get_input_prediction("unit").unwrap();
        assert!(prediction_last_used(&cache, "unit") >= before);

        // Recording again replaces the row and restamps it.
        set_prediction_last_used(&cache, "unit", 1);
        cache
            .put_input_prediction("unit", 2, None, "payload-2")
            .unwrap();
        assert!(prediction_last_used(&cache, "unit") >= before_sqlite);
    }

    #[test]
    fn prune_input_predictions_removes_only_rows_unused_past_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        // Empty table, as on every store with predictions off.
        assert_eq!(cache.prune_input_predictions().unwrap(), 0);

        let now = 1_900_000_000;
        for identity in ["expired", "boundary", "fresh"] {
            cache
                .put_input_prediction(identity, 1, None, "payload")
                .unwrap();
        }
        set_prediction_last_used(&cache, "expired", now - 2_592_001);
        set_prediction_last_used(&cache, "boundary", now - 2_592_000);
        set_prediction_last_used(&cache, "fresh", now);
        assert_eq!(cache.prune_input_predictions_at(now).unwrap(), 1);
        assert_eq!(cache.get_input_prediction_at("expired", now).unwrap(), None);
        assert!(
            cache
                .get_input_prediction_at("boundary", now - 86_400)
                .unwrap()
                .is_some()
        );
        assert!(
            cache
                .get_input_prediction_at("fresh", now)
                .unwrap()
                .is_some()
        );
        assert_eq!(cache.prune_input_predictions_at(now).unwrap(), 0);

        let indexed: bool = cache
            .db()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index'
                 AND name = 'input_predictions_last_used')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(indexed);
    }

    #[test]
    fn the_bounded_prediction_prune_goes_in_batches_and_stops_at_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        let now = 1_900_000_000;
        for n in 0..5 {
            let identity = format!("expired-{n}");
            cache
                .put_input_prediction(&identity, 2, None, "payload")
                .unwrap();
            set_prediction_last_used(&cache, &identity, now - 8 * 86_400);
        }
        cache
            .put_input_prediction("recent", 2, None, "payload")
            .unwrap();
        set_prediction_last_used(&cache, "recent", now - 6 * 86_400);
        let week = now - 7 * 86_400;
        let none = cache
            .prune_input_predictions_batched(week, week, Duration::ZERO, 2)
            .unwrap();
        assert_eq!((none.rows, none.batches, none.cut_short), (0, 0, true));
        let all = cache
            .prune_input_predictions_batched(week, week, Duration::from_secs(60), 2)
            .unwrap();
        assert_eq!((all.rows, all.batches, all.cut_short), (5, 3, false));
        let left: Vec<String> = cache
            .db()
            .prepare("SELECT identity FROM input_predictions")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(left, ["recent"]);
    }

    #[test]
    fn a_shorter_window_leaves_build_script_rows_their_thirty_days() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        let now = 1_900_000_000;
        for (identity, idle_days) in [
            ("rustc-unit", 8),
            ("build-script:abc", 8),
            ("unsettled:build-script:abc", 8),
            ("build-script:old", 31),
        ] {
            cache
                .put_input_prediction(identity, 2, None, "payload")
                .unwrap();
            set_prediction_last_used(&cache, identity, now - idle_days * 86_400);
        }
        let done = cache
            .prune_input_predictions_within(now, 7 * 86_400, Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            done.rows, 2,
            "the rustc row and the script row past 30 days"
        );
        let mut left: Vec<String> = cache
            .db()
            .prepare("SELECT identity FROM input_predictions")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        left.sort();
        assert_eq!(left, ["build-script:abc", "unsettled:build-script:abc"]);
    }

    #[test]
    fn a_writer_holding_the_index_cuts_the_prediction_prune_short_without_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let cache = FileHashCache::open(&path).unwrap();
        cache
            .put_input_prediction("old", 2, None, "payload")
            .unwrap();
        set_prediction_last_used(&cache, "old", 1);
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        cache.db().busy_timeout(Duration::ZERO).unwrap();
        let done = cache
            .prune_input_predictions_within(1_900_000_000, 86_400, Duration::from_secs(60))
            .unwrap();
        assert_eq!((done.rows, done.cut_short), (0, true));
        writer.execute_batch("ROLLBACK").unwrap();
        assert!(cache.get_input_prediction_at("old", 2).unwrap().is_some());
    }

    fn long_json(tag: usize) -> String {
        format!(
            "{{\"schema\":2,\"sources\":[{}]}}",
            (0..30)
                .map(|i| format!("\"/src/{tag}/module_{i}.rs\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    /// Each row's SQLite type, `last_used` and `updated_at`, by name.
    fn prediction_rows(cache: &FileHashCache<'_>) -> Vec<(String, String, i64, String)> {
        let db = cache.db();
        let mut statement = db
            .prepare(
                "SELECT identity, typeof(prediction_json), last_used, updated_at
                 FROM input_predictions ORDER BY identity",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn the_conversion_compresses_plain_rows_in_batches_and_keeps_their_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        for i in 0..250 {
            let identity = format!("unit-{i:03}");
            cache
                .put_input_prediction(&identity, 2, Some("c"), &long_json(i))
                .unwrap();
            set_prediction_last_used(&cache, &identity, 1_000 + i as i64);
        }
        cache
            .put_input_prediction("build-script:x", 2, None, &long_json(900))
            .unwrap();
        cache
            .put_input_prediction("unsettled:x", 2, None, &long_json(901))
            .unwrap();
        cache.put_input_prediction("tiny", 2, None, "{}").unwrap();
        let before = prediction_rows(&cache);

        let done = cache
            .compress_input_predictions_batched(Duration::MAX, 100)
            .unwrap();
        assert_eq!(
            (done.rows, done.kept_plain, done.batches, done.cut_short),
            (250, 1, 3, false),
            "the tiny row is the third batch's last"
        );
        let after = prediction_rows(&cache);
        assert_eq!(after.len(), before.len());
        for ((identity, kind, last_used, updated_at), old) in after.iter().zip(&before) {
            assert_eq!((identity, last_used, updated_at), (&old.0, &old.2, &old.3));
            let compressed = identity.starts_with("unit-");
            assert_eq!(kind, if compressed { "blob" } else { "text" }, "{identity}");
        }
        for i in [0, 99, 100, 249] {
            assert_eq!(
                cache
                    .get_input_prediction_at(&format!("unit-{i:03}"), 1_000 + i as i64)
                    .unwrap(),
                Some((2, long_json(i)))
            );
        }
        assert_eq!(
            cache.get_input_prediction("build-script:x").unwrap(),
            Some((2, long_json(900)))
        );

        let again = cache
            .compress_input_predictions_batched(Duration::MAX, 100)
            .unwrap();
        assert_eq!((again.rows, again.kept_plain, again.batches), (0, 1, 1));
    }

    #[test]
    fn the_conversion_stops_at_its_budget_and_goes_on_at_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        for i in 0..5 {
            cache
                .put_input_prediction(&format!("unit-{i}"), 2, None, &long_json(i))
                .unwrap();
        }
        let none = cache
            .compress_input_predictions_batched(Duration::ZERO, 2)
            .unwrap();
        assert_eq!((none.rows, none.batches, none.cut_short), (0, 0, true));
        // A batch of 2 holds the lock for well under the pause it then takes
        // (at least 5 ms), so a 1 ms budget lets exactly one batch through.
        let one = cache
            .compress_input_predictions_batched(Duration::from_millis(1), 2)
            .unwrap();
        assert_eq!((one.rows, one.batches, one.cut_short), (2, 1, true));
        let rest = cache
            .compress_input_predictions_within(Duration::from_secs(60))
            .unwrap();
        assert_eq!((rest.rows, rest.batches, rest.cut_short), (3, 1, false));
    }

    #[test]
    fn a_writer_holding_the_index_cuts_the_conversion_short_without_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let cache = FileHashCache::open(&path).unwrap();
        cache
            .put_input_prediction("unit", 2, None, &long_json(1))
            .unwrap();
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        cache.db().busy_timeout(Duration::ZERO).unwrap();
        let done = cache
            .compress_input_predictions_within(Duration::from_secs(60))
            .unwrap();
        assert_eq!((done.rows, done.cut_short), (0, true));
        writer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(prediction_rows(&cache)[0].1, "text");
    }

    /// Between the read that finds a batch and its write transaction, a
    /// writer may replace a row (under a new rowid, or under the old one
    /// after a delete, plain or compressed): each row then holds what was
    /// written last, never the closure the read found, and a compressed row
    /// under a found rowid is passed over, not read as text.
    #[test]
    fn a_row_written_after_the_conversion_found_it_keeps_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        for i in 0..3 {
            cache
                .put_input_prediction(&format!("unit-{i}"), 2, None, &long_json(i))
                .unwrap();
        }
        let rowids = cache.plain_prediction_rowids(0, 10).unwrap();
        assert_eq!(rowids.len(), 3);
        let rewrite = |identity: &str, codec, json: &str, rowid: Option<i64>| {
            cache
                .put_input_prediction_as(codec, identity, 2, None, json)
                .unwrap();
            if let Some(rowid) = rowid {
                // SQLite gives a deleted row's rowid to the next insert when
                // it was the largest; the test puts it there directly.
                cache
                    .db()
                    .execute(
                        "UPDATE input_predictions SET rowid = ?1 WHERE identity = ?2",
                        params![rowid, identity],
                    )
                    .unwrap();
            }
        };
        rewrite("unit-0", codec::Codec::Plain, &long_json(50), None);
        rewrite(
            "unit-1",
            codec::Codec::Plain,
            &long_json(51),
            Some(rowids[1]),
        );
        rewrite(
            "unit-2",
            codec::Codec::Zstd,
            &long_json(52),
            Some(rowids[2]),
        );
        let (converted, kept_plain) = cache.compress_prediction_rows(&rowids).unwrap();
        assert_eq!(
            (converted, kept_plain),
            (1, 0),
            "only the plain row under a found rowid"
        );
        for (i, json) in [(0, 50), (1, 51), (2, 52)] {
            assert_eq!(
                cache.get_input_prediction(&format!("unit-{i}")).unwrap(),
                Some((2, long_json(json)))
            );
        }
        let kinds: Vec<String> = prediction_rows(&cache)
            .into_iter()
            .map(|row| row.1)
            .collect();
        assert_eq!(kinds, ["text", "blob", "blob"]);
    }

    /// A row of about 3.5 KB has a page to itself (rows up to about 4 KB
    /// need no overflow page). Shrunk in place, it would keep that page;
    /// moved, it frees it.
    #[test]
    fn the_conversion_frees_the_pages_its_rows_leave() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        let json = format!(
            "{{\"sources\":[{}]}}",
            "\"/src/a/b/c/module.rs\",".repeat(150)
        );
        for i in 0..400 {
            cache
                .put_input_prediction(&format!("unit-{i:03}"), 2, None, &json)
                .unwrap();
        }
        let pages = |cache: &FileHashCache<'_>| -> (i64, i64) {
            let page = |name: &str| -> i64 {
                cache
                    .db()
                    .pragma_query_value(None, name, |row| row.get(0))
                    .unwrap()
            };
            (page("page_count"), page("freelist_count"))
        };
        let (before, _) = pages(&cache);
        let done = cache
            .compress_input_predictions_within(Duration::from_secs(60))
            .unwrap();
        assert_eq!(done.rows, 400);
        let (after, free) = pages(&cache);
        assert_eq!(after, before, "nothing truncates the file");
        assert!(
            (after - free) * 4 < before,
            "{} pages in use of {before}",
            after - free
        );
    }

    #[test]
    fn prune_input_predictions_uses_the_wall_clock() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        cache
            .put_input_prediction("ancient", 1, None, "payload")
            .unwrap();
        cache
            .put_input_prediction("live", 1, None, "payload")
            .unwrap();
        set_prediction_last_used(&cache, "ancient", 1);
        assert_eq!(cache.prune_input_predictions().unwrap(), 1);
        assert!(cache.get_input_prediction("live").unwrap().is_some());
        assert_eq!(cache.get_input_prediction("ancient").unwrap(), None);
    }

    #[test]
    fn schema_upgrade_adds_last_used_and_keeps_existing_predictions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Connection::open(&db_path).unwrap();
        create_legacy_input_predictions(&db);
        assert!(!input_predictions_have_last_used(&db).unwrap());

        ensure_input_predictions_last_used(&db, 1_700_000_000).unwrap();
        assert!(input_predictions_have_last_used(&db).unwrap());
        // A second upgrade changes nothing, including the recorded default.
        ensure_input_predictions_last_used(&db, 1_700_000_999).unwrap();
        ensure_file_hash_cache_schema(&db).unwrap();
        drop(db);

        let cache = FileHashCache::open(&db_path).unwrap();
        // Existing rows read as used at the upgrade, not at their last write.
        assert_eq!(prediction_last_used(&cache, "old-a"), 1_700_000_000);
        assert_eq!(prediction_last_used(&cache, "old-b"), 1_700_000_000);
        assert_eq!(
            cache
                .get_input_prediction_at("old-a", 1_700_000_000)
                .unwrap(),
            Some((3, "payload-a".into()))
        );
        assert_eq!(
            cache
                .get_input_prediction_at("old-b", 1_700_000_000)
                .unwrap(),
            Some((4, "payload-b".into()))
        );
        // They outlive a prune until a full window after the upgrade.
        assert_eq!(
            cache
                .prune_input_predictions_at(1_700_000_000 + 2_592_000)
                .unwrap(),
            0
        );
        assert_eq!(
            cache
                .prune_input_predictions_at(1_700_000_000 + 2_592_001)
                .unwrap(),
            2
        );
        // A row recorded after the upgrade carries its own stamp.
        cache
            .put_input_prediction("new", 5, None, "payload-n")
            .unwrap();
        assert!(prediction_last_used(&cache, "new") > 1_700_000_999);
    }

    #[test]
    fn schema_upgrade_survives_many_connections_opening_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        {
            let db = Connection::open(&db_path).unwrap();
            db.pragma_update(None, "journal_mode", "WAL").unwrap();
            create_legacy_input_predictions(&db);
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let db_path = db_path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = Connection::open(&db_path).unwrap();
                    db.pragma_update(None, "busy_timeout", "5000").unwrap();
                    barrier.wait();
                    ensure_input_predictions_last_used(&db, 1_700_000_000)
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        let cache = FileHashCache::open(&db_path).unwrap();
        assert_eq!(prediction_last_used(&cache, "old-a"), 1_700_000_000);
        assert_eq!(prediction_last_used(&cache, "old-b"), 1_700_000_000);
    }

    #[test]
    fn legacy_payload_upgrade_survives_concurrent_opens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Connection::open(&db_path).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute_batch(
            "CREATE TABLE input_predictions (
                identity TEXT PRIMARY KEY, schema INTEGER NOT NULL,
                crate_name TEXT NOT NULL, record_json TEXT NOT NULL
             );",
        )
        .unwrap();
        drop(db);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let db_path = db_path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = Connection::open(&db_path).unwrap();
                    db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                    barrier.wait();
                    ensure_input_predictions_payload(&db).unwrap();
                    let cache = FileHashCache::Borrowed(&db);
                    cache
                        .put_input_prediction("new", 2, None, "current")
                        .unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let cache = FileHashCache::open(&db_path).unwrap();
        assert_eq!(
            cache.get_input_prediction("new").unwrap(),
            Some((2, "current".into()))
        );
    }

    #[test]
    fn file_hash_memo_key_includes_inode() {
        // An in-place swap that preserves path+size+mtime+ctime but changes the
        // inode (and content) must NOT return a stale memoized hash
        // (kunobi-ninja/kache#324).
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_file_hash_cache_schema(&conn).unwrap();
        let cache = FileHashCache::Borrowed(&conn);

        let fp = |inode: i64| FileFingerprint {
            path: "/x/lib.rlib".to_string(),
            size: 100,
            mtime_ns: 1,
            ctime_ns: 2,
            inode,
        };

        cache.put(&fp(10), "hash_for_inode_10").unwrap();
        assert_eq!(
            cache.get(&fp(10)).unwrap().as_deref(),
            Some("hash_for_inode_10")
        );
        assert_eq!(
            cache.get(&fp(20)).unwrap(),
            None,
            "a different inode (same path/size/mtime/ctime) must miss the memo"
        );
    }

    #[test]
    fn test_hash_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();

        let hash = hash_file(&file).unwrap();
        assert_eq!(hash.len(), 64); // blake3 hex is 64 chars

        // Same content = same hash
        let file2 = dir.path().join("test2.rs");
        std::fs::write(&file2, b"fn main() {}").unwrap();
        let hash2 = hash_file(&file2).unwrap();
        assert_eq!(hash, hash2);

        // Different content = different hash
        let file3 = dir.path().join("test3.rs");
        std::fs::write(&file3, b"fn main() { println!(\"hello\"); }").unwrap();
        let hash3 = hash_file(&file3).unwrap();
        assert_ne!(hash, hash3);

        // Larger than blake3's streaming read buffer so the digest spans
        // multiple reads instead of relying on a single in-memory buffer.
        let large = dir.path().join("large.rlib");
        let large_bytes: Vec<u8> = (0..256 * 1024 + 17)
            .map(|index| (index % 251) as u8)
            .collect();
        std::fs::write(&large, &large_bytes).unwrap();
        assert_eq!(
            hash_file(&large).unwrap(),
            blake3::hash(&large_bytes).to_hex().to_string()
        );
    }

    #[test]
    fn lookup_cached_too_small_or_unreadable_is_uncacheable() {
        // A sub-threshold file and an unreadable path both yield Uncacheable
        // from the persistent cache: the first via the min-size guard, the
        // second via the metadata-read failure arm.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let small = dir.path().join("small.rs");
        std::fs::write(&small, b"fn main() {}").unwrap();

        let fh = FileHashCache::open(&db_path).unwrap();
        assert!(matches!(
            fh.lookup_cached(&small),
            FileHashLookup::Uncacheable
        ));
        // Nonexistent path -> FileFingerprint::from_path errors -> Uncacheable.
        assert!(matches!(
            fh.lookup_cached(&dir.path().join("nope.rlib")),
            FileHashLookup::Uncacheable
        ));
    }

    #[test]
    fn the_boot_time_is_the_btime_line_of_proc_stat() {
        let stat = "cpu  1 2 3\nintr 5\nbtime 1794057339\nprocesses 9\n";
        assert_eq!(boot_time_in_proc_stat(stat), Some(1_794_057_339));
        assert_eq!(boot_time_in_proc_stat("cpu 1\nbtimes 7\n"), None);
    }

    #[test]
    fn a_row_recorded_before_this_boot_is_not_served() {
        // A hard stop can lose a file's last writes while its size, times and
        // inode, already journaled, survive: the file then reads as zeros
        // under a stamp the memo recorded with the old bytes' hash
        // (2026-11-07: seeded .rmeta files spread that way). A row from
        // before the boot is read again once, and the row the read records
        // is served.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("crashed.rmeta");
        std::fs::write(&file, vec![0u8; 70 * 1024]).unwrap();
        let fh = FileHashCache::open(&dir.path().join("index.db")).unwrap();
        let FileHashLookup::NeedsHash(stamp) = fh.lookup_cached(&file) else {
            panic!("a new file has no row");
        };
        fh.record_cached(&stamp, "beforethestop");
        fh.db()
            .execute(
                "UPDATE file_hashes SET updated_at = '2026-11-07 13:14:09'",
                [],
            )
            .unwrap();
        let path = stamp.path.as_str();
        let boot = 1_794_057_339; // 2026-11-07 13:15:39 UTC
        with_boot_time_for_tests(boot, || {
            assert!(
                matches!(fh.lookup_cached(&file), FileHashLookup::NeedsHash(_)),
                "the row predates the boot, so the file must be read again"
            );
            assert_eq!(fh.get(&stamp).unwrap(), None, "get serves no pre-boot row");
            assert!(
                fh.recorded_at(&[path]).unwrap().is_empty(),
                "a seeding's lookup must not vouch for a pre-boot row"
            );
            fh.record_cached(&stamp, "readafterboot");
            assert_eq!(
                fh.get(&stamp).unwrap().as_deref(),
                Some("readafterboot"),
                "a row recorded in this boot is served"
            );
        });
        with_boot_time_for_tests(boot - 3_600, || {
            fh.db()
                .execute(
                    "UPDATE file_hashes SET updated_at = '2026-11-07 13:14:09'",
                    [],
                )
                .unwrap();
            assert!(
                fh.recorded_at(&[path]).unwrap().contains_key(path),
                "a row from this boot keeps vouching"
            );
        });
    }

    #[test]
    fn lookup_cached_miss_then_record_then_hit_roundtrips() {
        // The daemon's lock-narrowing seam: a large file first reports
        // NeedsHash (miss), then after record_cached() a subsequent
        // lookup_cached() returns Hit with the recorded digest — without ever
        // computing a blake3 in lookup_cached itself. Covers NeedsHash, the
        // record_cached put arm, and the Hit arm.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let file = dir.path().join("large.rlib");
        std::fs::write(&file, vec![7u8; 70 * 1024]).unwrap();

        let fh = FileHashCache::open(&db_path).unwrap();
        let fp = match fh.lookup_cached(&file) {
            FileHashLookup::NeedsHash(fp) => fp,
            _ => panic!("expected NeedsHash on first lookup"),
        };
        fh.record_cached(&fp, "cafef00d");

        match fh.lookup_cached(&file) {
            FileHashLookup::Hit(h) => assert_eq!(h, "cafef00d"),
            _ => panic!("expected Hit after record_cached"),
        }
    }

    #[test]
    fn record_verified_honors_the_persistence_floor() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let below = dir.path().join("below.rlib");
        let at_floor = dir.path().join("at-floor.rlib");
        std::fs::write(&below, vec![1u8; 65_535]).unwrap();
        std::fs::write(&at_floor, vec![2u8; 65_536]).unwrap();

        let hasher = FileHashCache::open(&db_path).unwrap();
        hasher.record_verified(&FileFingerprint::from_path(&below).unwrap(), "below");
        hasher.record_verified(&FileFingerprint::from_path(&at_floor).unwrap(), "at-floor");

        assert!(matches!(
            hasher.lookup_cached(&below),
            FileHashLookup::Uncacheable
        ));
        assert!(matches!(
            hasher.lookup_cached(&at_floor),
            FileHashLookup::Hit(hash) if hash == "at-floor"
        ));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_hashes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "sub-threshold fingerprints must not be stored");
    }
}
