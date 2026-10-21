//! A finished rustc compile's store, finished by the daemon when the index
//! is busy (`cache.daemon_publish_rustc`).
//!
//! A store sits between rustc's exit and the wrapper's return, so every
//! unit's dependents wait for it. On a quiet index a store is staging and
//! hashing plus a write transaction of a few milliseconds, and the wrapper
//! does it all, as it always did: the entry is visible when it returns. The
//! slow stores are the ones that queue for the index's write lock behind a
//! burst of other stores or a sweep (on a shared build machine over a day:
//! p50 7 ms, p99 3.9 s, max 22 s). Only that wait goes to the daemon:
//!
//! - The wrapper stages, hashes and publishes every output as a blob (a
//!   put's Phase 1), then asks for the write lock once with a short wait
//!   ([`INDEX_BUSY_WAIT`]). Granted, it registers the entry itself.
//! - Still held elsewhere, it links each blob into a request directory under
//!   the store's staging area and offers the staged entry to the daemon in a
//!   `PublishRustc` request. Nothing is staged or hashed again for it.
//! - The daemon takes the key's lock before it accepts, so a peer that looks
//!   the key up meanwhile finds no entry, then the key contended, and waits
//!   for the commit as it would for a wrapper still storing. The blobs are
//!   unreferenced until then, so no peer can find a partial entry.
//! - The daemon registers the entry with `Store::put_staged_handed_off`,
//!   which hashes each link first and commits nothing if one differs: a blob
//!   sharing its inode with an output rewritten in place after the wrapper
//!   returned costs the entry, never its correctness.
//! - Anything that keeps the daemon from accepting (no daemon, an older
//!   daemon, a full queue, a busy daemon past the hand-off budget) leaves
//!   the registration to the wrapper, which then waits for the lock as a put
//!   does, with the key lock taken back. A daemon that dies with the entry
//!   queued commits nothing: its blobs are orphans for the orphan sweep and
//!   its request directory is the handoff sweep's.
//!
//! Windows keeps the synchronous store; the hand-off has not been tried
//! there.

use std::path::{Path, PathBuf};

use crate::args::RustcArgs;
use crate::compiler::rustc::RustcCompiler;
use crate::compiler::{Artifact, ArtifactKind};
use crate::config::Config;
use crate::daemon_publish::{
    Handoff, PrestageHint, PublishCcRequest, RustcOutput, RustcPublish, StagedPublish,
};
use crate::events::EventResult;
use crate::store::{KeyLock, PutAttempt, StagedPut, Store, StorePutResult};

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

/// How long a rustc miss waits for the index's write lock before it hands
/// the registration to the daemon. A quiet index's writers hold the lock for
/// a few milliseconds, so a store that finds it held only by one of them
/// still registers here, and waits that long at most otherwise.
pub(super) const INDEX_BUSY_WAIT: std::time::Duration = std::time::Duration::from_millis(5);

/// How long a wrapper whose index is busy waits for the daemon to take the
/// staged entry. Longer than a C/C++ hand-off's budget: the wrapper would
/// otherwise wait for a lock another writer holds, for seconds under a burst
/// of stores, and a burst is when hundreds of wrappers hand off at once and
/// the daemon answers them in turn.
pub(super) const BUSY_HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether a busy index may hand this compile's registration to the daemon:
/// the setting is on, the entry goes to the main store (a `[cache.volumes]`
/// shard stays with the wrapper, as for C/C++), and a daemon holds its run
/// lock.
pub(super) fn wanted(config: &Config, store: &Store) -> bool {
    cfg!(unix)
        && config.daemon_publish_rustc
        && super::volume_cache_dirs_match(store.cache_dir(), &config.cache_dir)
        && crate::daemon::existing_daemon_run_lock_is_held(&config.socket_path()).unwrap_or(false)
}

/// What became of a rustc miss's store.
pub(super) enum RustcStore {
    /// Stored here, or failed here: the caller finishes as after a put.
    Stored(anyhow::Result<StorePutResult>),
    /// The daemon took the entry (or a peer stored the key meanwhile); the
    /// event is whoever stores it's.
    HandedOff,
}

/// Store a rustc miss: here when the index lets it register at once, else
/// by the daemon when it takes the staged entry, else here after all.
/// `build_lock` is the key's lock; after `HandedOff` it is the daemon's.
pub(super) fn store(
    config: &Config,
    store: &Store,
    build_lock: &mut Option<KeyLock>,
    handoff: RustcHandoff<'_>,
) -> RustcStore {
    store_within(
        config,
        store,
        build_lock,
        handoff,
        BUSY_HANDOFF_BUDGET,
        wanted(config, store),
    )
}

/// [`store`] with the wait for the daemon's answer, and whether to offer
/// it at all, supplied.
fn store_within(
    config: &Config,
    store: &Store,
    build_lock: &mut Option<KeyLock>,
    handoff: RustcHandoff<'_>,
    budget: std::time::Duration,
    hand_off_when_busy: bool,
) -> RustcStore {
    let (args, files) = (handoff.args, handoff.files);
    if !hand_off_when_busy {
        return RustcStore::Stored(store.put_with_compile_time(
            handoff.cache_key,
            handoff.crate_name,
            &args.crate_types,
            &args.features,
            handoff.target,
            handoff.profile,
            files,
            handoff.stdout,
            handoff.stderr,
            handoff.compile_time_ms,
        ));
    }
    let staged = match store.put_with_compile_time_unless_busy(
        handoff.cache_key,
        handoff.crate_name,
        &args.crate_types,
        &args.features,
        handoff.target,
        handoff.profile,
        files,
        handoff.stdout,
        handoff.stderr,
        handoff.compile_time_ms,
        INDEX_BUSY_WAIT,
    ) {
        Ok(PutAttempt::Stored(result)) => return RustcStore::Stored(Ok(result)),
        Ok(PutAttempt::Busy(staged)) => staged,
        Err(error) => return RustcStore::Stored(Err(error)),
    };
    match hand_off_staged(config, store, build_lock, handoff, &staged, budget) {
        CcHandoffOutcome::Accepted | CcHandoffOutcome::Done => RustcStore::HandedOff,
        CcHandoffOutcome::Publish => RustcStore::Stored(store.register_staged_put(&staged)),
    }
}

/// Offer a staged entry the index was too busy to register to the daemon.
/// `Accepted`: the daemon holds the key and registers the entry and its
/// event. `Done`: a peer took the key after the daemon declined; its entry
/// counts and the event is written. `Publish`: register it here, with the
/// key lock back in `build_lock` when it could be taken.
fn hand_off_staged(
    config: &Config,
    store: &Store,
    build_lock: &mut Option<KeyLock>,
    handoff: RustcHandoff<'_>,
    staged: &StagedPut,
    budget: std::time::Duration,
) -> CcHandoffOutcome {
    let stored = staged.files();
    if handoff.artifacts.len() != handoff.files.len() || stored.len() != handoff.files.len() {
        return CcHandoffOutcome::Publish;
    }
    // As the synchronous path does after its put, so the memo rows the
    // daemon seeds carry the date Cargo leaves.
    let redated = redate_check_outputs(handoff.args, handoff.files);
    let blobs: Vec<(PathBuf, String)> = stored
        .iter()
        .map(|file| (store.blob_path(&file.hash), file.name.clone()))
        .collect();
    let snapshots = match crate::daemon_publish::snapshot_staged_blobs(config, &blobs) {
        Ok(snapshots) => snapshots,
        Err(error) => {
            tracing::debug!("rustc hand-off: could not link the staged blobs: {error:#}");
            return CcHandoffOutcome::Publish;
        }
    };
    let outputs = handoff
        .artifacts
        .iter()
        .zip(handoff.files)
        .zip(stored)
        .zip(redated)
        .map(|(((artifact, (path, _)), file), redated)| RustcOutput {
            hash: file.hash.clone(),
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
    let result = staged.result();
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
            staged: Some(StagedPublish {
                files: stored.to_vec(),
                output_blobs: result.output_blobs,
                duplicate_blobs: result.duplicate_blobs,
                new_blobs: result.new_blobs,
            }),
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
/// fingerprint the daemon seeds is the one Cargo leaves. One flag per file: whether it
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
