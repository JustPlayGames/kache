//! Key bridges (lane KACHE-W20 prototype): a miss under this release's key
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

fn legacy_of(key: &str) -> Option<Legacy> {
    LEGACY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|map| map.get(key).cloned())
}

/// After `cache_key` missed in `store`: adopt its legacy twin's entry, if one
/// is remembered and the store has it. `true` when `cache_key` now has an
/// entry to look up again.
pub(crate) fn adopt_on_miss(store: &crate::store::Store, cache_key: &str) -> bool {
    let Some((version, legacy, crate_name)) = legacy_of(cache_key) else {
        return false;
    };
    match store.adopt_legacy_entry(&legacy, version, cache_key, &crate_name) {
        Ok(Some(adoption)) => {
            let adopted = ADOPTED.fetch_add(1, Ordering::Relaxed) + 1;
            let bytes =
                ADOPTED_BYTES.fetch_add(adoption.bytes, Ordering::Relaxed) + adoption.bytes;
            let verify_us = VERIFY_US.fetch_add(micros(adoption.verify), Ordering::Relaxed)
                + micros(adoption.verify);
            let register_us = REGISTER_US
                .fetch_add(micros(adoption.register), Ordering::Relaxed)
                + micros(adoption.register);
            tracing::debug!(
                "key bridge: adopted v{version} entry for {crate_name}: {} files, {} bytes \
                 re-hashed in {:.1} ms, registered in {:.1} ms{}",
                adoption.files,
                adoption.bytes,
                adoption.verify.as_secs_f64() * 1e3,
                adoption.register.as_secs_f64() * 1e3,
                if adoption.by_peer { " (by a peer)" } else { "" }
            );
            if adopted.is_multiple_of(100) {
                tracing::info!(
                    "key bridge: {adopted} entries of an earlier key version adopted, {bytes} \
                     bytes re-hashed in {:.1} s, {:.1} s registering",
                    verify_us as f64 / 1e6,
                    register_us as f64 / 1e6
                );
            }
            true
        }
        Ok(None) => {
            NOT_FOUND.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(error) => {
            tracing::warn!("key bridge: adopting for {crate_name} failed: {error:#}");
            false
        }
    }
}
