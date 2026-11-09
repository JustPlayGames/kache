//! A landing's burst of cache hits, replayed against a copy of a real index
//! (KACHE-NEXT75): each hit does the index work a delegated hit does inside
//! the daemon (open its store, read its input prediction, prefetch its
//! inputs' hashes, look its entry up and stamp it), with HashFiles, StampHits
//! and the prediction's use stamp either sent through the daemon's own socket
//! (as before) or handled in process. Ignored: run by hand, through the
//! lane's sandbox (run-suite.sh), with
//! `KACHE_BURST_RIG=<dir>` holding `index.db` (the copy), `files.tsv` (path,
//! size, mtime_ns, ctime_ns, inode of existing files), `keys.txt` (entry keys
//! with a `store/<key>/meta.json` each) and `idents.txt` (input prediction
//! identities). `RIG_MODE`: `base` (the socket for all three), `all` (the
//! product's routing with the invocation's daemon set), or any of `h`, `s`,
//! `t` (HashFiles, StampHits, touches in process).
//! KACHE-NEXT76: `RIG_POOL` (the daemon's idle index connections; 0 opens
//! one per store, as before the pool), `RIG_WRITERS` and
//! `RIG_WRITE_GAP_MS` (writers taking the index's write lock beside the
//! hits), `RIG_SAMPLE` (the threads' state, system call and wait channel
//! from /proc), and the stamps' time when they are timed (`s`, `base`).
use super::*;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn lines(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).expect("rig input");
    std::io::BufReader::new(file)
        .lines()
        .map(|line| line.expect("rig input line"))
        .filter(|line| !line.is_empty())
        .collect()
}

#[derive(Default)]
struct Times {
    store_open: Vec<f64>,
    prediction: Vec<f64>,
    prefetch: Vec<f64>,
    lookup: Vec<f64>,
    total: Vec<f64>,
}

fn summary(name: &str, mut values: Vec<f64>) -> String {
    if values.is_empty() {
        return format!("{name}: n=0");
    }
    values.sort_by(f64::total_cmp);
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    let sum: f64 = values.iter().sum();
    format!(
        "{name}: n={} p50={:.2} p90={:.2} p99={:.2} max={:.1} mean={:.2} sum={:.1}s over100={}",
        values.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        values[values.len() - 1],
        sum / values.len() as f64,
        sum / 1000.0,
        values.iter().filter(|v| **v > 100.0).count()
    )
}

fn cpu_seconds() -> (f64, f64) {
    // SAFETY: getrusage writes one `rusage` into the zeroed struct it is
    // given, which lives on this stack frame for the whole call.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (secs(usage.ru_utime), secs(usage.ru_stime))
}

fn threads_now() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Threads:"))
                .and_then(|n| n.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// `count` files of 64 KiB and a little (sparse: no blocks) under `dir`,
/// each with a memo row in the index, as a restore leaves them: a lookup of
/// one is an index read, never a hash. Made once; the lines of files.tsv.
fn synth_files(config: &Config, dir: &Path, count: usize) -> Vec<String> {
    let list = dir.join("list.tsv");
    if let Ok(text) = std::fs::read_to_string(&list)
        && text.lines().count() >= count
    {
        return text.lines().take(count).map(str::to_owned).collect();
    }
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let sub = dir.join(format!("{:03}", i % 512));
        std::fs::create_dir_all(&sub).expect("synth dir");
        let path = sub.join(format!("f{i:07}.rmeta"));
        let file = std::fs::File::create(&path).expect("synth file");
        file.set_len(65_536 + (i % 4096) as u64).expect("synth size");
        rows.push(path);
    }
    // Settle the stamps (HASH_SETTLE_NS) before the rows are written.
    std::thread::sleep(Duration::from_millis(2_500));
    let store = Store::open(config).expect("store");
    let hash = blake3::hash(b"synthetic").to_hex().to_string();
    let mut out = Vec::with_capacity(count);
    for chunk in rows.chunks(5_000) {
        let batch: Vec<(crate::cache_key::FileFingerprint, &str)> = chunk
            .iter()
            .map(|path| {
                let fp = crate::cache_key::FileFingerprint::from_path(path).expect("stat");
                out.push(format!(
                    "{}\t{}\t{}\t{}\t{}",
                    fp.path, fp.size, fp.mtime_ns, fp.ctime_ns, fp.inode
                ));
                (fp, hash.as_str())
            })
            .collect();
        store.record_verified_file_hashes(&batch);
    }
    std::fs::write(&list, out.join("\n") + "\n").expect("synth list");
    out
}

/// Make `pct` % of `idents` due for their daily use stamp (two days old),
/// the rest fresh, so every run writes the same stamps.
fn make_due(config: &Config, idents: &[String], pct: usize) {
    let store = Store::open(config).expect("store");
    let now = crate::cache_key::wall_clock_ns() / 1_000_000_000;
    let _ = store.with_write_transaction(|store| {
        let cache = store.file_hash_cache();
        let db = cache.db();
        let mut update =
            db.prepare("UPDATE input_predictions SET last_used = ?2 WHERE identity = ?1")?;
        for (i, identity) in idents.iter().enumerate() {
            let used = if i % 100 < pct { now - 2 * 86_400 } else { now };
            update.execute(rusqlite::params![identity, used])?;
        }
        Ok(())
    });
}

/// Make every key's entry due for its access stamp: a hit stamps an entry
/// only once per `HIT_STAMP_INTERVAL`, so a key an earlier run stamped
/// would not stamp again.
fn make_keys_stamp(config: &Config, keys: &[String]) {
    let store = Store::open(config).expect("store");
    let _ = store.with_write_transaction(|store| {
        let cache = store.file_hash_cache();
        let db = cache.db();
        let mut update = db.prepare(
            "UPDATE entries SET last_accessed = datetime('now', '-2 days') WHERE cache_key = ?1",
        )?;
        for key in keys {
            update.execute(rusqlite::params![key])?;
        }
        Ok(())
    });
}

/// What the process's threads are doing, sampled from `/proc/self/task`
/// every 100 ms: state, system call number and kernel wait channel.
fn sample_threads(done: &AtomicBool) -> std::collections::HashMap<String, usize> {
    let mut seen = std::collections::HashMap::new();
    while !done.load(Ordering::Relaxed) {
        if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
            for task in tasks.flatten() {
                let path = task.path();
                let read =
                    |name: &str| std::fs::read_to_string(path.join(name)).unwrap_or_default();
                let stat = read("stat");
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .unwrap_or("?")
                    .to_owned();
                let syscall = read("syscall");
                let call = syscall.split_whitespace().next().unwrap_or("?").to_owned();
                let wchan = read("wchan");
                *seen.entry(format!("{state} {call} {wchan}")).or_insert(0) += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    seen
}

#[test]
#[ignore = "rig: needs KACHE_BURST_RIG and a copy of an index"]
fn hit_burst_rig() {
    let Ok(dir) = std::env::var("KACHE_BURST_RIG") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mode = std::env::var("RIG_MODE").unwrap_or_else(|_| "base".into());
    let routed = mode == "all";
    // "base" names none of the three (its 's' is not StampHits: until
    // KACHE-NEXT76 the base mode handed its stamps over in process).
    let flags = if mode == "base" { "" } else { mode.as_str() };
    let hash_in_process = routed || flags.contains('h');
    let stamps_in_process = routed || flags.contains('s');
    let touches_batched = routed || flags.contains('t');
    let threads = env_num("RIG_THREADS", 20) as usize;
    let seconds = env_num("RIG_SECONDS", 20);
    let hits_per_sec = env_num("RIG_HITS_PER_SEC", 140);
    let files_per_hit = env_num("RIG_FILES_PER_HIT", 60) as usize;
    let handoffs = env_num("RIG_HANDOFFS", 1);
    let skip = env_num("RIG_SKIP", 0) as usize;

    let mut config = test_config(&dir);
    config.input_predictions = true;
    // The daemon's shared index connections (KACHE-NEXT76): RIG_POOL idle
    // connections (0: a connection per store, as before the pool).
    let pool_idle = env_num("RIG_POOL", 0) as usize;
    if pool_idle > 0 {
        kache_store::pool_index_connections(pool_idle);
    }
    let writers = env_num("RIG_WRITERS", 0) as usize;
    let write_gap = Duration::from_millis(env_num("RIG_WRITE_GAP_MS", 20));
    let synth = env_num("RIG_SYNTH", 0) as usize;
    let mut file_lines = if synth > 0 {
        synth_files(&config, &dir.join("synth"), synth)
    } else {
        Vec::new()
    };
    if std::env::var("RIG_SEED_ONLY").is_ok() {
        return;
    }
    file_lines.extend(lines(&dir.join("files.tsv")));
    let files: Vec<HashFileRequest> = file_lines
        .into_iter()
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            HashFileRequest {
                path: f[0].to_string(),
                size: f[1].parse().expect("size"),
                mtime_ns: f[2].parse().expect("mtime"),
                ctime_ns: f[3].parse().expect("ctime"),
                inode: f[4].parse().expect("inode"),
            }
        })
        .collect();
    let keys = lines(&dir.join("keys.txt"));
    let idents = lines(&dir.join("idents.txt"));
    let hits = (seconds * hits_per_sec) as usize;

    let due_pct = env_num("RIG_DUE_PCT", 100) as usize;
    make_due(&config, &idents, due_pct);
    make_keys_stamp(&config, &keys);
    let socket = config.socket_path();
    let cache_dir = config.cache_dir.clone();
    let daemon = Arc::new(Daemon::new(config.clone()));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(daemon.capacity.blocking_threads)
        .build()
        .expect("runtime");
    let lifecycle = Arc::new(Lifecycle::default());
    {
        let _guard = runtime.enter();
        let listener = bind_listener(&socket);
        let (d, flag) = (daemon.clone(), lifecycle.clone());
        let limiter = Arc::new(tokio::sync::Semaphore::new(daemon.capacity.connections));
        runtime.spawn(async move {
            loop {
                let Ok(stream) = listener.accept().await else {
                    continue;
                };
                let (d, flag, limiter) = (d.clone(), flag.clone(), limiter.clone());
                tokio::spawn(async move {
                    let _ =
                        handle_connection_after_queue(stream, &d, &flag, limiter, Instant::now())
                            .await;
                });
            }
        });
        for which in 0..2 {
            let d = daemon.clone();
            runtime.spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(250));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let d = d.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if which == 0 {
                            d.flush_file_hash_records();
                        } else {
                            d.flush_hit_stamps();
                            d.flush_handed_rows();
                        }
                    })
                    .await;
                }
            });
        }
    }
    // Warm the daemon's own connections (the box's daemon has them open).
    daemon.flush_hit_stamps();
    let _ = daemon.with_hash_store(|_| Ok(()));
    let _ = daemon.with_stamp_store(|_| Ok(()));

    let done = Arc::new(AtomicBool::new(false));
    let peak_threads = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let (done, peak) = (done.clone(), peak_threads.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(threads_now(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };

    // Writers beside the hits, as a burst's misses and bookkeeping are:
    // each takes the index's write lock in turn, and one waiting for it
    // holds its connection.
    let writer_threads: Vec<_> = (0..writers)
        .map(|writer| {
            let (config, done, dir) = (config.clone(), done.clone(), dir.clone());
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let store = Store::open(&config).expect("store");
                    let _ = store.remember_incremental_dir(
                        &dir.join(format!("rig-incr/w{writer}-{i}-{}", std::process::id())),
                    );
                    drop(store);
                    i += 1;
                    std::thread::sleep(write_gap);
                }
                i
            })
        })
        .collect();
    let thread_sampler = std::env::var("RIG_SAMPLE").is_ok().then(|| {
        let done = done.clone();
        std::thread::spawn(move || sample_threads(&done))
    });
    let pool_before = kache_store::index_pool_stats();
    let (user0, sys0) = cpu_seconds();
    let start = Instant::now() + Duration::from_millis(200);
    let times = Arc::new(Mutex::new(Times::default()));
    let failures = Arc::new(AtomicUsize::new(0));
    let stamp_times = Arc::new(Mutex::new(Vec::<f64>::new()));
    let files = Arc::new(files);
    let keys = Arc::new(keys);
    let idents = Arc::new(idents);
    let workers: Vec<_> = (0..threads)
        .map(|worker| {
            let (daemon, config, socket, cache_dir) = (
                daemon.clone(),
                config.clone(),
                socket.clone(),
                cache_dir.clone(),
            );
            let (files, keys, idents, times, failures, stamp_times) = (
                files.clone(),
                keys.clone(),
                idents.clone(),
                times.clone(),
                failures.clone(),
                stamp_times.clone(),
            );
            std::thread::spawn(move || {
                let env = crate::invocation::InvocationEnv::capture();
                let _scope = routed.then(|| {
                    crate::wrapper::InvocationWrapperScope::enter_for(&env, Some(daemon.clone()))
                });
                let mut local = Times::default();
                for hit in (worker..hits).step_by(threads) {
                    let due = start + Duration::from_secs_f64(hit as f64 / hits_per_sec as f64);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    let n = skip + hit;
                    let t0 = Instant::now();
                    for _ in 0..handoffs {
                        // The hit's own hand-off round trips share the socket.
                        let _ = send_request_with_timeout(
                            &socket,
                            &Request::Health,
                            Duration::from_secs(3),
                        );
                    }
                    let t_open = Instant::now();
                    let mut store = Store::open(&config).expect("store");
                    if routed {
                        crate::wrapper::send_hit_stamps_to_daemon(&config, &mut store);
                    } else {
                        if stamps_in_process {
                            let d = daemon.clone();
                            let dir = cache_dir.to_string_lossy().into_owned();
                            let timed = stamp_times.clone();
                            store.send_hit_stamps_to(Box::new(move |key| {
                                let t = Instant::now();
                                let ok = d
                                    .handle_stamp_hits(&kache_protocol::StampHitsRequest {
                                        cache_dir: dir.clone(),
                                        keys: vec![key.to_string()],
                                    })
                                    .ok;
                                timed
                                    .lock()
                                    .expect("stamps")
                                    .push(t.elapsed().as_secs_f64() * 1e3);
                                ok
                            }));
                        } else {
                            let (socket, dir) = (socket.clone(), cache_dir.clone());
                            let timed = stamp_times.clone();
                            store.send_hit_stamps_to(Box::new(move |key| {
                                let t = Instant::now();
                                let ok = send_hit_stamp(&socket, &dir, key);
                                timed
                                    .lock()
                                    .expect("stamps")
                                    .push(t.elapsed().as_secs_f64() * 1e3);
                                ok
                            }));
                        }
                        if touches_batched {
                            let d = daemon.clone();
                            store.send_prediction_touches_to(Box::new(move |identity, now| {
                                d.take_prediction_touch(identity, now)
                            }));
                        }
                    }
                    let t_pred = Instant::now();
                    let _ = store
                        .file_hash_cache()
                        .get_input_prediction(&idents[n % idents.len()]);
                    let t_pre = Instant::now();
                    let first = (n * files_per_hit) % files.len().max(1);
                    let request: Vec<HashFileRequest> = files
                        .iter()
                        .cycle()
                        .skip(first)
                        .take(files_per_hit.min(files.len()))
                        .cloned()
                        .collect();
                    let results = if hash_in_process && !routed {
                        hash_files_results_from_response(
                            daemon.handle_hash_files(&HashFilesRequest { files: request }),
                        )
                    } else {
                        send_hash_files_request(&socket, request)
                    };
                    if results.is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    let t_look = Instant::now();
                    let _ = store.get(&keys[n % keys.len()]);
                    let t_end = Instant::now();
                    drop(store);
                    let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
                    local.store_open.push(ms(t_open, t_pred));
                    local.prediction.push(ms(t_pred, t_pre));
                    local.prefetch.push(ms(t_pre, t_look));
                    local.lookup.push(ms(t_look, t_end));
                    local.total.push(ms(t0, t_end));
                }
                let mut all = times.lock().expect("times");
                all.store_open.extend(local.store_open);
                all.prediction.extend(local.prediction);
                all.prefetch.extend(local.prefetch);
                all.lookup.extend(local.lookup);
                all.total.extend(local.total);
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }
    let wall = start.elapsed().as_secs_f64();
    let (user1, sys1) = cpu_seconds();
    let pool_after = kache_store::index_pool_stats();
    // Let the ticks write what was handed over, as the daemon does.
    std::thread::sleep(Duration::from_millis(600));
    daemon.flush_hit_stamps();
    daemon.flush_file_hash_records();
    done.store(true, Ordering::Relaxed);
    sampler.join().expect("sampler");
    if let Some(sampler) = thread_sampler {
        let mut seen: Vec<(String, usize)> = sampler
            .join()
            .expect("thread sampler")
            .into_iter()
            .collect();
        seen.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let all: usize = seen.iter().map(|(_, n)| n).sum();
        for (what, n) in seen.iter().take(14) {
            println!(
                "sample {n:6} {:5.1}% {what}",
                *n as f64 * 100.0 / all.max(1) as f64
            );
        }
    }
    let writes: u64 = writer_threads
        .into_iter()
        .map(|writer| writer.join().expect("writer"))
        .sum();

    let times = std::mem::take(&mut *times.lock().expect("times"));
    println!(
        "mode={mode} threads={threads} seconds={seconds} hits_per_sec={hits_per_sec} \
         files_per_hit={files_per_hit} handoffs={handoffs} skip={skip} due_pct={due_pct} hits={} files={} keys={} idents={}",
        times.total.len(),
        files.len(),
        keys.len(),
        idents.len()
    );
    println!("{}", summary("hit_total_ms", times.total));
    println!("{}", summary("store_open_ms", times.store_open));
    println!("{}", summary("prediction_read_ms", times.prediction));
    println!("{}", summary("input_hash_prefetch_ms", times.prefetch));
    println!(
        "{}",
        summary(
            "stamp_ms",
            std::mem::take(&mut *stamp_times.lock().expect("stamps"))
        )
    );
    println!(
        "pool idle_cap={pool_idle} writers={writers} writes={writes} opened={} reused={} closed_over_cap={} in_use_max={} idle_after={}",
        pool_after.opened - pool_before.opened,
        pool_after.reused - pool_before.reused,
        pool_after.closed_over_cap - pool_before.closed_over_cap,
        pool_after.in_use_max,
        pool_after.idle
    );
    println!("{}", summary("lookup_ms", times.lookup));
    println!(
        "wall={wall:.1}s cpu_user={:.1}s cpu_sys={:.1}s peak_threads={} prefetch_failures={} processes=1",
        user1 - user0,
        sys1 - sys0,
        peak_threads.load(Ordering::Relaxed),
        failures.load(Ordering::Relaxed)
    );
    drop(runtime);
}
