//! Reclaim idle managed graphs during the existing target-unit cleanup pass.

use super::*;
use crate::unit_prune::Pruned;
use std::io;
use std::time::Duration;

pub(crate) fn prune_stale(
    cache_dir: &Path,
    target: &Path,
    window: Duration,
    at: SystemTime,
) -> Pruned {
    let Ok(Some(_reservation)) = crate::target_use::try_exclusive(cache_dir) else {
        return Pruned::default();
    };
    sweep(
        target,
        window,
        at,
        true,
        crate::cache_fs::probe(target).is_local,
    )
}

pub(crate) fn preview_stale(target: &Path, window: Duration, at: SystemTime) -> Pruned {
    sweep(
        target,
        window,
        at,
        false,
        crate::cache_fs::probe(target).is_local,
    )
}

/// Remove the finalized sessions under `rustc_dir` older than the newest of
/// their crate, once a compile has finished. rustc reads only the newest
/// (`find_source_directory`). Up to 1.99 it collects an older session only when
/// the next compile ends, which leaves about twice a graph's bytes at rest for
/// a session nothing reads (2026-10-31: 38-52 GB of a machine's 71-93 GB);
/// nightly-2026-10-19 collects it itself, and this then finds nothing to do.
/// Best-effort: what stays, rustc's next compile collects. The caller holds the
/// unit's lock, so no compiler runs here.
pub(super) fn drop_superseded_sessions(rustc_dir: &Path) {
    let Ok(crates) = fs::read_dir(rustc_dir) else {
        return;
    };
    for krate in crates.flatten() {
        let Ok(entries) =
            fs::read_dir(krate.path()).and_then(|entries| entries.collect::<io::Result<Vec<_>>>())
        else {
            continue;
        };
        // None for an unfamiliar layout or a session still `-working`.
        let Some(newest) = worktree_seed::newest_session(&entries, 1) else {
            continue;
        };
        for entry in entries {
            let session = entry.path();
            if worktree_seed::session_timestamp(&session, 2).is_some_and(|time| time < newest)
                && fs::remove_dir_all(&session).is_ok()
            {
                let _ = fs::remove_file(session_lock_file(&session));
            }
        }
    }
}

/// rustc's lock file of a session `s-<time>-<random>-<hash>`:
/// `s-<time>-<random>.lock`.
fn session_lock_file(session: &Path) -> PathBuf {
    let name = session.file_name().unwrap_or_default().to_string_lossy();
    let stem = name
        .rsplit_once('-')
        .map_or(name.as_ref(), |(stem, _)| stem);
    session.with_file_name(format!("{stem}.lock"))
}

fn sweep(
    target: &Path,
    window: Duration,
    at: SystemTime,
    remove: bool,
    local: Option<bool>,
) -> Pruned {
    let mut pruned = Pruned::default();
    if !real_directory(target) || local != Some(true) {
        return pruned;
    }
    // Profiles can outlive their last Cargo fingerprint. Look at both the
    // native and target-triple layouts without descending into graph data.
    for (profile, version) in crate::unit_prune::subdirectories(target)
        .into_iter()
        .flat_map(|profile| {
            let nested = crate::unit_prune::subdirectories(&profile);
            std::iter::once(profile).chain(nested)
        })
        // Retired state still expires under the same locks and age policy.
        .flat_map(|profile| ["v1", POLICY_VERSION].map(|version| (profile.clone(), version)))
    {
        let auto_root = profile.join(AUTO_ROOT);
        let root = auto_root.join(version);
        if !real_directory(&auto_root) || !real_directory(&root) {
            continue;
        }
        let _profile_locks = if remove {
            let Some(held) = crate::unit_prune::hold(&profile) else {
                continue;
            };
            Some(held)
        } else {
            None
        };
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(key) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_hex_digest(&key) || !real_directory(&entry.path()) {
                continue;
            }
            let unit_dir = entry.path();
            let unit = AdaptiveUnit {
                source_file: None,
                original_incremental: profile.join("incremental"),
                policy_guard: Vec::new(),
                unit_key: key,
                state_path: unit_dir.join("state.json"),
                lock_path: unit_dir.join("unit.lock"),
                rustc_dir: unit_dir.join("rustc"),
                unit_dir,
            };
            if !fs::symlink_metadata(&unit.lock_path).is_ok_and(|metadata| metadata.is_file()) {
                continue;
            }
            let _unit_lock = if remove {
                let Some(lock) = unit.lock() else {
                    continue;
                };
                Some(lock)
            } else {
                None
            };
            let Some(bytes) = stale_bytes(&unit, window, at) else {
                continue;
            };
            // Keep the directory and lock inode: a compiler may already have
            // opened that lock. Unlinking it would permit two independent locks.
            if !remove || reset_locked(&unit) {
                pruned.units += 1;
                pruned.bytes += bytes;
            }
        }
    }
    pruned
}

fn stale_bytes(unit: &AdaptiveUnit, window: Duration, at: SystemTime) -> Option<u64> {
    if !real_directory(&unit.rustc_dir) {
        return None;
    }
    let last_used_secs = match unit.load_state() {
        LoadedState::Valid(state) => state.last_used_secs,
        // Immediate leases with no prior learning remove state.json at exit.
        LoadedState::Missing => 0,
        LoadedState::Corrupt | LoadedState::Unavailable => return None,
    };
    // Every lease replaces or removes state.json, refreshing its parent mtime.
    // Also honor the recorded clock when it is ahead of the filesystem clock.
    let modified = fs::symlink_metadata(&unit.unit_dir).ok()?.modified().ok()?;
    let recorded = UNIX_EPOCH.checked_add(Duration::from_secs(last_used_secs))?;
    let last_used = modified.max(recorded);
    if at.duration_since(last_used).ok()? < window {
        return None;
    }
    Some(crate::unit_prune::size(&unit.rustc_dir) + crate::unit_prune::size(&unit.state_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_state_expires_without_touching_busy_or_unknown_versions() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("debug");
        let unit = |version: &str| {
            profile
                .join("incremental.kache-auto")
                .join(version)
                .join("a".repeat(64))
        };
        for version in ["v1", "v2", "v3"] {
            let path = unit(version);
            fs::create_dir_all(path.join("rustc")).unwrap();
            fs::write(path.join("rustc/graph.bin"), b"payload").unwrap();
            fs::write(path.join("unit.lock"), b"").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(100, 0)).unwrap();
        }
        let old_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(unit("v1").join("unit.lock"))
            .unwrap();
        old_lock.try_lock().unwrap();
        let window = Duration::from_secs(10);
        let at = UNIX_EPOCH + Duration::from_secs(110);
        let preview = sweep(dir.path(), window, at, false, Some(true));
        assert_eq!(preview.units, 2);
        assert_eq!(preview.bytes, 14);
        assert!(unit("v1").join("rustc/graph.bin").is_file());
        assert!(unit("v2").join("rustc/graph.bin").is_file());

        assert_eq!(sweep(dir.path(), window, at, true, Some(true)).units, 1);
        assert!(unit("v1").join("rustc/graph.bin").is_file());
        assert!(!unit("v2").join("rustc").exists());
        old_lock.unlock().unwrap();
        assert_eq!(sweep(dir.path(), window, at, true, Some(true)).units, 1);
        assert!(!unit("v1").join("rustc").exists());
        assert!(unit("v1").join("unit.lock").is_file());
        assert_eq!(
            fs::read(unit("v3").join("rustc/graph.bin")).unwrap(),
            b"payload"
        );
    }

    /// rustc reads only the newest finalized session, whose name's time is
    /// base 36. A layout it would not recognize keeps everything.
    #[test]
    fn a_finished_compile_keeps_only_the_newest_session() {
        let dir = tempfile::tempdir().unwrap();
        let rustc = dir.path().join("rustc");
        let session = |krate: &str, name: &str| {
            let path = rustc.join(krate).join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("dep-graph.bin"), name).unwrap();
            path
        };
        let lock = |krate: &str, name: &str| {
            let path = rustc.join(krate).join(name);
            fs::write(&path, b"").unwrap();
            path
        };
        // Numerically 35 < 36, though "z" sorts after "10".
        let older = session("app-1", "s-z-rand-hash1");
        let older_lock = lock("app-1", "s-z-rand.lock");
        let newest = session("app-1", "s-10-rand-hash2");
        let newest_lock = lock("app-1", "s-10-rand.lock");
        let waiting = session("lib-2", "s-1-rand-hash");
        let working = session("lib-2", "s-2-rand-working");

        drop_superseded_sessions(&rustc);

        assert!(!older.exists() && !older_lock.exists());
        assert!(newest.join("dep-graph.bin").is_file() && newest_lock.exists());
        assert!(waiting.exists() && working.exists());
    }

    #[test]
    fn an_unverified_filesystem_keeps_graphs_and_creates_no_cargo_locks() {
        for local in [None, Some(false)] {
            let dir = tempfile::tempdir().unwrap();
            let profile = dir.path().join("debug");
            let unit = profile
                .join(AUTO_ROOT)
                .join(POLICY_VERSION)
                .join("a".repeat(64));
            let graph = unit.join("rustc");
            fs::create_dir_all(&graph).unwrap();
            fs::write(graph.join("graph.bin"), b"payload").unwrap();
            fs::write(unit.join("unit.lock"), b"").unwrap();
            filetime::set_file_mtime(&unit, filetime::FileTime::from_unix_time(100, 0)).unwrap();
            let window = Duration::from_secs(10);
            let at = UNIX_EPOCH + Duration::from_secs(110);
            assert_eq!(sweep(dir.path(), window, at, true, local).units, 0);
            assert_eq!(fs::read(graph.join("graph.bin")).unwrap(), b"payload");
            assert!(!profile.join(".cargo-lock").exists());
            assert_eq!(sweep(dir.path(), window, at, true, Some(true)).units, 1);
            assert!(!graph.exists());
        }
    }
}
