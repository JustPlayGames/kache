//! A unit looked up while a peer's entry for it is still being stored.
//!
//! When the index is busy, a wrapper hands its finished compile to the
//! daemon and returns to Cargo; the daemon holds the key until the index
//! lets it commit the entry. A checkout built in that window found no
//! entry, and a unit that compiles before its key (no closure record yet)
//! compiled again instead of waiting: a missed hit, one run in two of the
//! OUT_DIR alias test under load. Here the test holds the index's write
//! lock itself, so every hand-off of the first checkout stays in flight
//! until the second checkout's units are seen waiting for them.

#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod common;
use common::{build_kache, hermetic_command, isolated_config_path, kache_binary};

/// What the wrapper logs when a lookup finds the key's entry being stored.
const WAITING: &str = "its entry is being stored; waiting for the commit";

/// What it logs when a unit that links, the index having never held it,
/// finds another checkout's entry of it being stored, and waits for that
/// before it compiles at all.
const WAITING_FOR_UNIT: &str = "another checkout's entry is being stored; waiting for it";

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn manifest(name: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}")
}

/// A proc macro (a compile that links, which waits before it starts), shaped like a
/// registry package, and a workspace lib (a compile that does not link)
/// that uses it.
fn write_fixture(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
    );
    let fixture = root.join("registry/src/fixture");
    write(
        &fixture.join("pm/Cargo.toml"),
        &manifest("pm", "\n[lib]\nproc-macro = true\n"),
    );
    // Long enough a codegen that its dep-info reaches the wrapper well
    // before rustc exits, as in any crate of some size: a stop needs rustc
    // still running.
    write(
        &fixture.join("pm/src/lib.rs"),
        &format!(
            "use proc_macro::TokenStream;\n\n#[proc_macro]\n\
             pub fn answer(_input: TokenStream) -> TokenStream {{\n    \
             std::hint::black_box(&FNS);\n    \"42\".parse().unwrap()\n}}\n{}",
            slow_codegen()
        ),
    );
    write(
        &root.join("app/Cargo.toml"),
        &manifest(
            "app",
            "\n[dependencies]\npm = { path = \"../registry/src/fixture/pm\" }\n",
        ),
    );
    write(
        &root.join("app/src/lib.rs"),
        "pub fn value() -> u32 {\n    pm::answer!()\n}\n",
    );
}

fn slow_codegen() -> String {
    const N: usize = 1500;
    let functions: String = (0..N)
        .map(|i| {
            format!(
                "#[inline(never)]\nfn f{i}(x: u64) -> u64 {{\n    let mut y = x ^ {i};\n    \
                 for i in 0..64u64 {{\n        \
                 y = y.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left((i % 63) as u32) ^ i;\n    \
                 }}\n    y\n}}\n"
            )
        })
        .collect();
    let table: Vec<String> = (0..N).map(|i| format!("f{i}")).collect();
    format!(
        "{functions}static FNS: [fn(u64) -> u64; {N}] = [{}];\n",
        table.join(", ")
    )
}

fn cargo_build(workspace: &Path, cache: &Path, log: Option<&Path>) -> Command {
    let mut command = hermetic_command("cargo", cache, Some(&isolated_config_path(cache)));
    command
        .args(["build", "--offline", "--workspace"])
        .current_dir(workspace)
        .env("RUSTC_WRAPPER", kache_binary())
        .env("CARGO_TARGET_DIR", workspace.join("target"))
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TERM_COLOR", "never")
        .env("KACHE_BASE_DIR", workspace)
        .env("KACHE_LOG", "off")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("KACHE_DISABLED")
        .env_remove("KACHE_LOG_FILE")
        .env_remove("KACHE_DAEMON_PUBLISH_RUSTC")
        .env_remove("KACHE_VERIFY_INPUT_PREDICTIONS");
    if let Some(log) = log {
        command
            .env("KACHE_LOG_FILE", "kache::wrapper=debug,kache::compile=debug")
            .env("KACHE_LOG_FILE_PATH", log);
    }
    command
}

fn assert_built(output: &Output, tree: &str) {
    assert!(
        output.status.success(),
        "building {tree} failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The compiles' results in the event log, without the heartbeats a compile
/// past 30 s writes there (a starved machine's do).
fn events(cache: &Path) -> Vec<Value> {
    std::fs::read_to_string(cache.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("result").is_some())
        .collect()
}

/// Whether `pm` in the build logging to `log` waited for its entry being
/// stored.
fn waited(log: &Path) -> bool {
    std::fs::read_to_string(log).is_ok_and(|log| {
        log.contains(&format!("pm: {WAITING}")) || log.contains(&format!("pm: {WAITING_FOR_UNIT}"))
    })
}

/// Whether `pm` in the build logging to `log` waits for another process:
/// for its entry being stored, or, after compiling it, for its key's claim.
fn waits(log: &Path) -> bool {
    std::fs::read_to_string(log).is_ok_and(|log| {
        log.contains(&format!("pm: {WAITING}"))
            || log.contains(&format!("pm: {WAITING_FOR_UNIT}"))
            || log.contains("waiting for pm to be built by another process")
    })
}

/// A cache whose daemon holds `b`'s two entries, handed to it while the test
/// holds the index's write lock. Dropped in field order: the lock goes with
/// its connection, then the daemon, then the folder.
struct InFlight {
    index: rusqlite::Connection,
    base: PathBuf,
    cache: PathBuf,
    _daemon: common::DaemonGuard,
    _tmp: TempDir,
}

/// How many times the test builds `b` for its hand-offs before it fails.
const ATTEMPTS: usize = 4;

/// Build `b` while the index is held, so that every store of it hands its
/// entry to the daemon, which holds each key until the index frees. `Err`
/// says why the daemon does not hold both: one that answers past the
/// wrapper's hand-off budget, as a CPU-starved one does, leaves the wrapper
/// to store the entry itself, as designed, and nothing in flight for `c`.
fn hand_off_b() -> Result<InFlight, String> {
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let cache = base.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let daemon = common::DaemonGuard::new(kache_binary(), &cache);
    for name in ["b", "c"] {
        write_fixture(&base.join(name));
    }

    // The index exists and is current before anything holds it, so opening
    // it needs no write lock.
    let index_path = cache.join("index.db");
    drop(kache_store::open_index_db(&index_path).unwrap());
    let index = rusqlite::Connection::open(&index_path).unwrap();
    index.execute_batch("BEGIN IMMEDIATE").unwrap();

    let b = cargo_build(&base.join("b"), &cache, None)
        .output()
        .unwrap();
    assert_built(&b, "b");
    // A wrapper that stored its entry itself logged its event before it
    // exited; the daemon logs a hand-off's once it is stored.
    let done: Vec<Value> = events(&cache)
        .into_iter()
        .filter(|event| event["crate_name"] == "pm" || event["crate_name"] == "app")
        .collect();
    let held = common::daemon_publishes_pending(&cache);
    if !done.is_empty() || held.is_none_or(|held| held < 2) {
        return Err(format!(
            "the daemon holds {held:?} of b's 2 entries; logged: {done:?}"
        ));
    }
    Ok(InFlight {
        index,
        base,
        cache,
        _daemon: daemon,
        _tmp: tmp,
    })
}

#[test]
fn a_unit_looked_up_while_its_entry_is_handed_off_waits_for_it() {
    build_kache();
    let mut missed = Vec::new();
    let flight = loop {
        match hand_off_b() {
            Ok(flight) => break flight,
            Err(why) => {
                eprintln!("b's hand-offs, attempt {}: {why}", missed.len() + 1);
                missed.push(why);
            }
        }
        assert!(
            missed.len() < ATTEMPTS,
            "b's entries never all waited for the index:\n{}",
            missed.join("\n")
        );
    };
    let (index, base, cache) = (&flight.index, &flight.base, &flight.cache);

    // `c` runs while those hand-offs wait for the index; each unit must wait
    // for its entry instead of compiling it again.
    let log = base.join("c.log");
    let mut c = cargo_build(&base.join("c"), cache, Some(&log))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !waits(&log)
        && c.try_wait().unwrap().is_none()
        && started.elapsed() < Duration::from_secs(60)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let seen_waiting = waited(&log);
    // Let the daemon commit what `b` handed it.
    index.execute_batch("ROLLBACK").unwrap();
    let c = c.wait_with_output().unwrap();
    assert_built(&c, "c");

    let events = events(cache);
    for krate in ["pm", "app"] {
        let stored: Vec<&Value> = events
            .iter()
            .filter(|event| event["crate_name"] == krate && event["result"] == "miss")
            .collect();
        assert_eq!(stored.len(), 1, "{krate} was not stored once: {stored:?}");
        let in_c: Vec<&Value> = events
            .iter()
            .filter(|event| {
                event["crate_name"] == krate
                    && event["root"]
                        .as_str()
                        .is_some_and(|root| Path::new(root) == base.join("c"))
            })
            .collect();
        assert!(
            !in_c.is_empty() && in_c.iter().all(|event| event["result"] == "local_hit"),
            "{krate} ran rustc in c: {in_c:?}\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
    assert!(
        seen_waiting,
        "c's pm did not wait for b's entry in flight:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}
