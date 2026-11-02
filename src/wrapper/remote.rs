//! Remote coordination shared by compiler wrappers. Restoration stays compiler-owned.

use crate::config::Config;
use crate::events::EventResult;
use crate::store::{EntryMeta, Store};
use std::path::Path;

pub(super) fn compiler_remote_enabled(config: &Config, publishes_to_remote: bool) -> bool {
    publishes_to_remote && config.remote.is_some()
}

pub(super) fn compiler_upload_enabled(config: &Config, publishes_to_remote: bool) -> bool {
    compiler_remote_enabled(config, publishes_to_remote) && !config.remote_readonly
}

pub(super) fn maybe_enqueue_upload(
    config: &Config,
    store: &Store,
    cache_key: &str,
    crate_name: &str,
    publishes_to_remote: bool,
) {
    if !compiler_remote_enabled(config, publishes_to_remote) {
        return;
    }
    let entry_dir = store.entry_dir(cache_key);
    if let Err(e) = crate::daemon::send_upload_job(config, cache_key, &entry_dir, crate_name) {
        tracing::warn!("failed to send upload job to daemon: {e}");
    }
}

/// Preserve each compiler path's policy for an entry published during the wait.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NegativeReply {
    ContinueCompile,
    CheckConcurrentEntry,
}

/// Reload validated metadata after the daemon's reply. The result describes
/// availability, not a completed hit; callers must restore before reporting it.
pub(super) fn acquire_entry(
    config: &Config,
    store: &Store,
    cache_key: &str,
    crate_name: &str,
    negative_reply: NegativeReply,
) -> Option<(EntryMeta, EventResult)> {
    config.remote.as_ref()?;
    let entry_dir = store.entry_dir(cache_key);
    let shard_dir = crate::daemon::remote_check_shard_dir_arg(&config.cache_dir, store.cache_dir());
    crate::demand::record(cache_key);
    let waiting = std::time::Instant::now();
    let reply = crate::daemon::send_remote_check(
        config,
        cache_key,
        &entry_dir,
        crate_name,
        shard_dir.as_deref().map(Path::new),
    );
    crate::demand::remote_wait(cache_key, waiting.elapsed());
    let reply = reply?;
    if !reply.found && negative_reply == NegativeReply::ContinueCompile {
        return None;
    }
    let meta = store.get(cache_key).ok()??;
    let origin = if !reply.found {
        // A negative remote reply cannot establish remote or prefetch provenance.
        EventResult::LocalHit
    } else if reply.prefetched {
        EventResult::PrefetchHit
    } else {
        EventResult::RemoteHit
    };
    tracing::debug!(
        crate_name,
        cache_key,
        ?origin,
        "entry available after remote check"
    );
    Some((meta, origin))
}

/// After the remote missed `cache_key`: ask it for the legacy key a key
/// bridge computed for the same invocation (lane KACHE-W23). The daemon
/// downloads and verifies it as any entry (key, crate and every blob's
/// content address) and imports it under the legacy key; the local store
/// then adopts it as it adopts a local legacy entry, under the key's build
/// lock and after re-hashing its blobs ([`crate::key_bridge::adopt_on_miss`]).
/// Restore from the entry this returns.
pub(super) fn acquire_legacy_entry(
    config: &Config,
    store: &Store,
    cache_key: &str,
) -> Option<(EntryMeta, EventResult)> {
    config.remote.as_ref()?;
    let (_, legacy, crate_name) = crate::key_bridge::legacy_of(cache_key)?;
    let (_, origin) = acquire_entry(
        config,
        store,
        &legacy,
        &crate_name,
        NegativeReply::ContinueCompile,
    )?;
    let meta = crate::key_bridge::adopt_on_miss(store, cache_key)?;
    tracing::debug!(
        crate_name,
        cache_key,
        ?origin,
        "adopted an earlier key version's remote entry"
    );
    Some((meta, origin))
}
