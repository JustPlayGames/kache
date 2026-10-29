//! `kache gc --stale-targets <days>` (lane KACHE-W21): the store's blobs that
//! target directories unused for that long still hardlink.
//!
//! A blob a target directory hardlinks is held by it: evicting the entry
//! frees nothing, so a size sweep leaves it out and the store can sit far
//! over its cap on disk. Deleting whole stale targets (`kache clean
//! --stale`) frees those bytes and everything else in them. This frees
//! only the store's share: it lists the files in stale targets that are
//! hardlinks of a store blob, and with `--apply` unlinks just those. Cargo
//! sees the output missing the next time that target is built and runs the
//! unit again, which a cache hit restores. It is never run by itself: only
//! `--apply` removes anything.

use anyhow::Result;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

use crate::config::Config;
#[cfg(unix)]
use crate::store::Store;

/// One stale target and the store blob hardlinks in it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct StaleTarget {
    pub path: PathBuf,
    pub idle_days: u64,
    /// Files that are hardlinks of a store blob.
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    /// Skipped: a Cargo build holds it right now.
    pub in_use: bool,
}

/// What a run found, or with `--apply` removed.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Report {
    pub days: u64,
    pub applied: bool,
    pub targets: Vec<StaleTarget>,
    pub files: usize,
    pub bytes: u64,
    /// With `--apply`: files unlinked, and those left because they changed
    /// since they were listed or could not be removed.
    pub unlinked: usize,
    pub left: usize,
}

/// `(device, inode)` of every store blob another name links to.
#[cfg(unix)]
fn shared_blob_inodes(store_dir: &Path) -> std::collections::HashSet<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut inodes = std::collections::HashSet::new();
    let Ok(shards) = std::fs::read_dir(store_dir.join("blobs")) else {
        return inodes;
    };
    for shard in shards.flatten() {
        let Ok(blobs) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for blob in blobs.flatten() {
            if let Ok(meta) = std::fs::symlink_metadata(blob.path())
                && meta.is_file()
                && meta.nlink() > 1
            {
                inodes.insert((meta.dev(), meta.ino()));
            }
        }
    }
    inodes
}

/// The files under `root` (symlinks not followed) that are one of `inodes`.
#[cfg(unix)]
fn links_under(root: &Path, inodes: &std::collections::HashSet<(u64, u64)>) -> (Vec<PathBuf>, u64) {
    use std::os::unix::fs::MetadataExt;
    let mut files = Vec::new();
    let mut bytes = 0;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                dirs.push(entry.path());
            } else if meta.is_file()
                && meta.nlink() > 1
                && inodes.contains(&(meta.dev(), meta.ino()))
            {
                bytes += meta.len();
                files.push(entry.path());
            }
        }
    }
    files.sort();
    (files, bytes)
}

/// Find, and with `apply` unlink, the store blob hardlinks in tracked
/// targets no build used for `days` days.
#[cfg(unix)]
pub(crate) fn run(config: &Config, days: u64, apply: bool) -> Result<Report> {
    use std::os::unix::fs::MetadataExt;
    let store = Store::open(config)?;
    let inodes = shared_blob_inodes(&config.store_dir());
    let now = chrono::Utc::now().timestamp();
    let mut report = Report {
        days,
        applied: apply,
        ..Report::default()
    };
    for tracked in store.tracked_target_roots(0)? {
        let Some(idle) = crate::target_cleanup::idle_secs(config, &tracked, now) else {
            continue;
        };
        let idle_days = u64::try_from(idle).unwrap_or(0) / 86_400;
        if idle_days < days || !crate::target_cleanup::intact(&tracked) {
            continue;
        }
        let (files, bytes) = links_under(&tracked.path, &inodes);
        if files.is_empty() {
            continue;
        }
        let in_use = crate::cli::target_in_use(&tracked.path);
        if !in_use {
            report.files += files.len();
            report.bytes += bytes;
        }
        if apply && !in_use {
            for file in &files {
                // Listed a moment ago: unlink it only while it is still a
                // store blob's other name.
                let still = std::fs::symlink_metadata(file).is_ok_and(|meta| {
                    meta.is_file() && meta.nlink() > 1 && inodes.contains(&(meta.dev(), meta.ino()))
                });
                if still && std::fs::remove_file(file).is_ok() {
                    report.unlinked += 1;
                } else {
                    report.left += 1;
                }
            }
        }
        report.targets.push(StaleTarget {
            path: tracked.path,
            idle_days,
            files,
            bytes,
            in_use,
        });
    }
    Ok(report)
}

#[cfg(not(unix))]
pub(crate) fn run(_config: &Config, _days: u64, _apply: bool) -> Result<Report> {
    anyhow::bail!("`kache gc --stale-targets` reads hardlinks by inode, on Unix only for now")
}

/// The report as `kache gc --stale-targets` prints it.
pub(crate) fn lines(report: &Report) -> Vec<String> {
    use crate::term;
    let mut lines = Vec::new();
    for target in &report.targets {
        lines.push(format!(
            "{}  {} unused  {} in {}{}",
            term::home_path(&target.path),
            term::counted(target.idle_days, "day", "days"),
            term::bytes(target.bytes),
            term::counted(target.files.len() as u64, "file", "files"),
            if target.in_use {
                "  (a build holds it: skipped)"
            } else {
                ""
            }
        ));
    }
    let total = format!(
        "{} in {} hardlinked from the store by targets unused for {}",
        term::bytes(report.bytes),
        term::counted(report.files as u64, "file", "files"),
        term::counted(report.days, "day", "days")
    );
    if report.applied {
        lines.push(format!(
            "{total}: unlinked {}{}. The next sweep can evict their entries; a build of \
             those targets restores them from the cache.",
            report.unlinked,
            if report.left > 0 {
                format!(", left {} that changed or could not be removed", report.left)
            } else {
                String::new()
            }
        ));
    } else {
        lines.push(format!(
            "{total}. Nothing removed: `--apply` unlinks these files (only them; a build \
             of those targets restores them from the cache)."
        ));
    }
    lines
}

#[cfg(all(test, unix))]
mod tests;
