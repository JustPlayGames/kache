//! Two checkouts compiling the same unit at once, each before its key.
//!
//! A unit whose predicted key missed compiles first and keys from the
//! dep-info rustc writes. Two checkouts that reach such a miss together both
//! found nothing stored and nothing being stored at their dep-info, so both
//! compiled the unit to its end and the second's store found the first's
//! entry: one compile in vain (133 a day on a shared build machine, about
//! 1.5 k compile-seconds). At its dep-info a compile knows its key: the first
//! to get there takes the key's compile flight, and the second waits for it,
//! then restores the entry instead of compiling on.

#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod common;
use common::{build_kache, hermetic_command, isolated_config_path, kache_binary};

/// What the wrapper logs when a compile reaches its dep-info while a peer
/// compiles the same key.
const WAITING: &str = "a peer compiles its key; waiting for it";

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A workspace lib `slow` (a compile that does not link) with a long codegen,
/// on a small path dependency `dep` whose edit changes `slow`'s key but not
/// its closure, so its recorded prediction holds and its lookup misses.
fn write_fixture(root: &Path, answer: u32) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"slow\", \"dep\"]\nresolver = \"2\"\n\n\
         [profile.dev.package.slow]\nopt-level = 2\n",
    );
    write(
        &root.join("dep/Cargo.toml"),
        "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &root.join("dep/src/lib.rs"),
        &format!("pub const ANSWER: u32 = {answer};\n"),
    );
    write(
        &root.join("slow/Cargo.toml"),
        "[package]\nname = \"slow\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ndep = { path = \"../dep\" }\n",
    );
    write(&root.join("slow/src/lib.rs"), &slow_codegen());
}

/// Seconds of optimized codegen after a dep-info that comes at once.
fn slow_codegen() -> String {
    const N: usize = 2500;
    let functions: String = (0..N)
        .map(|i| {
            format!(
                "#[inline(never)]\npub fn f{i}(x: u64) -> u64 {{\n    \
                 let mut y = x ^ {i} ^ u64::from(dep::ANSWER);\n    \
                 for i in 0..64u64 {{\n        \
                 y = y.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left((i % 63) as u32) ^ i;\n    \
                 }}\n    y\n}}\n"
            )
        })
        .collect();
    let table: Vec<String> = (0..N).map(|i| format!("f{i}")).collect();
    format!(
        "{functions}pub static FNS: [fn(u64) -> u64; {N}] = [{}];\n",
        table.join(", ")
    )
}

fn cargo_build(workspace: &Path, cache: &Path, log: &Path) -> Command {
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
        .env("KACHE_LOG_FILE", "kache::wrapper=debug")
        .env("KACHE_LOG_FILE_PATH", log)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("KACHE_DISABLED")
        .env_remove("KACHE_DAEMON_PUBLISH_RUSTC")
        .env_remove("KACHE_VERIFY_INPUT_PREDICTIONS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

fn build(workspace: &Path, cache: &Path, log: &Path) {
    let output = cargo_build(workspace, cache, log).output().unwrap();
    assert_built(&output, &workspace.display().to_string());
}

fn log_has(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log).is_ok_and(|log| log.contains(needle))
}

fn read(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// `slow`'s results in the event log since line `from`, by checkout.
fn slow_results(cache: &Path, from: usize, base: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(cache.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .skip(from)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["crate_name"] == "slow" && event.get("result").is_some())
        .map(|event| {
            let root = event["root"].as_str().unwrap_or_default();
            let checkout = Path::new(root)
                .strip_prefix(base)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| root.to_string());
            (
                checkout,
                event["result"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn event_lines(cache: &Path) -> usize {
    std::fs::read_to_string(cache.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .count()
}

struct Race {
    /// `slow`'s results while both checkouts built it after `dep` changed.
    results: Vec<(String, String)>,
    /// Whether either checkout's `slow` waited at its dep-info for the other.
    waited: bool,
    logs: String,
}

/// Build `b` and `c` once, one after the other, so each records `slow`'s
/// closure; then change `dep` in both and build them together, starting `c`
/// once `b`'s `slow` is compiling before its key.
fn race() -> Race {
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let cache = base.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), &cache);
    for name in ["b", "c"] {
        write_fixture(&base.join(name), 1);
    }
    build(&base.join("b"), &cache, &base.join("b0.log"));
    build(&base.join("c"), &cache, &base.join("c0.log"));
    common::wait_for_daemon_publishes(&cache);

    for name in ["b", "c"] {
        write_fixture(&base.join(name), 2);
    }
    let from = event_lines(&cache);
    let (b_log, c_log) = (base.join("b.log"), base.join("c.log"));
    let b: Child = cargo_build(&base.join("b"), &cache, &b_log)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !log_has(
        &b_log,
        "slow: no lane took the miss; compiling before keying",
    ) && started.elapsed() < Duration::from_secs(120)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let c = cargo_build(&base.join("c"), &cache, &c_log)
        .spawn()
        .unwrap();
    assert_built(&b.wait_with_output().unwrap(), "b");
    assert_built(&c.wait_with_output().unwrap(), "c");
    common::wait_for_daemon_publishes(&cache);
    Race {
        results: slow_results(&cache, from, &base),
        waited: log_has(&b_log, &format!("slow: {WAITING}"))
            || log_has(&c_log, &format!("slow: {WAITING}")),
        logs: format!("b:\n{}\nc:\n{}", read(&b_log), read(&c_log)),
    }
}

/// How many races the test runs before it gives up on seeing one overlap: on
/// a starved machine `b` may finish `slow` before `c` reaches its dep-info,
/// and then `c` simply hits.
const ATTEMPTS: usize = 3;

#[test]
fn two_checkouts_compiling_one_key_before_it_is_known_compile_it_once() {
    build_kache();
    let mut tries = Vec::new();
    for _ in 0..ATTEMPTS {
        let race = race();
        let compiled: Vec<&(String, String)> = race
            .results
            .iter()
            .filter(|(_, result)| matches!(result.as_str(), "miss" | "dup" | "skipped"))
            .collect();
        assert_eq!(
            compiled.len(),
            1,
            "slow was compiled {} times by two checkouts: {:?}\n{}",
            compiled.len(),
            race.results,
            race.logs
        );
        assert!(
            race.results.iter().any(|(_, result)| result == "local_hit"),
            "the other checkout did not restore slow: {:?}",
            race.results
        );
        if race.waited {
            return;
        }
        tries.push(format!("{:?}", race.results));
    }
    panic!(
        "slow never reached its dep-info while the other checkout compiled it: {}",
        tries.join("; ")
    );
}
