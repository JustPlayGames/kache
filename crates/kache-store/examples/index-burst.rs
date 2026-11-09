//! A landing's burst of hits against a copy of a large index: the daemon's
//! HashFiles lookups under one store mutex, the 250 ms memo-row flush, and
//! each hit's input-prediction read (with its daily `last_used` touch).
//! Run with `--features test-support --example index-burst -- CACHE_DIR MODE THREADS SECONDS PREFIX`.
//! MODE: `shared` (the flush holds the lookups' mutex, as the daemon does
//! today) or `split` (the flush writes on a connection of its own).

use anyhow::{Context, Result, ensure};
use kache_store::config::Config;
use kache_store::file_hash::FileFingerprint;
use kache_store::{ArtifactPolicy, ArtifactStore};
use rusqlite::params;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

/// One worker's latencies: prefetch, its mutex wait, prediction read, and
/// (start, prefetch, prediction) per hit.
type WorkerTimes = (Vec<f64>, Vec<f64>, Vec<f64>, Vec<(f64, f64, f64)>);
static OPENS: Mutex<Vec<f64>> = Mutex::new(Vec::new());
static READS: Mutex<Vec<f64>> = Mutex::new(Vec::new());

fn config(dir: &std::path::Path) -> Config {
    Config {
        cache_dir: dir.to_path_buf(),
        max_size: u64::MAX / 4,
        gc_evict_shared: false,
        gc_spare_young_hours: 0,
        held_bytes_budget: Default::default(),
        upload_spool_max_jobs: 1,
        deferred_durability: true,
        index_mode: Default::default(),
        compress_predictions: true,
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
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
        "{name}: n={} p50={:.2} p90={:.2} p99={:.1} max={:.1} mean={:.2} sum={:.1}s over100={}",
        xs.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        xs[xs.len() - 1],
        sum / xs.len() as f64,
        sum / 1000.0,
        xs.iter().filter(|x| **x > 100.0).count()
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 6,
        "usage: index-burst CACHE_DIR shared|split THREADS SECONDS PATH_PREFIX"
    );
    let dir = std::path::PathBuf::from(&args[1]);
    // `pooled`: the lookups go through the hit's own pooled connection,
    // with no store mutex, and the flush writes on its own connection.
    let (split, pooled) = match args[2].as_str() {
        "shared" => (false, false),
        "split" => (true, false),
        "pooled" => (true, true),
        other => anyhow::bail!("mode {other}"),
    };
    let threads: usize = args[3].parse()?;
    let seconds: u64 = args[4].parse()?;
    let prefix = args[5].clone();
    let files_per_hit: usize = std::env::var("FILES_PER_HIT")
        .ok()
        .map_or(Ok(60), |v| v.parse())?;
    let hits_per_sec: f64 = std::env::var("HITS_PER_SEC")
        .ok()
        .map_or(Ok(140.0), |v| v.parse())?;
    kache_store::defer_wal_checkpoints();
    kache_store::pool_index_connections(32);
    let cfg = config(&dir);

    // Samples: paths an earlier landing of one checkout left (their rows'
    // old stamps are REPLACEd, as a new target folder's are), and
    // prediction rows due for their daily touch.
    let raw = rusqlite::Connection::open(dir.join("index.db"))?;
    let upper = format!("{prefix}\u{10FFFF}");
    let mut paths: Vec<(String, i64)> = raw
        .prepare("SELECT path, size FROM file_hashes WHERE path > ?1 AND path < ?2 LIMIT 40000")?
        .query_map(params![prefix, upper], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let identities: Vec<String> = raw
        .prepare("SELECT identity FROM input_predictions INDEXED BY input_predictions_last_used WHERE last_used < ?1 ORDER BY last_used DESC LIMIT 20000")?
        .query_map(params![now - 86_400 - 60], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(raw);
    ensure!(!paths.is_empty(), "no file_hashes rows under {prefix}");
    ensure!(!identities.is_empty(), "no prediction rows due for a touch");
    // A run's stamp: every path is new to the index at this stamp.
    let stamp = now * 1_000_000_000 + i64::from(std::process::id());
    paths.sort();
    println!(
        "paths={} identities={} mode={} threads={threads} seconds={seconds} files_per_hit={files_per_hit} hits_per_sec={hits_per_sec}",
        paths.len(),
        identities.len(),
        args[2]
    );

    let hash_store = Arc::new(Mutex::new(Store::open(&cfg)?));
    hash_store.lock().unwrap().defer_wal_checkpoints_here()?;
    let flush_store = Arc::new(Mutex::new(Store::open(&cfg)?));
    flush_store.lock().unwrap().defer_wal_checkpoints_here()?;
    let pending: Arc<Mutex<Vec<(FileFingerprint, String)>>> = Arc::default();
    let learned: Arc<Mutex<std::collections::HashSet<usize>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let next_identity = Arc::new(AtomicUsize::new(0));
    let paths = Arc::new(paths);
    let identities = Arc::new(identities);
    let started = Instant::now();

    let flusher = {
        let (hash_store, flush_store, pending, stop) = (
            hash_store.clone(),
            flush_store.clone(),
            pending.clone(),
            stop.clone(),
        );
        std::thread::spawn(move || {
            let mut flushes = Vec::new();
            let mut sizes = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let batch = std::mem::take(&mut *pending.lock().unwrap());
                if batch.is_empty() {
                    continue;
                }
                let rows: Vec<(FileFingerprint, &str)> = batch
                    .iter()
                    .map(|(fp, h)| (fp.clone(), h.as_str()))
                    .collect();
                let t = Instant::now();
                let store = if split { &flush_store } else { &hash_store };
                store.lock().unwrap().record_verified_file_hashes(&rows);
                flushes.push(ms(t));
                sizes.push(rows.len() as f64);
            }
            (flushes, sizes)
        })
    };

    let mut workers = Vec::new();
    for worker in 0..threads {
        let (hash_store, pending, learned, stop, paths, identities, next_identity, cfg) = (
            hash_store.clone(),
            pending.clone(),
            learned.clone(),
            stop.clone(),
            paths.clone(),
            identities.clone(),
            next_identity.clone(),
            cfg.clone(),
        );
        workers.push(std::thread::spawn(move || -> Result<WorkerTimes> {
            let mut prefetch = Vec::new();
            let mut mutex_wait = Vec::new();
            let mut predict = Vec::new();
            let mut timeline = Vec::new();
            let mut opens = Vec::new();
            let mut reads = Vec::new();
            let mut rng = 0x9E3779B97F4A7C15u64 ^ (worker as u64 * 7919);
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let pace = Duration::from_secs_f64(threads as f64 / hits_per_sec);
            let mut due = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < due {
                    std::thread::sleep(due - now);
                }
                due += pace;
                let hit_start = ms(started);
                // A unit's inputs: a run of neighbouring paths (one crate's
                // deps) from a random place.
                let base = (next() as usize) % paths.len();
                let t = Instant::now();
                let mut waited = 0.0;
                let own = if pooled {
                    Some(Store::open(&cfg)?)
                } else {
                    None
                };
                for i in 0..files_per_hit {
                    let index = (base + i) % paths.len();
                    let (path, size) = &paths[index];
                    let fp = FileFingerprint {
                        path: path.clone(),
                        size: (*size).max(65_536),
                        mtime_ns: stamp,
                        ctime_ns: stamp,
                        inode: 1_000_000 + index as i64,
                    };
                    let found = if let Some(own) = &own {
                        own.file_hash_cache().get(&fp)?
                    } else {
                        let w = Instant::now();
                        let store = hash_store.lock().unwrap();
                        waited += ms(w);
                        store.file_hash_cache().get(&fp)?
                    };
                    if found.is_none() && learned.lock().unwrap().insert(index) {
                        pending
                            .lock()
                            .unwrap()
                            .push((fp, blake3::hash(path.as_bytes()).to_hex().to_string()));
                    }
                }
                drop(own);
                prefetch.push(ms(t));
                mutex_wait.push(waited);
                let t = Instant::now();
                let store = Store::open(&cfg)?;
                opens.push(ms(t));
                let identity =
                    &identities[next_identity.fetch_add(1, Ordering::Relaxed) % identities.len()];
                let r = Instant::now();
                let _ = store.file_hash_cache().get_input_prediction(identity);
                reads.push(ms(r));
                drop(store);
                predict.push(ms(t));
                timeline.push((
                    hit_start,
                    *prefetch.last().unwrap(),
                    *predict.last().unwrap(),
                ));
            }
            OPENS.lock().unwrap().extend(opens);
            READS.lock().unwrap().extend(reads);
            Ok((prefetch, mutex_wait, predict, timeline))
        }));
    }
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut prefetch = Vec::new();
    let mut waits = Vec::new();
    let mut predict = Vec::new();
    let mut timeline = Vec::new();
    for worker in workers {
        let (a, b, c, d) = worker.join().expect("worker").context("worker failed")?;
        prefetch.extend(a);
        waits.extend(b);
        predict.extend(c);
        timeline.extend(d);
    }
    let (flushes, sizes) = flusher.join().expect("flusher");
    summary("prefetch_ms", prefetch);
    summary("prefetch_mutex_wait_ms", waits);
    summary("prediction_read_ms", predict);
    summary("store_open_ms", std::mem::take(&mut *OPENS.lock().unwrap()));
    summary(
        "get_input_prediction_ms",
        std::mem::take(&mut *READS.lock().unwrap()),
    );
    let raw = rusqlite::Connection::open(dir.join("index.db"))?;
    let used = identities.len().min(next_identity.load(Ordering::Relaxed));
    let mut touched = 0;
    for identity in identities.iter().take(used) {
        let last: i64 = raw
            .query_row(
                "SELECT last_used FROM input_predictions WHERE identity = ?1",
                params![identity],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if last >= now {
            touched += 1;
        }
    }
    println!("touched {touched} of {used} rows read");
    summary("flush_ms", flushes.clone());
    summary("flush_rows", sizes);
    if let Ok(out) = std::env::var("TIMELINE") {
        let mut text = String::new();
        timeline.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (t, p, r) in timeline {
            text.push_str(&format!("{t:.1}\t{p:.2}\t{r:.2}\n"));
        }
        std::fs::write(out, text)?;
    }
    Ok(())
}
