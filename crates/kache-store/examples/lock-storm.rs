//! An index storm in processes: who gets the index's write lock, and when,
//! while it stays busy and after a long hold lets it go.
//!
//! Run with `--features test-support --example lock-storm -- DIR` (DIR a new
//! scratch folder; the index is DIR/index.db, opened with
//! [`kache_store::open_index_db`] as every writer opens it). Settings come
//! from the environment (defaults in brackets):
//!
//! - `STORM_SECONDS` [30]: how long writers keep arriving.
//! - `STALL_AT_MS` [1000], `STALL_MS` [8000], `STALLS` [1], `STALL_EVERY_MS`
//!   [10000]: a writer that holds the lock that long (a sweep, a commit on a
//!   paged-out index), so waiters pile up behind it.
//! - `REG_PER_SEC` [20], `REG_HOLD_MS` [10..40]: wrapper registrations, one
//!   process each, waiting as [`kache_store`]'s `begin_write_waiting` does
//!   (the connection's 5 s busy timeout, four times).
//! - `PILE` [120]: extra registrations that arrive during the first stall.
//! - `BATCH_EVERY_MS` [150], `BATCH_HOLD_MS` [30..80]: the daemon's batches,
//!   which ask in 20 ms waits for up to 20 s.
//! - `STAMP_PER_SEC` [100]: hit stamps (one 100 ms wait, given up when busy).
//! - `STAMPERS` [4]: processes the stamps are spread over.
//! - `WAIT_MODE` [`kache`]: `sqlite` sets the waits with SQLite's own busy
//!   timeout, `kache` with [`kache_store::set_index_busy_wait`] (the index's
//!   turns, when the store keeps them).
//!
//! Each writer prints one line per write; the parent summarises them: each
//! kind's waits (p50/p90/p99/max), the writes that gave up, when the last
//! registration got through, and the CPU every writer process used.

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, TransactionBehavior};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A range `A..B` (milliseconds) from the environment.
fn env_range(name: &str, default: (u64, u64)) -> (u64, u64) {
    std::env::var(name)
        .ok()
        .and_then(|v| {
            let (a, b) = v.split_once("..")?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or(default)
}

/// Milliseconds since the Unix epoch: the clock every process shares.
fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs_f64()
        * 1000.0
}

/// A cheap per-process random stream (xorshift), seeded by pid and time.
struct Rng(u64);
impl Rng {
    fn new() -> Self {
        let seed = (std::process::id() as u64) << 32 ^ now_ms() as u64 ^ 0x9E37_79B9_7F4A_7C15;
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn between(&mut self, (a, b): (u64, u64)) -> u64 {
        if b <= a { a } else { a + self.next() % (b - a) }
    }
    /// An exponential gap for `per_sec` arrivals a second.
    fn gap(&mut self, per_sec: f64) -> Duration {
        let u = (self.next() % 1_000_000 + 1) as f64 / 1_000_001.0;
        Duration::from_secs_f64(-u.ln() / per_sec)
    }
}

fn sqlite_mode() -> bool {
    std::env::var("WAIT_MODE").is_ok_and(|m| m == "sqlite")
}

/// Set how long `db` waits for a lock held elsewhere.
fn set_wait(db: &Connection, wait: Duration) -> Result<()> {
    if sqlite_mode() {
        db.busy_timeout(wait)?;
    } else {
        kache_store::set_index_busy_wait(db, wait)?;
    }
    Ok(())
}

fn open(dir: &Path) -> Result<Connection> {
    let db = kache_store::open_index_db(&dir.join("index.db"))?;
    if sqlite_mode() {
        db.busy_timeout(Duration::from_secs(5))?;
    }
    Ok(db)
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Hold the write lock about `hold` ms (rows written, then the rest slept,
/// as a commit on a paged-out index waits for its pages), and commit.
/// Prints `H kind took_ms held_ms`.
fn hold_and_commit(tx: rusqlite::Transaction<'_>, who: &str, hold: u64) -> Result<()> {
    let took = now_ms();
    let started = Instant::now();
    tx.execute(
        "INSERT INTO storm_rows(who, payload) VALUES (?1, zeroblob(512))",
        [who],
    )?;
    let left = Duration::from_millis(hold).saturating_sub(started.elapsed());
    std::thread::sleep(left);
    tx.commit()?;
    println!("H {who} {took:.1} {:.1}", now_ms() - took);
    Ok(())
}

/// One line per write: `R kind start_ms wait_ms ok|gave_up`.
fn report(kind: &str, start: f64, wait: f64, ok: bool) {
    println!(
        "R {kind} {start:.1} {wait:.1} {}",
        if ok { "ok" } else { "gave_up" }
    );
}

/// A wrapper's registration: four waits of the connection's 5 s timeout.
fn child_register(dir: &Path) -> Result<()> {
    let mut rng = Rng::new();
    let db = open(dir)?;
    set_wait(&db, Duration::from_secs(5))?;
    let _place = kache_store::hold_index_place(&db);
    let start = now_ms();
    let mut attempt = 1;
    let tx = loop {
        match rusqlite::Transaction::new_unchecked(&db, TransactionBehavior::Immediate) {
            Ok(tx) => break Some(tx),
            Err(e) if is_busy(&e) && attempt < 4 => attempt += 1,
            Err(e) if is_busy(&e) => break None,
            Err(e) => return Err(e.into()),
        }
    };
    let wait = now_ms() - start;
    match tx {
        Some(tx) => {
            report("register", start, wait, true);
            hold_and_commit(
                tx,
                "register",
                rng.between(env_range("REG_HOLD_MS", (10, 40))),
            )?;
        }
        None => report("register", start, wait, false),
    }
    Ok(())
}

/// The daemon's batches until `end_ms`: 20 ms waits for up to 20 s each.
fn child_daemon(dir: &Path, end_ms: f64) -> Result<()> {
    let mut rng = Rng::new();
    let db = open(dir)?;
    let every = Duration::from_millis(env_u64("BATCH_EVERY_MS", 150));
    let hold = env_range("BATCH_HOLD_MS", (30, 80));
    while now_ms() < end_ms {
        std::thread::sleep(every);
        let start = now_ms();
        let begun = Instant::now();
        let _place = kache_store::hold_index_place(&db);
        let tx = loop {
            set_wait(&db, Duration::from_millis(20))?;
            match rusqlite::Transaction::new_unchecked(&db, TransactionBehavior::Immediate) {
                Ok(tx) => break Some(tx),
                Err(e) if is_busy(&e) && begun.elapsed() < Duration::from_secs(20) => {}
                Err(e) if is_busy(&e) => break None,
                Err(e) => return Err(e.into()),
            }
        };
        let wait = now_ms() - start;
        match tx {
            Some(tx) => {
                report("batch", start, wait, true);
                hold_and_commit(tx, "batch", rng.between(hold))?;
            }
            None => report("batch", start, wait, false),
        }
    }
    Ok(())
}

/// Hit stamps until `end_ms`: one 100 ms wait each, dropped when busy.
fn child_stamps(dir: &Path, end_ms: f64, per_sec: f64) -> Result<()> {
    let mut rng = Rng::new();
    let db = open(dir)?;
    while now_ms() < end_ms {
        std::thread::sleep(rng.gap(per_sec));
        set_wait(&db, Duration::from_millis(100))?;
        let start = now_ms();
        match db.execute(
            "INSERT INTO storm_rows(who, payload) VALUES ('stamp', zeroblob(64))",
            [],
        ) {
            Ok(_) => report("stamp", start, now_ms() - start, true),
            Err(e) if is_busy(&e) => report("stamp", start, now_ms() - start, false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// The long holds: `STALLS` of `STALL_MS`, the first at `STALL_AT_MS`.
fn child_staller(dir: &Path, epoch: f64) -> Result<()> {
    let db = open(dir)?;
    let at = env_u64("STALL_AT_MS", 1000) as f64;
    let every = env_u64("STALL_EVERY_MS", 10_000) as f64;
    for n in 0..env_u64("STALLS", 1) {
        let due = epoch + at + every * n as f64;
        let left = due - now_ms();
        if left > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(left / 1000.0));
        }
        set_wait(&db, Duration::from_secs(60))?;
        let start = now_ms();
        let tx = rusqlite::Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        report("stall", start, now_ms() - start, true);
        hold_and_commit(tx, "stall", env_u64("STALL_MS", 8000))?;
    }
    Ok(())
}

struct Line {
    hold: bool,
    kind: String,
    start: f64,
    wait: f64,
    ok: bool,
}

fn summary(kind: &str, all: &[Line], epoch: f64) {
    let lines: Vec<&Line> = all.iter().filter(|l| !l.hold).collect();
    let mut waits: Vec<f64> = lines
        .iter()
        .filter(|l| l.kind == kind)
        .map(|l| l.wait)
        .collect();
    if waits.is_empty() {
        return;
    }
    let gave_up = lines.iter().filter(|l| l.kind == kind && !l.ok).count();
    waits.sort_by(f64::total_cmp);
    let at = |p: f64| waits[((waits.len() - 1) as f64 * p).round() as usize];
    let last_done = lines
        .iter()
        .filter(|l| l.kind == kind && l.ok)
        .map(|l| l.start + l.wait - epoch)
        .fold(0.0, f64::max);
    println!(
        "{kind:<9} n={:5} gave_up={:4} wait ms p50={:8.1} p90={:8.1} p99={:8.1} max={:8.1} last_ok_at={:7.0}",
        waits.len(),
        gave_up,
        at(0.5),
        at(0.9),
        at(0.99),
        waits[waits.len() - 1],
        last_done
    );
}

fn spawn(exe: &Path, args: &[String]) -> Result<Child> {
    Command::new(exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning a writer")
}

/// User + system CPU seconds of the children reaped so far.
fn children_cpu() -> f64 {
    // SAFETY: getrusage writes into the zeroed struct we pass and keeps no
    // pointer to it.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage);
        usage
    };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

fn parent(dir: PathBuf) -> Result<()> {
    ensure!(
        !dir.join("index.db").exists(),
        "{} already has an index: give a new folder",
        dir.display()
    );
    std::fs::create_dir_all(&dir)?;
    {
        let db = kache_store::open_index_db(&dir.join("index.db"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS storm_rows(id INTEGER PRIMARY KEY, who TEXT, payload BLOB)",
        )?;
    }
    let exe = std::env::current_exe()?;
    let d = dir.display().to_string();
    let seconds = env_u64("STORM_SECONDS", 30);
    let epoch = now_ms() + 500.0;
    let end = epoch + seconds as f64 * 1000.0;
    let mut children = vec![
        spawn(&exe, &["staller".into(), d.clone(), format!("{epoch}")])?,
        spawn(&exe, &["daemon".into(), d.clone(), format!("{end}")])?,
    ];
    let stampers = env_u64("STAMPERS", 4).max(1);
    let stamp_rate = env_u64("STAMP_PER_SEC", 100) as f64;
    if stamp_rate > 0.0 {
        for _ in 0..stampers {
            let rate = stamp_rate / stampers as f64;
            children.push(spawn(
                &exe,
                &[
                    "stamps".into(),
                    d.clone(),
                    format!("{end}"),
                    format!("{rate}"),
                ],
            )?);
        }
    }
    // Registrations: a steady stream, and a pile during the first stall.
    let mut rng = Rng::new();
    let rate = env_u64("REG_PER_SEC", 20) as f64;
    let stall_at = env_u64("STALL_AT_MS", 1000) as f64;
    let stall_ms = env_u64("STALL_MS", 8000) as f64;
    let mut due: Vec<f64> = Vec::new();
    if rate > 0.0 {
        let mut t = epoch;
        loop {
            t += rng.gap(rate).as_secs_f64() * 1000.0;
            if t >= end {
                break;
            }
            due.push(t);
        }
    }
    for _ in 0..env_u64("PILE", 120) {
        due.push(epoch + stall_at + 50.0 + (rng.next() % (stall_ms.max(100.0) as u64 - 50)) as f64);
    }
    due.sort_by(f64::total_cmp);
    let mut registrations = Vec::new();
    for t in due {
        let left = t - now_ms();
        if left > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(left / 1000.0));
        }
        registrations.push(spawn(&exe, &["register".into(), d.clone()])?);
    }
    children.extend(registrations);
    let mut lines = Vec::new();
    let mut ends: std::collections::BTreeMap<String, f64> = Default::default();
    for mut child in children {
        let out = child.stdout.take().expect("piped stdout");
        for line in BufReader::new(out).lines() {
            let line = line?;
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() == 3 && f[0] == "E" {
                let end: f64 = f[2].parse::<f64>()? - epoch;
                let latest = ends.entry(f[1].to_owned()).or_insert(end);
                *latest = latest.max(end);
            }
            if f.len() == 4 && f[0] == "H" {
                lines.push(Line {
                    hold: true,
                    kind: f[1].to_owned(),
                    start: f[2].parse()?,
                    wait: f[3].parse()?,
                    ok: true,
                });
            }
            if f.len() == 5 && f[0] == "R" {
                lines.push(Line {
                    hold: false,
                    kind: f[1].to_owned(),
                    start: f[2].parse()?,
                    wait: f[3].parse()?,
                    ok: f[4] == "ok",
                });
            }
        }
        let status = child.wait()?;
        ensure!(status.success(), "a writer failed: {status}");
    }
    let wall = (now_ms() - epoch) / 1000.0;
    println!(
        "mode={} seconds={seconds} wall={wall:.1}s writer_cpu={:.2}s",
        if sqlite_mode() { "sqlite" } else { "kache" },
        children_cpu()
    );
    for kind in ["register", "batch", "stamp", "stall"] {
        summary(kind, &lines, epoch);
    }
    let ends: Vec<String> = ends.iter().map(|(k, v)| format!("{k} {:.0}", v)).collect();
    println!("last exit ms: {}", ends.join(", "));
    // Registrations that were waiting when the first stall let go: in what
    // order did they get the lock?
    let holds: Vec<&Line> = lines.iter().filter(|l| l.hold).collect();
    let lines: Vec<&Line> = lines.iter().filter(|l| !l.hold).collect();
    let release = lines
        .iter()
        .find(|l| l.kind == "stall")
        .map(|l| l.start + l.wait + stall_ms);
    if let Some(release) = release {
        let mut piled: Vec<&Line> = lines
            .iter()
            .copied()
            .filter(|l| {
                l.kind == "register" && l.ok && l.start < release && l.start + l.wait > release
            })
            .collect();
        piled.sort_by(|a, b| a.start.total_cmp(&b.start));
        let n = piled.len();
        if n > 1 {
            // Pairs taken in the order they came (Kendall's tau-like share).
            let mut in_order = 0u64;
            let mut pairs = 0u64;
            for i in 0..n {
                for j in i + 1..n {
                    pairs += 1;
                    if piled[i].start + piled[i].wait <= piled[j].start + piled[j].wait {
                        in_order += 1;
                    }
                }
            }
            let drained = piled
                .iter()
                .map(|l| l.start + l.wait - release)
                .fold(0.0, f64::max);
            // How much of the drain the lock was held (a write's hold
            // clipped to the window).
            let (a, b) = (release, release + drained);
            let held: f64 = holds
                .iter()
                .map(|h| (h.start + h.wait).min(b) - h.start.max(a))
                .filter(|x| *x > 0.0)
                .sum();
            // The free gaps between holds in the drain, by size.
            let mut spans: Vec<(f64, f64)> = holds
                .iter()
                .map(|h| (h.start, h.start + h.wait))
                .filter(|(s, e)| *e > a && *s < b)
                .collect();
            spans.sort_by(|x, y| x.0.total_cmp(&y.0));
            let mut gaps = [(0u32, 0.0f64); 4];
            let mut end = a;
            for (s, e) in spans {
                let gap = s - end;
                if gap > 0.0 {
                    let class = match gap {
                        g if g < 5.0 => 0,
                        g if g < 50.0 => 1,
                        g if g < 250.0 => 2,
                        _ => 3,
                    };
                    gaps[class].0 += 1;
                    gaps[class].1 += gap;
                }
                end = end.max(e);
            }
            println!(
                "free gaps in the drain: <5 ms {} ({:.0} ms), 5-50 ms {} ({:.0} ms), 50-250 ms {} ({:.0} ms), >=250 ms {} ({:.0} ms)",
                gaps[0].0,
                gaps[0].1,
                gaps[1].0,
                gaps[1].1,
                gaps[2].0,
                gaps[2].1,
                gaps[3].0,
                gaps[3].1
            );
            println!(
                "after the stall: {n} waiting registrations, {:.0} % of pairs served in arrival order, the last {:.0} ms after it let go, lock held {:.0} % of that",
                in_order as f64 * 100.0 / pairs as f64,
                drained,
                held * 100.0 / drained.max(1.0)
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = |i: usize| -> Result<PathBuf> { Ok(PathBuf::from(args.get(i).context("DIR")?)) };
    let num = |i: usize| -> Result<f64> { Ok(args.get(i).context("number")?.parse()?) };
    let ran = |kind: &str, result: Result<()>| {
        println!("E {kind} {:.1}", now_ms());
        result
    };
    match args.first().map(String::as_str) {
        Some("register") => ran("register", child_register(&dir(1)?)),
        Some("daemon") => ran("daemon", child_daemon(&dir(1)?, num(2)?)),
        Some("stamps") => ran("stamps", child_stamps(&dir(1)?, num(2)?, num(3)?)),
        Some("staller") => ran("staller", child_staller(&dir(1)?, num(2)?)),
        Some(_) => parent(dir(0)?),
        None => bail!("usage: lock-storm DIR (settings in the environment; see the file's header)"),
    }
}
