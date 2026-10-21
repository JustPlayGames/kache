//! A finished rustc compile's store, handed to the daemon
//! (`cache.daemon_publish_rustc`).
//!
//! A store sits between rustc's exit and the wrapper's return, so every
//! unit's dependents wait for it. Most stores take milliseconds, but one
//! that queues for the index's write lock, or copies a large binary on a
//! busy disk, takes seconds (on a shared build machine over a day: p50 7 ms,
//! p99 3.9 s, max 22 s). The wrapper therefore does only what fixes the
//! entry's content and hands the rest to the daemon, as the C/C++ path does
//! ([`crate::daemon_publish`]):
//!
//! - It hardlinks each output into a request directory under the store's
//!   staging area and reads each snapshot's digest. That is the address the
//!   entry gets: the bytes rustc wrote, read before Cargo or anything else
//!   has had the file.
//! - The daemon takes the key's lock before it accepts, so a peer that looks
//!   the key up meanwhile finds no entry, then the key contended, and waits
//!   for the commit as it would for a wrapper still storing.
//! - The daemon stores the snapshots with `Store::put_handed_off`, which
//!   hashes each again once it is staged (and read-only, when linked) and
//!   commits nothing if one differs: an output rewritten in place after the
//!   wrapper returned (the link shares its inode) costs the entry, never its
//!   correctness. An output replaced by a new file leaves the link's inode,
//!   and so the entry, as rustc wrote it.
//! - Anything that keeps the daemon from accepting (no daemon, an older
//!   daemon, a full queue, a busy daemon past the hand-off budget) falls
//!   back to the wrapper's own store, with the key lock taken back.
//!
//! Windows keeps the synchronous store; the hand-off has not been tried
//! there.

use std::path::{Path, PathBuf};

use crate::args::RustcArgs;
use crate::compiler::rustc::RustcCompiler;
use crate::compiler::{Artifact, ArtifactKind};
use crate::config::Config;
use crate::daemon_publish::{Handoff, PrestageHint, PublishCcRequest, RustcOutput, RustcPublish};
use crate::events::EventResult;
use crate::store::{KeyLock, Store};

use super::{CcHandoffOutcome, EventInputs};

/// What the daemon needs to store a finished rustc compile and log its
/// event.
pub(super) struct RustcHandoff<'a> {
    pub(super) cache_key: &'a str,
    pub(super) crate_name: &'a str,
    pub(super) target: &'a str,
    pub(super) profile: &'a str,
    pub(super) args: &'a RustcArgs,
    pub(super) compiler: &'a RustcCompiler,
    /// The compile's artifacts, in the order of `files`.
    pub(super) artifacts: &'a [Artifact],
    /// `(path, store name)` as the synchronous put would read them.
    pub(super) files: &'a [(PathBuf, String)],
    pub(super) stdout: &'a str,
    pub(super) stderr: &'a str,
    pub(super) compile_time_ms: u64,
    /// The event the synchronous path would log, without its store fields.
    pub(super) event: EventInputs<'a>,
    pub(super) size: u64,
    pub(super) start: std::time::Instant,
    pub(super) store_start: std::time::Instant,
}

/// Whether to offer this compile's store to the daemon: the setting is on,
/// the entry goes to the main store (a `[cache.volumes]` shard stays with
/// the wrapper, as for C/C++), and a daemon holds its run lock.
pub(super) fn wanted(config: &Config, store: &Store) -> bool {
    cfg!(unix)
        && config.daemon_publish_rustc
        && super::volume_cache_dirs_match(store.cache_dir(), &config.cache_dir)
        && crate::daemon::existing_daemon_run_lock_is_held(&config.socket_path()).unwrap_or(false)
}

/// Offer the compile to the daemon. `Accepted`: the daemon holds the key and
/// stores the entry and its event. `Done`: a peer took the key after the
/// daemon declined; its entry counts and the event is written. `Publish`:
/// store here, with the key lock back in `build_lock` when it could be taken.
pub(super) fn hand_off(
    config: &Config,
    store: &Store,
    build_lock: &mut Option<KeyLock>,
    handoff: RustcHandoff<'_>,
) -> CcHandoffOutcome {
    hand_off_within(
        config,
        store,
        build_lock,
        handoff,
        crate::daemon_publish::PUBLISH_HANDOFF_TIMEOUT,
    )
}

/// [`hand_off`] with the wait for the daemon's answer supplied.
fn hand_off_within(
    config: &Config,
    store: &Store,
    build_lock: &mut Option<KeyLock>,
    handoff: RustcHandoff<'_>,
    budget: std::time::Duration,
) -> CcHandoffOutcome {
    if handoff.artifacts.len() != handoff.files.len() {
        return CcHandoffOutcome::Publish;
    }
    let redated = redate_check_outputs(handoff.args, handoff.files);
    let (snapshots, digests) =
        match crate::daemon_publish::snapshot_rustc_outputs(config, handoff.files) {
            Ok(snapshots) => snapshots,
            Err(error) => {
                tracing::debug!("rustc hand-off: could not snapshot outputs: {error:#}");
                return CcHandoffOutcome::Publish;
            }
        };
    let outputs = handoff
        .artifacts
        .iter()
        .zip(handoff.files)
        .zip(digests)
        .zip(redated)
        .map(|(((artifact, (path, _)), hash), redated)| RustcOutput {
            hash,
            output: compiler_owned(artifact, path).then(|| path.to_string_lossy().into_owned()),
            redated,
        })
        .collect();
    let prestage = prestage_hints(handoff.compiler, handoff.args, handoff.files);
    let elapsed = handoff.start.elapsed().as_millis() as u64;
    let mut event = super::build_event_details(
        config,
        handoff
            .event
            .store_ms(handoff.store_start.elapsed().as_millis() as u64),
    );
    event.elapsed_ms = elapsed;
    let request = PublishCcRequest {
        client_epoch: crate::daemon::build_epoch(),
        client_version: Some(crate::VERSION.to_owned()),
        cache_key: handoff.cache_key.to_string(),
        crate_name: handoff.crate_name.to_string(),
        target: handoff.target.to_string(),
        files: snapshots,
        stdout: handoff.stdout.to_string(),
        stderr: handoff.stderr.to_string(),
        compile_time_ms: handoff.compile_time_ms,
        publishes_to_remote: super::compiler_upload_enabled(config, true),
        event,
        memo: None,
        rustc: Some(RustcPublish {
            crate_types: handoff.args.crate_types.clone(),
            features: handoff.args.features.clone(),
            profile: handoff.profile.to_string(),
            outputs,
            unit: handoff.args.get_codegen_opt("metadata").map(str::to_string),
            prestage,
        }),
    };
    // The daemon takes the key itself before it answers; ours must be gone
    // first, since a file lock cannot be shared across processes.
    *build_lock = None;
    match crate::daemon_publish::hand_off_cc_publish_within(config, &request, budget) {
        Handoff::Accepted => {
            super::print_progress(handoff.crate_name, EventResult::Miss, elapsed, handoff.size);
            CcHandoffOutcome::Accepted
        }
        Handoff::Declined(reason) => {
            tracing::debug!(
                "rustc hand-off declined for {}: {reason}",
                handoff.crate_name
            );
            crate::daemon_publish::remove_handoff_files(&request.files);
            super::resolve_declined_handoff(
                config,
                store,
                build_lock,
                request.event,
                handoff.cache_key,
                handoff.crate_name,
                handoff.start,
                handoff.size,
            )
        }
    }
}

/// Whether `path` is the compiler's own output for `artifact`, whose memo the
/// daemon may seed, rather than a file of the wrapper's (normalized dep-info,
/// a debug bundle).
fn compiler_owned(artifact: &Artifact, path: &Path) -> bool {
    !matches!(
        artifact.kind,
        ArtifactKind::DepInfo | ArtifactKind::DebugBundle
    ) && artifact.path == path
}

/// For a check unit, set each `.rmeta`'s mtime to the invocation time Cargo
/// is about to give it, as the synchronous path does after its put, so the
/// stored fingerprint is the one Cargo leaves. One flag per file: whether it
/// now carries that date.
fn redate_check_outputs(args: &RustcArgs, files: &[(PathBuf, String)]) -> Vec<bool> {
    let invoked = super::cargo_check_redate_ns(args);
    files
        .iter()
        .map(|(path, _)| {
            invoked.is_some_and(|invoked| super::is_rmeta(path) && redate(path, invoked))
        })
        .collect()
}

fn redate(path: &Path, invoked_ns: i64) -> bool {
    let Ok(before) = crate::cache_key::FileFingerprint::from_path(path) else {
        return false;
    };
    if before.mtime_ns == invoked_ns {
        return true;
    }
    let seconds = invoked_ns.div_euclid(1_000_000_000);
    let nanos = invoked_ns.rem_euclid(1_000_000_000) as u32;
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, nanos)).is_ok()
}

/// The large executables of this compile the daemon records for
/// [`crate::prestage`] once they are stored, as the synchronous path's
/// `remember_prestaged_executables` does.
fn prestage_hints(
    compiler: &RustcCompiler,
    args: &RustcArgs,
    files: &[(PathBuf, String)],
) -> Vec<PrestageHint> {
    if !super::stores_prestage_candidate(files) {
        return Vec::new();
    }
    let (Some(output_dir), Some(target_dir)) = (
        super::rustc_output_dir(args),
        super::prestage_target_dir(args),
    ) else {
        return Vec::new();
    };
    let shared = super::shared_inode_loadable(
        args,
        crate::compiler::platform::current().may_share_restored_loadables(),
    );
    files
        .iter()
        .filter(|(path, name)| {
            std::fs::metadata(path).is_ok_and(|meta| {
                super::prestages(
                    compiler,
                    args,
                    name,
                    meta.len(),
                    kache_store::filesystem::is_executable(&meta),
                    shared,
                )
            })
        })
        .map(|(_, name)| PrestageHint {
            target_dir: target_dir.to_string_lossy().into_owned(),
            dest: super::artifact_target_path(args, &output_dir, name)
                .to_string_lossy()
                .into_owned(),
            store_name: name.clone(),
        })
        .collect()
}

// The hand-off is Unix only, and so are its tests.
#[cfg(all(test, unix))]
mod tests;
