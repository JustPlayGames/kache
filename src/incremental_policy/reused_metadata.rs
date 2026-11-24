//! The metadata an incremental compile may take unchanged from its session.
//!
//! rustc saves a library's metadata in its incremental session
//! (`<dir>/<crate>-<id>/s-<time>-<random>-<hash>/metadata.rmeta`). When a
//! later compile on that session finds the metadata unchanged, rustc links
//! the saved file as the unit's `.rmeta` instead of writing it again, so the
//! `.rmeta` keeps the time the saved file was made, and the `.rlib` beside it
//! is new.
//!
//! Cargo judges the units built on a library by that time across its runs: a
//! library built on another stays fresh while its outputs are newer than the
//! other's `.rmeta`. Inside one run Cargo rebuilds what is built on a unit it
//! rebuilt; across runs that build other packages in one target folder
//! (`cargo build -p a`, then `-p b`) it compares times only. That holds while
//! the saved file is the one the folder held when the units above were
//! built. It is not after an import, which copies another folder's session
//! with that folder's times, or when something else replaced the folder's
//! `.rmeta` since the session's last compile, as a restore of the shared
//! tree does, which leaves the session in place. A unit above, built on what
//! the folder held, would then be called fresh on a library it was not built
//! on: rustc refuses the next link of it (E0463, `Rejecting via hash`), and a
//! program built on it alone is not linked again at all.
//!
//! So before such a compile the saved file gets a file of its own with the
//! same bytes, written now ([`refresh`]). What rustc then links is newer than
//! anything built on the folder's earlier metadata, and older than what Cargo
//! starts once rustc reports the metadata ready, as a written `.rmeta` is.
//! Where the saved file is the very file the folder holds, it is left: the
//! metadata is the folder's own, and a compile that leaves it unchanged lets
//! Cargo skip the units built on it in a later run.
//!
//! The time of an existing file is never set: another name may share its
//! inode (an output of this folder, or of a donor), and every unit built on
//! that name would turn stale or fresh with it.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// The file a session saves a library's metadata in.
const SAVED: &str = "metadata.rmeta";

/// Where [`refresh`] writes the copy before renaming it over [`SAVED`].
const STAGED: &str = "metadata.rmeta.kache-fresh";

/// The incremental directory `args` give rustc: the last `-C incremental=`
/// in any of its spellings, as rustc takes the last.
fn incremental_dir(args: &[String]) -> Option<&str> {
    let mut found = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        let value = if matches!(argument.as_str(), "-C" | "--codegen") {
            arguments
                .next()
                .and_then(|next| next.strip_prefix("incremental="))
        } else {
            argument
                .strip_prefix("-Cincremental=")
                .or_else(|| argument.strip_prefix("--codegen=incremental="))
        };
        if value.is_some() {
            found = value;
        }
    }
    found
}

/// The saved metadata of each session rustc may take up for `crate_name` in
/// `dir`: the newest finalized session of every `<crate_name>-<id>` folder
/// (an isolated directory holds one; more when the unit's crate id moved).
fn saved_metadata(dir: &Path, crate_name: &str) -> Vec<PathBuf> {
    let Ok(crates) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut saved = Vec::new();
    for entry in crates.flatten() {
        let name = entry.file_name();
        let Some((name, id)) = name.to_str().and_then(|name| name.rsplit_once('-')) else {
            continue;
        };
        if name != crate_name || id.is_empty() || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let Ok(sessions) = fs::read_dir(entry.path()) else {
            continue;
        };
        let newest = sessions
            .flatten()
            .filter(|session| session.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|session| {
                let path = session.path();
                super::worktree_seed::session_timestamp(&path, 2).map(|time| (time, path))
            })
            .max_by_key(|(time, _)| *time);
        if let Some((_, session)) = newest {
            let file = session.join(SAVED);
            if fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file()) {
                saved.push(file);
            }
        }
    }
    saved
}

/// Before rustc compiles on the incremental session `args` name, give its
/// saved metadata a file of its own, written now, unless it is the file the
/// target folder holds as the unit's `.rmeta`. Only a compile that writes
/// metadata into `out_dir` can take the saved file as its output. Best
/// effort: a saved file that cannot be copied is removed, and rustc writes
/// the metadata again.
pub(crate) fn prepare(
    args: &[String],
    cwd: &Path,
    out_dir: Option<&Path>,
    crate_name: Option<&str>,
    extra_filename: Option<&str>,
    emit: &[String],
) {
    let (Some(out_dir), Some(crate_name), Some(dir)) = (out_dir, crate_name, incremental_dir(args))
    else {
        return;
    };
    if !emit.iter().any(|kind| kind == "metadata") {
        return;
    }
    let held = out_dir.join(format!(
        "lib{}.rmeta",
        crate::args::format_crate_output_stem(crate_name, extra_filename.unwrap_or(""))
    ));
    let held = kache_fs::file_identity(&held).ok();
    for saved in saved_metadata(&cwd.join(dir), crate_name) {
        if held.is_some() && kache_fs::file_identity(&saved).ok() == held {
            continue;
        }
        if let Err(error) = refresh(&saved) {
            tracing::warn!(
                "could not renew the saved metadata {}: {error}; removing it",
                saved.display()
            );
            let _ = fs::remove_file(&saved);
        }
    }
}

/// Replace `saved` by a file of its own with its bytes and permissions,
/// written now: a plain copy, renamed over it. Not a clone, which some
/// systems give the source's times.
fn refresh(saved: &Path) -> io::Result<()> {
    let staged = saved.with_file_name(STAGED);
    match fs::remove_file(&staged) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let copied = (|| {
        let mut input = File::open(saved)?;
        let permissions = input.metadata()?.permissions();
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        io::copy(&mut input, &mut output)?;
        drop(output);
        fs::set_permissions(&staged, permissions)?;
        fs::rename(&staged, saved)
    })();
    if copied.is_err() {
        let _ = fs::remove_file(&staged);
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// A unit's isolated incremental directory with one finalized session
    /// that saved `bytes` as its metadata, made an hour ago, and its folder
    /// of outputs.
    struct Unit {
        _root: tempfile::TempDir,
        cwd: PathBuf,
        saved: PathBuf,
        rmeta: PathBuf,
    }

    const SESSION: &str = "s-hogr6z70bs-03b1zxc-06p0ajcunkz4karwu8xbm4akh";

    fn unit(bytes: &[u8]) -> Unit {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_path_buf();
        let session = cwd.join("incr/edited-2w6m76gurwh4p").join(SESSION);
        fs::create_dir_all(&session).unwrap();
        fs::create_dir_all(cwd.join("deps")).unwrap();
        let saved = session.join(SAVED);
        fs::write(&saved, bytes).unwrap();
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        filetime::set_file_mtime(&saved, filetime::FileTime::from_system_time(hour_ago)).unwrap();
        let rmeta = cwd.join("deps/libedited-0123456789abcdef.rmeta");
        Unit {
            _root: root,
            cwd,
            saved,
            rmeta,
        }
    }

    fn args() -> Vec<String> {
        ["--crate-name", "edited", "-C", "incremental=incr", "src/lib.rs"]
            .map(String::from)
            .to_vec()
    }

    fn prepare_unit(unit: &Unit, args: &[String], emit: &[&str]) {
        let emit: Vec<String> = emit.iter().map(|kind| kind.to_string()).collect();
        prepare(
            args,
            &unit.cwd,
            Some(&unit.cwd.join("deps")),
            Some("edited"),
            Some("-0123456789abcdef"),
            &emit,
        );
    }

    fn written_now(path: &Path) -> bool {
        let modified = fs::metadata(path).unwrap().modified().unwrap();
        SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age < Duration::from_secs(600))
    }

    #[test]
    fn saved_metadata_the_folder_does_not_hold_is_written_anew() {
        let unit = unit(b"saved metadata");
        let other = unit.cwd.join("donor.rmeta");
        fs::hard_link(&unit.saved, &other).unwrap();
        // The folder holds other metadata (a restore's), or none.
        for held in [Some(&b"restored metadata"[..]), None] {
            if let Some(bytes) = held {
                fs::write(&unit.rmeta, bytes).unwrap();
            }
            prepare_unit(&unit, &args(), &["dep-info", "metadata", "link"]);
            assert_eq!(fs::read(&unit.saved).unwrap(), b"saved metadata");
            assert!(written_now(&unit.saved), "held {held:?}");
            assert_ne!(
                kache_fs::file_identity(&unit.saved).unwrap(),
                kache_fs::file_identity(&other).unwrap(),
                "a file of its own"
            );
            assert!(
                !written_now(&other),
                "another name of the old file keeps its time"
            );
            assert!(!unit.saved.with_file_name(STAGED).exists());
            let _ = fs::remove_file(&unit.rmeta);
            let hour_ago = SystemTime::now() - Duration::from_secs(3600);
            filetime::set_file_mtime(&unit.saved, filetime::FileTime::from_system_time(hour_ago))
                .unwrap();
        }
    }

    #[test]
    fn saved_metadata_the_folder_holds_is_left_as_it_is() {
        let unit = unit(b"saved metadata");
        fs::hard_link(&unit.saved, &unit.rmeta).unwrap();
        let before = kache_fs::file_identity(&unit.saved).unwrap();
        prepare_unit(&unit, &args(), &["dep-info", "metadata", "link"]);
        assert_eq!(kache_fs::file_identity(&unit.saved).unwrap(), before);
        assert!(!written_now(&unit.saved));
    }

    #[test]
    fn only_a_compile_that_writes_metadata_on_a_session_renews_it() {
        let unit = unit(b"saved metadata");
        fs::write(&unit.rmeta, b"restored metadata").unwrap();
        // A program, which writes no metadata of its own.
        prepare_unit(&unit, &args(), &["dep-info", "link"]);
        // No incremental session given to rustc.
        prepare_unit(&unit, &args()[..2], &["dep-info", "metadata", "link"]);
        // Another crate's session.
        let other = ["--crate-name", "other", "-Cincremental=incr"].map(String::from);
        prepare(
            &other,
            &unit.cwd,
            Some(&unit.cwd.join("deps")),
            Some("other"),
            Some("-0123456789abcdef"),
            &["metadata".to_string()],
        );
        assert!(!written_now(&unit.saved));
    }

    #[test]
    fn the_last_incremental_argument_in_any_spelling_names_the_session() {
        let args = [
            "-Cincremental=first",
            "-C",
            "incremental=second",
            "--codegen",
            "opt-level=0",
            "--codegen=incremental=third",
        ]
        .map(String::from);
        assert_eq!(incremental_dir(&args), Some("third"));
        assert_eq!(incremental_dir(&args[..3]), Some("second"));
        assert_eq!(incremental_dir(&args[1..5]), Some("second"));
        assert_eq!(incremental_dir(&args[3..5]), None);
    }
}
