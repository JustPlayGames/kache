//! Background store eviction when the filesystem runs short of space.
//! Wrapper hints and post-upload checks run this without waiting for idle
//! builds. Target cleanup keeps its separate quiet-machine rules.

use crate::config::Config;
use crate::store::Store;
use anyhow::Result;
use kache_fs::VolumeUsage;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(300);
/// Longest one pass spends removing entries. Only the removals count: the
/// candidate scan before them takes 16-73 s on an index of 12 GB, and a
/// clock started before it ended every pass before its first removal.
const REMOVAL_CAP: Duration = Duration::from_secs(60);
/// A pass goes on until free space is this many floors, so one pass buys
/// room for the builds that filled the disk instead of stopping a byte
/// above the floor.
const GOAL_FLOORS: u64 = 3;
const LAST_CHECK: &str = "disk-recovery.last";

/// When this process last recovered each store whose timestamp it could not
/// write, by cache dir. On a full disk every write of the timestamp fails;
/// this keeps the process to the same interval.
static UNRECORDED: Mutex<BTreeMap<PathBuf, u64>> = Mutex::new(BTreeMap::new());

fn last_check(cache_dir: &Path) -> Result<u64> {
    match std::fs::read_to_string(cache_dir.join(LAST_CHECK)) {
        Ok(text) => Ok(text.trim().parse()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn unrecorded() -> MutexGuard<'static, BTreeMap<PathBuf, u64>> {
    UNRECORDED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// This process's last recovery of `cache_dir` without a timestamp, `0`
/// when there is none.
fn last_unrecorded(cache_dir: &Path) -> u64 {
    unrecorded().get(cache_dir).copied().unwrap_or(0)
}

fn due(last: u64, now: u64) -> bool {
    last == 0 || now < last || now - last >= INTERVAL.as_secs()
}

fn below_floor(floor: u64, usage: Option<VolumeUsage>) -> bool {
    usage.is_some_and(|usage| floor <= usage.total && usage.free < floor)
}

/// Free space under which a volume counts as running low: twice the floor,
/// or 2.5 % of the volume when that is more (46 GiB of 1.8 TiB with the
/// default floor). Builds that fill a disk at 3 GiB a minute cross it a
/// quarter of an hour before the floor. `0`, never low, without a floor.
fn low_band(floor: u64, total: u64) -> u64 {
    if floor == 0 {
        return 0;
    }
    floor.saturating_mul(2).max(total / 40)
}

fn below_low_band(floor: u64, usage: Option<VolumeUsage>) -> bool {
    usage.is_some_and(|usage| floor <= usage.total && usage.free < low_band(floor, usage.total))
}

/// Whether the volume of the store of `config` is running low on free
/// space (see [`low_band`]). Automatic eviction then starts without waiting
/// out the backoff a sweep left. Off with automatic GC or the floor.
pub(crate) fn low_on_disk(config: &Config) -> bool {
    low_on_disk_at(config, &config.cache_dir)
}

/// [`low_on_disk`] for the volume holding `path`, with the floor of
/// `config`.
pub(crate) fn low_on_disk_at(config: &Config, path: &Path) -> bool {
    config.auto_gc
        && below_low_band(
            config.auto_recover_min_free_bytes,
            kache_fs::volume_usage(path),
        )
}

/// Whether a pass that has spent `walked` removing entries goes on: under
/// `cap`, with free space short of [`GOAL_FLOORS`] floors.
fn keep_recovering(
    floor: u64,
    walked: Duration,
    cap: Duration,
    usage: Option<VolumeUsage>,
) -> bool {
    walked < cap
        && usage.is_some_and(|usage| {
            floor <= usage.total && usage.free < floor.saturating_mul(GOAL_FLOORS)
        })
}

/// Cheap check shared by wrappers and collectors. An unreadable timestamp
/// or volume skips recovery. The collector checks again under `gc.lock`.
pub(crate) fn wanted(config: &Config) -> bool {
    if !config.auto_gc || config.auto_recover_min_free_bytes == 0 {
        return false;
    }
    let now = crate::maintenance::unix_now_secs();
    last_check(&config.cache_dir).is_ok_and(|last| due(last, now))
        && due(last_unrecorded(&config.cache_dir), now)
        && below_floor(
            config.auto_recover_min_free_bytes,
            kache_fs::volume_usage(&config.cache_dir),
        )
}

/// The daemon and detached worker share the lock and timestamp. No work is
/// done while another collector owns this store. Never propagates a failure
/// to the build that asked for recovery.
pub(crate) fn run(config: &Config) {
    if let Err(error) = recover(config) {
        tracing::warn!(store = %config.cache_dir.display(), "disk recovery failed: {error:#}");
    }
}

fn recover(config: &Config) -> Result<()> {
    recover_with(config, REMOVAL_CAP, |path, now| {
        crate::atomic::atomic_replace(path, now.to_string().as_bytes())
    })
}

/// [`recover`], removing for at most `cap` and writing the timestamp with
/// `record`.
fn recover_with(
    config: &Config,
    cap: Duration,
    record: impl FnOnce(&Path, u64) -> Result<()>,
) -> Result<()> {
    if !wanted(config) {
        return Ok(());
    }
    let store = Store::open(config)?;
    let Some(_lock) = store.try_gc_lock()? else {
        return Ok(());
    };
    if !wanted(config) {
        return Ok(());
    }
    let now = crate::maintenance::unix_now_secs();
    // A full disk refuses the timestamp just when space is needed most:
    // evict anyway, and pace this process from memory instead.
    if let Err(error) = record(&config.cache_dir.join(LAST_CHECK), now) {
        tracing::warn!(
            store = %config.cache_dir.display(),
            "cannot record the disk recovery time, recovering anyway: {error:#}"
        );
        unrecorded().insert(config.cache_dir.clone(), now);
    }
    let started = Instant::now();
    let mut stats = store.evict_for_disk_pressure(|walked| {
        !keep_recovering(
            config.auto_recover_min_free_bytes,
            walked,
            cap,
            kache_fs::volume_usage(&config.cache_dir),
        )
    })?;
    stats.duration_ms = started.elapsed().as_millis() as u64;
    crate::report::record_gc_run(
        config,
        "disk-recovery",
        crate::store::SweepOrigin::Automatic,
        &stats,
    )?;
    tracing::info!(
        store = %config.cache_dir.display(),
        entries = stats.entries_evicted,
        elapsed_ms = stats.duration_ms,
        "disk recovery finished"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_requires_a_valid_floor_and_a_readable_volume() {
        let usage = Some(VolumeUsage {
            total: 100,
            free: 10,
        });
        assert!(below_floor(11, usage));
        assert!(below_floor(100, usage));
        assert!(!below_floor(10, usage));
        assert!(!below_floor(9, usage));
        assert!(!below_floor(101, usage));
        assert!(!below_floor(
            0,
            Some(VolumeUsage {
                total: 100,
                free: 0
            })
        ));
        assert!(!below_floor(11, None));
    }

    #[test]
    fn the_low_band_is_two_floors_or_a_fortieth_of_the_volume() {
        const GIB: u64 = 1 << 30;
        assert_eq!(low_band(10 * GIB, 100 * GIB), 20 * GIB);
        assert_eq!(low_band(10 * GIB, 1843 * GIB), 1843 * GIB / 40);
        assert_eq!(low_band(10, 800), 20);
        assert_eq!(low_band(10, 801), 20);
        assert_eq!(low_band(10, 840), 21);
        assert_eq!(low_band(0, 1843 * GIB), 0);
        let free = |free| Some(VolumeUsage { total: 1000, free });
        assert!(below_low_band(10, free(24)));
        assert!(!below_low_band(10, free(25)));
        assert!(below_low_band(20, free(39)));
        assert!(!below_low_band(20, free(40)));
        assert!(!below_low_band(0, free(0)));
        assert!(!below_low_band(1001, free(0)));
        assert!(!below_low_band(10, None));
    }

    #[test]
    fn low_on_disk_needs_automatic_gc_and_a_floor() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_config(dir.path().join("cache"));
        let _volume = kache_fs::testutil::VolumeUsageGuard::new(
            dir.path(),
            Some(VolumeUsage {
                total: 1000,
                free: 199,
            }),
        );
        config.auto_recover_min_free_bytes = 100;
        config.auto_gc = true;
        assert!(low_on_disk(&config));
        config.auto_recover_min_free_bytes = 99;
        assert!(!low_on_disk(&config));
        config.auto_recover_min_free_bytes = 100;
        config.auto_gc = false;
        assert!(!low_on_disk(&config));
    }

    #[test]
    fn repeated_pressure_checks_back_off_and_handle_clock_changes() {
        assert_eq!(INTERVAL, Duration::from_secs(300));
        assert!(due(0, 1));
        assert!(due(50, 49));
        assert!(!due(50, 50));
        assert!(!due(50, 349));
        assert!(due(50, 350));
        assert!(due(50, 351));
    }

    #[test]
    fn a_pass_goes_on_to_three_floors_of_free_space_for_at_most_a_minute() {
        assert_eq!(REMOVAL_CAP, Duration::from_secs(60));
        let free = |free| Some(VolumeUsage { total: 100, free });
        let minute = Duration::from_secs(60);
        let just_under = Duration::from_millis(59_999);
        // Back over the floor is not enough: the pass goes on to 3 floors.
        assert!(keep_recovering(10, Duration::ZERO, minute, free(9)));
        assert!(keep_recovering(10, just_under, minute, free(10)));
        assert!(keep_recovering(10, just_under, minute, free(29)));
        assert!(!keep_recovering(10, Duration::ZERO, minute, free(30)));
        assert!(!keep_recovering(10, minute, minute, free(9)));
        assert!(!keep_recovering(10, Duration::ZERO, minute, None));
        // A floor larger than the volume is no floor.
        assert!(!keep_recovering(101, Duration::ZERO, minute, free(9)));
        assert!(keep_recovering(100, Duration::ZERO, minute, free(9)));
    }

    #[test]
    fn missing_timestamp_is_due_but_bad_or_unreadable_records_skip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_check(dir.path()).unwrap(), 0);
        let path = dir.path().join(LAST_CHECK);
        std::fs::write(&path, " 42\n").unwrap();
        assert_eq!(last_check(dir.path()).unwrap(), 42);
        std::fs::write(&path, "bad").unwrap();
        assert!(last_check(dir.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(last_check(dir.path()).is_err());
    }

    #[test]
    fn disabled_and_unreadable_stores_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_config(dir.path().join("missing"));
        assert!(!wanted(&config));
        run(&config);
        assert!(!config.cache_dir.exists());
        config.auto_recover_min_free_bytes = 1;
        assert!(!wanted(&config));
        run(&config);
        assert!(!config.cache_dir.exists());
    }

    /// An entry no eviction protection covers: accessed an hour ago, with
    /// no clone of its blob left outside the store.
    #[cfg(unix)]
    fn put_evictable(store: &Store, dir: &Path, key: &str) {
        let source = dir.join(format!("{key}.rlib"));
        std::fs::write(&source, key).unwrap();
        store
            .put(
                key,
                key,
                &["lib".into()],
                &[],
                "host",
                "dev",
                &[(source.clone(), format!("{key}.rlib"))],
                "",
                "",
            )
            .unwrap();
        store.remove_clone_for_test(&source);
        store.set_last_accessed_for_test(key, "-1 hour");
    }

    #[test]
    #[cfg(unix)]
    fn a_full_disk_that_refuses_the_timestamp_still_recovers_once_an_interval() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_config(dir.path().join("cache"));
        let store = Store::open(&config).unwrap();
        let _volume = kache_fs::testutil::VolumeUsageGuard::new(
            dir.path(),
            Some(VolumeUsage {
                total: 1000,
                free: 10,
            }),
        );
        config.auto_recover_min_free_bytes = 100;
        let full = |_: &Path, _: u64| -> Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull).into())
        };
        put_evictable(&store, dir.path(), "old");
        recover_with(&config, REMOVAL_CAP, full).unwrap();
        assert!(!store.contains("old"), "evicted without a timestamp");
        assert!(!config.cache_dir.join(LAST_CHECK).exists());

        // This process keeps the interval without the file.
        put_evictable(&store, dir.path(), "next");
        assert!(!wanted(&config));
        run(&config);
        assert!(store.contains("next"));

        let interval_ago = crate::maintenance::unix_now_secs() - INTERVAL.as_secs();
        unrecorded().insert(config.cache_dir.clone(), interval_ago);
        run(&config);
        assert!(!store.contains("next"), "the next interval recovers");
        assert!(config.cache_dir.join(LAST_CHECK).exists());
    }

    #[test]
    #[cfg(unix)]
    fn a_candidate_scan_longer_than_the_removal_cap_still_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_config(dir.path().join("cache"));
        let store = Store::open(&config).unwrap();
        let _volume = kache_fs::testutil::VolumeUsageGuard::new(
            dir.path(),
            Some(VolumeUsage {
                total: 1000,
                free: 10,
            }),
        );
        config.auto_recover_min_free_bytes = 100;
        put_evictable(&store, dir.path(), "old");
        // The scan alone outlasts the cap, as on a large index.
        let cap = Duration::from_millis(200);
        let _scan =
            crate::store::SlowEvictionScanGuard::new(&config.cache_dir, Duration::from_millis(400));
        recover_with(&config, cap, |_, _| Ok(())).unwrap();
        assert!(!store.contains("old"), "the removals have their own cap");
    }
}
