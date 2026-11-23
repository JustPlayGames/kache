//! Outputs an incremental compile wrote, which no cache entry holds.
//!
//! A unit compiled on an incremental lane is never stored, and its rlib is
//! not what a fresh compile of the same sources writes: rustc partitions an
//! incremental build's code differently. A unit that links that rlib keys on
//! its bytes, so its key recurs only while that very file stays in place.
//! Storing such a link costs its store (a desktop program is hundreds of
//! megabytes) and its dep-info pre-pass on every edit, and no other checkout
//! can ever ask for it.
//!
//! An incremental compile marks what it wrote: one small file per output under
//! the profile's policy root, holding the output's size and which file it is
//! (its identity on Unix, its creation time on Windows). A mark whose file has since been replaced, by a
//! cache hit or an ordinary compile, no longer matches and counts as absent.
//! A mark is a hint only: a wrong one changes what is stored, never what is
//! built.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Under `<profile>/incremental.kache-auto`, beside the units' state.
const DIR: &str = "private";
const VERSION: &str = "v2";

/// What a mark records of its output, read from the file's metadata: its
/// size and which file it is. Not its modification time: Cargo redates what
/// a unit wrote once the unit ends (a check's metadata, to the moment it
/// invoked the compiler), and the units built on it are started after that.
fn stamp(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    // A file written anew is created anew; redating leaves that time alone.
    #[cfg(windows)]
    let identity = {
        use std::os::windows::fs::MetadataExt;
        metadata.creation_time()
    };
    #[cfg(not(any(unix, windows)))]
    let identity = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{VERSION} {} {identity}", metadata.len()))
}

/// Where the mark for `output` lives: named by a digest of its path, so the
/// per-unit layout's equal file names in different units never share one.
fn mark_path(output: &Path) -> Option<PathBuf> {
    let profile = crate::cargo_layout::deps_profile(output.parent()?)?;
    let digest = blake3::hash(output.as_os_str().as_encoded_bytes()).to_hex();
    Some(
        profile
            .join(super::AUTO_ROOT)
            .join(DIR)
            .join(&digest.as_str()[..32]),
    )
}

/// Whether a unit that takes `output` as an extern can link it: an rlib or
/// rmeta, or a dylib or proc macro.
fn linkable(output: &Path) -> bool {
    output
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "rlib" | "rmeta" | "so" | "dylib" | "dll"))
}

/// How long a claim on outputs a compile has yet to write holds. A compile
/// that ends replaces or forgets its claim; one that died leaves it to lapse.
const CLAIM_SECS: u64 = 30 * 60;
const CLAIM: &str = "claimed ";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |now| now.as_secs())
}

/// Claim the outputs an incremental compile is about to write. Cargo starts
/// the units built on a library when its metadata is out, before the compile
/// ends and marks what it wrote; without the claim they would take the
/// metadata for a stored unit's.
pub(crate) fn claim<'a>(outputs: impl IntoIterator<Item = &'a Path>) {
    let claim = format!("{CLAIM}{}", now_secs());
    for output in outputs.into_iter().filter(|output| linkable(output)) {
        if let Some(path) = mark_path(output)
            && let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = crate::atomic::atomic_replace_deferrable(&path, claim.as_bytes(), false);
        }
    }
}

/// Drop the claims of a compile that wrote nothing of its own after all: a
/// hit stopped it and restored the outputs.
pub(crate) fn forget<'a>(outputs: impl IntoIterator<Item = &'a Path>) {
    for path in outputs.into_iter().filter_map(mark_path) {
        if std::fs::read_to_string(&path).is_ok_and(|recorded| recorded.starts_with(CLAIM)) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Mark the outputs an incremental compile just wrote. Best effort: a mark
/// that cannot be written leaves the old behavior, a stored link.
pub(crate) fn mark<'a>(outputs: impl IntoIterator<Item = &'a Path>) {
    for output in outputs.into_iter().filter(|output| linkable(output)) {
        let (Some(path), Some(stamp)) = (mark_path(output), stamp(output)) else {
            continue;
        };
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = crate::atomic::atomic_replace_deferrable(&path, stamp.as_bytes(), false);
        }
    }
}

/// Whether `output` is the file an incremental compile wrote and marked. A
/// mark its file no longer matches is removed.
pub(crate) fn is_private(output: &Path) -> bool {
    let Some(path) = mark_path(output) else {
        return false;
    };
    let Ok(recorded) = std::fs::read_to_string(&path) else {
        return false;
    };
    if let Some(claimed) = recorded.strip_prefix(CLAIM) {
        if claimed
            .parse::<u64>()
            .is_ok_and(|at| now_secs().saturating_sub(at) <= CLAIM_SECS)
        {
            return true;
        }
    } else if stamp(output).is_some_and(|current| current == recorded) {
        return true;
    }
    let _ = std::fs::remove_file(&path);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(dir: &Path) -> PathBuf {
        let deps = dir.join("debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        deps
    }

    #[test]
    fn a_marked_output_is_private_until_its_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let rlib = deps(dir.path()).join("libedited-0123456789abcdef.rlib");
        std::fs::write(&rlib, b"incremental").unwrap();
        assert!(!is_private(&rlib), "nothing marked yet");

        mark([rlib.as_path()]);
        assert!(is_private(&rlib));

        // Cargo redates a unit's outputs when the unit ends; the file is the
        // same one.
        filetime::set_file_mtime(&rlib, filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
        assert!(is_private(&rlib), "redated, not replaced");

        // A hit or an ordinary compile writes a new file in its place.
        std::fs::remove_file(&rlib).unwrap();
        std::fs::write(&rlib, b"from the cache, longer").unwrap();
        assert!(!is_private(&rlib));
        assert!(
            !mark_path(&rlib).unwrap().exists(),
            "a mark its file no longer matches is removed"
        );
    }

    #[test]
    fn a_claimed_output_is_private_before_it_is_written_until_forgotten_or_marked() {
        let dir = tempfile::tempdir().unwrap();
        let rmeta = deps(dir.path()).join("libedited-0123456789abcdef.rmeta");
        claim([rmeta.as_path()]);
        assert!(is_private(&rmeta), "claimed before the compile wrote it");
        std::fs::write(&rmeta, b"metadata").unwrap();
        assert!(is_private(&rmeta), "and while the compile goes on");

        // The compile's end replaces the claim with what it wrote.
        mark([rmeta.as_path()]);
        std::fs::remove_file(&rmeta).unwrap();
        std::fs::write(&rmeta, b"from the cache, longer").unwrap();
        assert!(!is_private(&rmeta));

        // A hit that stopped the compile forgets the claim, and only a claim.
        claim([rmeta.as_path()]);
        forget([rmeta.as_path()]);
        assert!(!is_private(&rmeta));
        mark([rmeta.as_path()]);
        forget([rmeta.as_path()]);
        assert!(is_private(&rmeta), "a mark is not a claim");

        // A claim nobody ended lapses.
        let stale = format!("{CLAIM}{}", now_secs() - CLAIM_SECS - 1);
        std::fs::write(mark_path(&rmeta).unwrap(), stale).unwrap();
        assert!(!is_private(&rmeta));
    }

    #[test]
    fn only_linkable_outputs_in_a_profile_are_marked() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let dep_info = deps.join("edited-0123456789abcdef.d");
        let rmeta = deps.join("libedited-0123456789abcdef.rmeta");
        let elsewhere = dir.path().join("libloose.rlib");
        for file in [&dep_info, &rmeta, &elsewhere] {
            std::fs::write(file, b"x").unwrap();
        }
        mark([dep_info.as_path(), rmeta.as_path(), elsewhere.as_path()]);
        assert!(is_private(&rmeta));
        assert!(!is_private(&dep_info));
        assert!(!is_private(&elsewhere), "outside a Cargo profile");
    }
}
