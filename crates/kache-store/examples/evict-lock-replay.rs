//! A size sweep against builds that write: how long a build's write waits
//! for the index's write lock while a sweep removes entries whose blobs are
//! real files on the disk (each removal unlinks its last-reference blobs).
//! Run with `--features test-support --example evict-lock-replay -- CACHE_DIR ENTRIES BLOB_MIB`.
//! `PRESSURE_MIBS` (default 0): a writer that fsyncs that many MiB a second
//! into the cache dir during the sweep, as a loaded disk's journal would.
//! `WRITER_GAP_MS` (default 5): the pause between a build's writes.
//! Prints the sweep's figures and the build's lock waits.

use anyhow::{Context, Result, ensure};
use kache_store::config::Config;
use kache_store::{ArtifactPolicy, ArtifactStore};
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct Policy;
impl ArtifactPolicy for Policy {
    fn allow_hardlink(_: &str) -> bool {
        false
    }
    fn allow_empty(_: &str, _: &[String]) -> bool {
        false
    }
    fn emit_kind(_: &str) -> Option<&'static str> {
        None
    }
    fn stable_after_store(_: &str) -> bool {
        true
    }
}
type Store = ArtifactStore<Policy>;

fn config(dir: &std::path::Path, max_size: u64) -> Config {
    Config {
        cache_dir: dir.to_path_buf(),
        max_size,
        gc_evict_shared: false,
        gc_spare_young_hours: 0,
        held_bytes_budget: Default::default(),
        upload_spool_max_jobs: 1,
        deferred_durability: true,
        index_mode: Default::default(),
        compress_predictions: true,
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    std::env::var(name)
        .ok()
        .map_or(Ok(default), |v| v.parse().with_context(|| name.to_owned()))
}

/// Distinct bytes per entry, so no two blobs share a hash.
fn fill(buf: &mut [u8], seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

fn summary(name: &str, mut xs: Vec<f64>) {
    if xs.is_empty() {
        println!("{name}: none");
        return;
    }
    xs.sort_by(f64::total_cmp);
    let at = |p: f64| xs[((xs.len() - 1) as f64 * p) as usize];
    let sum: f64 = xs.iter().sum();
    println!(
        "{name}: n={} p50={:.2} p90={:.2} p99={:.1} max={:.1} ms; sum={:.2}s over100ms={} over1s={}",
        xs.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        xs[xs.len() - 1],
        sum / 1000.0,
        xs.iter().filter(|x| **x > 100.0).count(),
        xs.iter().filter(|x| **x > 1000.0).count()
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: evict-lock-replay CACHE_DIR ENTRIES BLOB_MIB"
    );
    let dir = std::path::PathBuf::from(&args[1]);
    let entries: usize = args[2].parse()?;
    let blob_mib: usize = args[3].parse()?;
    let pressure = env_u64("PRESSURE_MIBS", 0)?;
    let gap = Duration::from_millis(env_u64("WRITER_GAP_MS", 5)?);
    std::fs::create_dir_all(&dir)?;
    let src = dir.join("src");
    std::fs::create_dir_all(&src)?;

    // Fill the store: one blob of BLOB_MIB per entry, copied in, the source
    // removed so nothing outside the store holds it.
    let store = Store::open(config(&dir, u64::MAX / 4))?;
    let mut buf = vec![0u8; blob_mib << 20];
    let filled = Instant::now();
    let mut keys = Vec::new();
    for i in 0..entries {
        fill(&mut buf, i as u64 + 1);
        let file = src.join(format!("lib{i}.rlib"));
        std::fs::write(&file, &buf)?;
        let key = format!("{:064x}", i as u128 * 0x1_0000_0001 + 7);
        store.put(
            &key,
            &format!("crate{i}"),
            &["lib".to_owned()],
            &[],
            "x86_64-unknown-linux-gnu",
            "debug",
            &[(file.clone(), format!("lib{i}.rlib"))],
            "",
            "",
        )?;
        std::fs::remove_file(&file)?;
        keys.push(key);
    }
    for key in &keys {
        store.set_last_accessed_for_test(key, "-1 day");
    }
    let size = store.physical_size()?;
    drop(store);
    println!(
        "filled {entries} entries of {blob_mib} MiB ({:.1} GiB) in {:.1}s",
        size as f64 / (1u64 << 30) as f64,
        filled.elapsed().as_secs_f64()
    );

    // A requested sweep over a cap of a quarter: it evicts down to 90 % of
    // that, about three quarters of the entries.
    let store = Store::open(config(&dir, size / 4))?;
    let probe = rusqlite::Connection::open(dir.join("index.db"))?;
    probe.busy_timeout(Duration::from_secs(120))?;
    probe.execute_batch("CREATE TABLE IF NOT EXISTS replay_probe (n INTEGER)")?;

    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let done = done.clone();
        std::thread::spawn(move || -> Result<(Vec<f64>, Vec<f64>)> {
            let mut waits = Vec::new();
            let mut commits = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let asked = Instant::now();
                probe.execute_batch("BEGIN IMMEDIATE")?;
                waits.push(asked.elapsed().as_secs_f64() * 1000.0);
                let held = Instant::now();
                probe.execute("INSERT INTO replay_probe (n) VALUES (1)", [])?;
                probe.execute_batch("COMMIT")?;
                commits.push(held.elapsed().as_secs_f64() * 1000.0);
                std::thread::sleep(gap);
            }
            Ok((waits, commits))
        })
    };
    let presser = (pressure > 0).then(|| {
        let done = done.clone();
        let path = dir.join("pressure.bin");
        std::thread::spawn(move || -> Result<(u64, Vec<f64>)> {
            let mut syncs = Vec::new();
            let chunk = vec![0x5au8; 4 << 20];
            let mut file = std::fs::File::create(&path)?;
            let mut written = 0u64;
            let started = Instant::now();
            while !done.load(Ordering::Relaxed) {
                // Ring over 256 MiB, so the file stays bounded.
                if written % (256 << 20) == 0 {
                    file = std::fs::File::create(&path)?;
                }
                file.write_all(&chunk)?;
                let synced = Instant::now();
                file.sync_data()?;
                syncs.push(synced.elapsed().as_secs_f64() * 1000.0);
                written += chunk.len() as u64;
                let due = Duration::from_secs_f64(written as f64 / ((pressure << 20) as f64));
                if let Some(rest) = due.checked_sub(started.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            drop(file);
            let _ = std::fs::remove_file(&path);
            Ok((written, syncs))
        })
    });

    // Let the writer settle before the sweep starts.
    std::thread::sleep(Duration::from_millis(500));
    let swept = Instant::now();
    let stats = store.evict()?;
    let sweep_s = swept.elapsed().as_secs_f64();
    std::thread::sleep(Duration::from_millis(200));
    done.store(true, Ordering::Relaxed);
    let (waits, commits) = writer.join().expect("writer thread")?;
    let (pressed, syncs) = match presser {
        Some(handle) => handle.join().expect("pressure thread")?,
        None => (0, Vec::new()),
    };
    println!(
        "sweep: {} entries evicted, {:.1} GiB freed, {} blobs, {:.1}s, evict_write_ms {}, evict_lock_ms {}, evict_lock_max_ms {}, pressure {} MiB",
        stats.entries_evicted,
        stats.bytes_freed as f64 / (1u64 << 30) as f64,
        stats.blobs_removed,
        sweep_s,
        stats.evict_write_ms,
        stats.evict_lock_ms,
        stats.evict_lock_max_ms,
        pressed >> 20
    );
    summary("build write-lock waits during the sweep", waits);
    summary("build commit (insert + WAL fsync) under the lock", commits);
    summary("pressure fdatasync of 4 MiB", syncs);
    Ok(())
}
