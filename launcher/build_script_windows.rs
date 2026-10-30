//! The program Cargo runs on Windows in place of a build script whose run
//! kache caches: the Windows counterpart of `build_script.rs` beside it.
//!
//! kache's own build script compiles this file when kache is built for a
//! Windows target, and `src/build_script.rs` embeds the result and copies it
//! over every build script it caches. Windows has no `exec`, so it starts the
//! pinned kache as a child with the same arguments, standard handles and
//! environment (plus `KACHE_BUILD_SCRIPT_PATH`, the path Cargo ran), waits
//! for it, and exits with its code. The environment is passed on as a whole,
//! so Cargo's `DEP_<LINKS>_<KEY>` variables arrive whatever their spelling.
//!
//! It reads `.kache-launch` beside itself, written by kache at install time:
//! the preserved script's file name, the pinned kache's path relative to the
//! profile directory, and the way up from this directory to the profile
//! directory, each NUL-terminated (UTF-8). When that kache is gone (a pruned
//! or partially restored target directory) it runs the preserved script
//! directly: uncached is always acceptable.

use std::path::{Path, PathBuf};
use std::process::Command;

const SHIM_PATH_ENV: &str = "KACHE_BUILD_SCRIPT_PATH";
const LAUNCH_RECORD: &str = ".kache-launch";

fn main() {
    match launch() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("kache: build-script launcher: {message}");
            std::process::exit(127)
        }
    }
}

fn launch() -> Result<i32, String> {
    let invoked = std::env::current_exe().map_err(|error| format!("locating itself: {error}"))?;
    let directory = invoked
        .parent()
        .ok_or("its own path has no directory")?
        .to_path_buf();
    let record = std::fs::read(directory.join(LAUNCH_RECORD))
        .map_err(|error| format!("reading {LAUNCH_RECORD}: {error}"))?;
    let (real, kache) = targets(&directory, &record)?;
    let arguments = std::env::args_os().skip(1);
    // Resolve the relative hops before CreateProcess: they can push the
    // spelling past MAX_PATH, and verbatim Windows paths cannot contain them.
    let status = match kache
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path.is_file())
    {
        Some(kache) => Command::new(kache)
            .args(arguments)
            .env(SHIM_PATH_ENV, &invoked)
            .status(),
        None => Command::new(&real).args(arguments).status(),
    }
    .map_err(|error| format!("starting the build script: {error}"))?;
    // The child's whole exit code, an NTSTATUS such as an access violation
    // included.
    Ok(status.code().unwrap_or(1))
}

/// The preserved script and the pinned kache named by a launch record read
/// from `directory`.
fn targets(directory: &Path, record: &[u8]) -> Result<(PathBuf, Option<PathBuf>), String> {
    let text = std::str::from_utf8(record).map_err(|_| "the launch record is not UTF-8")?;
    let mut fields = text.split('\0');
    let (Some(real_name), Some(kache_relative)) = (fields.next(), fields.next()) else {
        return Err("the launch record is incomplete".into());
    };
    if real_name.is_empty() {
        return Err("the launch record names no script".into());
    }
    let to_profile = fields.next().filter(|field| !field.is_empty());
    let kache = match to_profile {
        Some(to_profile) if !kache_relative.is_empty() => {
            Some(directory.join(to_profile).join(kache_relative))
        }
        _ => None,
    };
    Ok((directory.join(real_name), kache))
}
