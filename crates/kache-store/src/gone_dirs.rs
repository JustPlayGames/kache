//! Pruning `file_hashes` rows of directories that no longer exist.
//!
//! Every worktree and target directory a build hashes leaves its rows in the
//! memo, and the 30-day retention prune ([`FileHashCache::prune_file_hashes`])
//! is the only thing that removes them. On a machine where agents make and
//! delete a worktree per task, nearly every row belongs to a directory that
//! is already gone: 27.6 of 28.0 million rows on a shared build host
//! (2026-11-06), about 12.8 GB of a 29 GB index. Such a row can never be
//! served again, since its file is gone, so the GC sweep deletes them, a
//! directory at a time, in short write transactions.

use crate::file_hash::FileHashCache;
use rusqlite::{ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Rows one write transaction deletes. Each batch holds the index's write
/// lock, which every store and hit stamp waits on. On a copy of the
/// 28-million-row index above, with a writer committing every 5 ms beside
/// it, batches of 500 kept that writer's wait at 3 ms (p90) and 8 ms (p99),
/// under the 100 ms a hit stamp waits; batches of 2,000 deleted 1.6 times as
/// fast but made it wait 18 ms and 55-104 ms.
pub const GONE_DIR_PRUNE_BATCH: usize = 500;

/// How long one GC sweep spends on the prune: the walk and the deletes. A
/// prune the budget cuts short goes on at the next sweep, which walks again
/// from the start; the directories already pruned have no rows left to walk.
/// The copy above took 12 sweeps at 2,000 rows a batch, so about 20 at 500;
/// once its backlog was gone, a sweep's walk over the 400,000 rows left
/// took 1.7 s.
pub const GONE_DIR_PRUNE_BUDGET: Duration = Duration::from_secs(60);

/// What one [`FileHashCache::prune_file_hashes_under_gone_dirs`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoneDirPrune {
    /// Rows deleted.
    pub rows: usize,
    /// Gone directories whose rows were deleted.
    pub dirs: usize,
    /// Write transactions the deletes took.
    pub batches: usize,
    /// The longest any batch held the write lock.
    pub longest_batch: Duration,
    /// Whether the budget, or a writer holding the lock, stopped the prune
    /// before the walk reached the end of the table.
    pub cut_short: bool,
}

/// What a directory's lookup said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirState {
    /// The directory is there.
    Present,
    /// The lookup said it does not exist (`ENOENT`), at this Unix second.
    Gone(i64),
    /// Anything else: no permission to look, an I/O error, a file where a
    /// directory was. The rows stay.
    Unknown,
}

/// Look `dir` up without following a final symlink. Only "not found" counts
/// as gone: a directory this account may not search (another account's
/// worktree in a shared store) answers `EACCES` and keeps its rows.
fn dir_state(dir: &str) -> DirState {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => DirState::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirState::Gone(unix_now()),
        Err(_) => DirState::Unknown,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Where the walk stands on a row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowDir {
    /// Every directory down to the row's file is there: the row stays.
    Present,
    /// This directory is gone, seen so at this Unix second: its rows go.
    Gone(String, i64),
    /// This directory's lookup was not sure: its rows stay, and the walk
    /// skips them.
    Unsure(String),
}

/// The first directory on the way down to `path`'s parent that is not there,
/// or that a lookup was not sure of. A path that is not absolute counts as
/// present. `known` keeps each directory's answer for the walk, so a
/// directory of a million rows is looked up once.
fn row_dir(
    path: &str,
    known: &mut HashMap<String, DirState>,
    look: &dyn Fn(&str) -> DirState,
) -> RowDir {
    let Some(parent) = path.rfind('/').map(|end| &path[..end]) else {
        return RowDir::Present;
    };
    if !path.starts_with('/') {
        return RowDir::Present;
    }
    let mut end = 0;
    while end < parent.len() {
        end = parent[end + 1..]
            .find('/')
            .map_or(parent.len(), |next| end + 1 + next);
        let dir = &parent[..end];
        let state = match known.get(dir) {
            Some(state) => *state,
            None => {
                let state = look(dir);
                known.insert(dir.to_owned(), state);
                state
            }
        };
        match state {
            DirState::Present => {}
            DirState::Gone(at) => return RowDir::Gone(dir.to_owned(), at),
            DirState::Unknown => return RowDir::Unsure(dir.to_owned()),
        }
    }
    RowDir::Present
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

impl FileHashCache<'_> {
    /// Delete the rows of files under directories that no longer exist,
    /// for at most `budget`. Unix only: elsewhere it does nothing.
    ///
    /// The walk seeks through the primary key, one row per directory that is
    /// still there and one per gone directory, in short reads that pin
    /// nothing. A gone directory's rows go by key range in batches of
    /// [`GONE_DIR_PRUNE_BATCH`], each its own write transaction, with a pause
    /// as long as the batch between them so that waiting writers get the
    /// lock. A row written after the directory was seen gone stays: the
    /// directory was made again (a pool worktree of the same name) and a
    /// build is filling it. The rows are a memo, so a row deleted while its
    /// file was in use costs that file one hash, never a wrong one.
    pub fn prune_file_hashes_under_gone_dirs(
        &self,
        budget: Duration,
    ) -> rusqlite::Result<GoneDirPrune> {
        if cfg!(unix) {
            self.prune_file_hashes_under_gone_dirs_with(budget, GONE_DIR_PRUNE_BATCH, &dir_state)
        } else {
            Ok(GoneDirPrune::default())
        }
    }

    pub(crate) fn prune_file_hashes_under_gone_dirs_with(
        &self,
        budget: Duration,
        batch: usize,
        look: &dyn Fn(&str) -> DirState,
    ) -> rusqlite::Result<GoneDirPrune> {
        let start = Instant::now();
        let mut done = GoneDirPrune::default();
        let mut known = HashMap::new();
        let mut from_dir: Option<String> = None;
        let mut after_row = String::new();
        let mut at_or_after = self
            .db()
            .prepare("SELECT path FROM file_hashes WHERE path >= ?1 ORDER BY path LIMIT 1")?;
        let mut past = self
            .db()
            .prepare("SELECT path FROM file_hashes WHERE path > ?1 ORDER BY path LIMIT 1")?;
        loop {
            if start.elapsed() >= budget {
                done.cut_short = true;
                return Ok(done);
            }
            let next: Option<String> = match &from_dir {
                Some(bound) => at_or_after.query_row([bound], |row| row.get(0)),
                None => past.query_row([&after_row], |row| row.get(0)),
            }
            .optional()?;
            let Some(path) = next else {
                return Ok(done);
            };
            let (dir, seen_gone_at) = match row_dir(&path, &mut known, look) {
                RowDir::Present => {
                    after_row = path;
                    from_dir = None;
                    continue;
                }
                RowDir::Unsure(dir) => {
                    from_dir = Some(format!("{dir}0"));
                    continue;
                }
                RowDir::Gone(dir, at) => (dir, at),
            };
            // Every path under `dir` starts with `dir/`, and `0` follows `/`.
            let (low, high) = (format!("{dir}/"), format!("{dir}0"));
            loop {
                if start.elapsed() >= budget {
                    done.cut_short = true;
                    return Ok(done);
                }
                let held = Instant::now();
                let deleted = match self.delete_batch(&low, &high, seen_gone_at, batch) {
                    Ok(deleted) => deleted,
                    Err(error) if is_busy(&error) => {
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
                    break;
                }
                std::thread::sleep(held.max(Duration::from_millis(5)));
            }
            done.dirs += 1;
            from_dir = Some(high);
        }
    }

    fn delete_batch(
        &self,
        low: &str,
        high: &str,
        seen_gone_at: i64,
        batch: usize,
    ) -> rusqlite::Result<usize> {
        let tx = Transaction::new_unchecked(self.db(), TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM file_hashes WHERE path IN (
                 SELECT path FROM file_hashes
                 WHERE path >= ?1 AND path < ?2 AND updated_at < datetime(?3, 'unixepoch')
                 LIMIT ?4)",
            params![
                low,
                high,
                seen_gone_at,
                i64::try_from(batch).unwrap_or(i64::MAX)
            ],
        )?;
        tx.commit()?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests;
