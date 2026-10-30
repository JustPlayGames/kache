//! Key bridges (lanes KACHE-W20..W22): a miss under this release's key
//! adopts the entry an earlier key version stored for the same invocation.
//!
//! The key computation ([`crate::cache_key::KEY_BRIDGES`]) hands each bridged
//! key's legacy twin to [`remember`]; a local lookup that misses asks
//! [`adopt_on_miss`], which has the store verify and re-index the legacy
//! entry under the new key ([`crate::store::Store::adopt_legacy_entry`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// New key -> (legacy version, legacy key, crate name). Bounded: a daemon
/// keys many units; a key is looked up right after it was computed.
/// (legacy version, legacy key, crate name).
type Legacy = (u32, String, String);
static LEGACY: Mutex<Option<HashMap<String, Legacy>>> = Mutex::new(None);
const MAX_REMEMBERED: usize = 65_536;

pub static ADOPTED: AtomicU64 = AtomicU64::new(0);
static ADOPTED_BYTES: AtomicU64 = AtomicU64::new(0);
static VERIFY_US: AtomicU64 = AtomicU64::new(0);
static REGISTER_US: AtomicU64 = AtomicU64::new(0);

fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
pub static NOT_FOUND: AtomicU64 = AtomicU64::new(0);

/// Remember that `key`'s invocation keyed `legacy` under an earlier version.
pub(crate) fn remember(key: &str, version: u32, legacy: &str, crate_name: &str) {
    let mut map = LEGACY.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if map.len() >= MAX_REMEMBERED {
        map.clear();
    }
    map.insert(
        key.to_string(),
        (version, legacy.to_string(), crate_name.to_string()),
    );
}

/// The legacy key `key`'s invocation also keyed, when a bridge applies.
pub(crate) fn legacy_of(key: &str) -> Option<Legacy> {
    LEGACY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(key).cloned())
}

/// The daemon a wrapper hands its verified adoptions to for a batched
/// registration: (socket, the cache directory of the store it serves).
static DAEMON: Mutex<Option<(std::path::PathBuf, std::path::PathBuf)>> = Mutex::new(None);

/// Adoptions in `cache_dir` go to the daemon at `socket` to register
/// (`crate::daemon::send_adoption`); set where the wrapper hands its hit
/// stamps over (Unix: elsewhere the wrapper registers itself).
#[cfg(unix)]
pub(crate) fn register_through_daemon(socket: std::path::PathBuf, cache_dir: std::path::PathBuf) {
    *DAEMON.lock().unwrap_or_else(|e| e.into_inner()) = Some((socket, cache_dir));
}

pub static CONTENDED: AtomicU64 = AtomicU64::new(0);

/// Register `request` through the daemon serving `store`, when one takes
/// it: `lock` goes first, the daemon takes the key's lock itself and writes
/// the row later ([`crate::store::Registered::Deferred`]). Else in `store`
/// itself, under `lock`.
fn register(
    store: &crate::store::Store,
    request: &crate::store::AdoptionRequest,
    lock: crate::store::KeyLock,
) -> anyhow::Result<crate::store::Registered> {
    let daemon = DAEMON.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some((socket, cache_dir)) = daemon
        && cache_dir == store.cache_dir()
    {
        // The daemon claims the key's lock before it answers; a peer that
        // takes it in between adopts or stores the key itself.
        drop(lock);
        if crate::daemon::send_adoption(&socket, &cache_dir, request) {
            return Ok(crate::store::Registered::Deferred);
        }
        // Not taken: register here. The transaction checks the key again,
        // so a peer's commit meanwhile is left as it is.
        return Ok(store.register_adoptions(std::slice::from_ref(request))?[0]);
    }
    let registered = store.register_adoptions(std::slice::from_ref(request))?[0];
    drop(lock);
    Ok(registered)
}

/// After `cache_key` missed in `store`: adopt its legacy twin's entry, if one
/// is remembered and the store has it. The entry to restore from: the
/// verified legacy entry under `cache_key`, whether its row is written yet
/// or handed to the daemon to write ([`register`]); or a peer's entry
/// committed meanwhile. `None` when there is nothing to adopt, and also when
/// another process holds the key (adopting or compiling it): the caller's
/// claim then waits for its commit.
pub(crate) fn adopt_on_miss(
    store: &crate::store::Store,
    cache_key: &str,
) -> Option<crate::store::EntryMeta> {
    let (version, legacy, crate_name) = legacy_of(cache_key)?;
    let adopted =
        store.adopt_legacy_entry_with(&legacy, version, cache_key, &crate_name, |request, lock| {
            register(store, request, lock)
        });
    match adopted {
        Ok(Some(adoption)) if adoption.outcome == crate::store::AdoptionOutcome::Contended => {
            CONTENDED.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("key bridge: {crate_name} is held by another process; waiting for it");
            None
        }
        Ok(Some(adoption)) => {
            let by_peer = adoption.meta.is_none();
            let adopted = if by_peer {
                ADOPTED.load(Ordering::Relaxed)
            } else {
                ADOPTED.fetch_add(1, Ordering::Relaxed) + 1
            };
            let bytes = ADOPTED_BYTES.fetch_add(adoption.bytes, Ordering::Relaxed) + adoption.bytes;
            let verify_us = VERIFY_US.fetch_add(micros(adoption.verify), Ordering::Relaxed)
                + micros(adoption.verify);
            let register_us = REGISTER_US.fetch_add(micros(adoption.register), Ordering::Relaxed)
                + micros(adoption.register);
            tracing::debug!(
                "key bridge: adopted v{version} entry for {crate_name}: {} files, {} bytes \
                 re-hashed in {:.1} ms, registered in {:.1} ms{}",
                adoption.files,
                adoption.bytes,
                adoption.verify.as_secs_f64() * 1e3,
                adoption.register.as_secs_f64() * 1e3,
                match adoption.outcome {
                    _ if by_peer => " (by a peer)",
                    crate::store::AdoptionOutcome::Deferred => " (handed to the daemon)",
                    _ => "",
                }
            );
            if !by_peer && adopted.is_multiple_of(100) {
                tracing::info!(
                    "key bridge: {adopted} entries of an earlier key version adopted, {bytes} \
                     bytes re-hashed in {:.1} s, {:.1} s registering",
                    verify_us as f64 / 1e6,
                    register_us as f64 / 1e6
                );
            }
            match adoption.meta {
                Some(meta) => Some(meta),
                None => store.get(cache_key).ok().flatten(),
            }
        }
        Ok(None) => {
            NOT_FOUND.fetch_add(1, Ordering::Relaxed);
            None
        }
        Err(error) => {
            tracing::warn!("key bridge: adopting for {crate_name} failed: {error:#}");
            None
        }
    }
}
