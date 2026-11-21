//! A hit found by a compile that starts before its key never waits for
//! compile capacity.
//!
//! A unit whose closure this checkout has no record of, while the store holds
//! the unit (another checkout built it), compiles first and keys from the
//! dep-info rustc writes, stopping on a hit. That compile took its permit
//! before it knew it was a hit, so on a full pool the hit waited for compile
//! capacity it never used: on a 64-thread Windows machine whose memory
//! pressure ceiling made every compile ask for the whole pool, 30 minutes for
//! an entry that compiles in half a second. With every permit slot held, the
//! second checkout's unit restores without waiting for one.

#![cfg(target_os = "linux")]

use kache_store::StoreLock;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod common;
use common::{build_kache, hermetic_command, isolated_config_path, kache_binary};

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A lib `pooled` that another lib uses, so Cargo pipelines it: rustc reports
/// its dep-info as an artifact, which a compile that starts before its key
/// needs to stop on a hit.
fn write_fixture(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"pooled\", \"user\"]\nresolver = \"2\"\n",
    );
    write(
        &root.join("pooled/Cargo.toml"),
        "[package]\nname = \"pooled\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &root.join("pooled/src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n",
    );
    write(
        &root.join("user/Cargo.toml"),
        "[package]\nname = \"user\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\npooled = { path = \"../pooled\" }\n",
    );
    write(
        &root.join("user/src/lib.rs"),
        "pub fn twice() -> u32 {\n    pooled::answer() * 2\n}\n",
    );
}

/// Build `workspace`; past `limit`, kill the build and fail with the wrapper's log.
fn build(workspace: &Path, cache: &Path, limit: Duration) {
    let log = workspace.with_extension("log");
    let mut child = hermetic_command("cargo", cache, Some(&isolated_config_path(cache)))
        .args(["build", "--offline", "--workspace"])
        .current_dir(workspace)
        .env("RUSTC_WRAPPER", kache_binary())
        .env("CARGO_TARGET_DIR", workspace.join("target"))
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TERM_COLOR", "never")
        .env("KACHE_BASE_DIR", workspace)
        .env("KACHE_LOG", "off")
        .env("KACHE_LOG_FILE", "kache::wrapper=debug,kache::scheduler=debug")
        .env("KACHE_LOG_FILE_PATH", &log)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("KACHE_DISABLED")
        .env_remove("KACHE_VERIFY_INPUT_PREDICTIONS")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "building {} took over {limit:?}; the wrapper's log:\n{}",
                workspace.display(),
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        status.success(),
        "building {} failed:\n{stderr}",
        workspace.display()
    );
}

/// The last event of `pooled` built in `checkout`.
fn last_event(cache: &Path, checkout: &Path) -> Value {
    std::fs::read_to_string(cache.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .rfind(|event| {
            event["crate_name"] == "pooled"
                && event["root"]
                    .as_str()
                    .is_some_and(|root| Path::new(root).starts_with(checkout))
        })
        .unwrap_or_else(|| panic!("no event for pooled in {}", checkout.display()))
}

#[test]
fn a_hit_found_at_the_dep_info_takes_no_permit_from_a_full_pool() {
    build_kache();
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let cache = base.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), &cache);
    let (a, b) = (base.join("a"), base.join("b"));
    write_fixture(&a);
    write_fixture(&b);
    // A file the compile never reads changes `b`'s source tree, so the closure
    // recorded from `a` is not trusted there: `b` compiles before its key and
    // stops on the hit at the dep-info (the "tree-changed" prediction).
    write(&b.join("pooled/src/unused.rs"), "// not a module\n");
    build(&a, &cache, Duration::from_secs(240));
    common::wait_for_daemon_publishes(&cache);

    // Every slot of the pool held, as by compiles that run for minutes.
    let permits = cache.join("scheduler").join("permits");
    std::fs::create_dir_all(&permits).unwrap();
    let pool = std::thread::available_parallelism().map_or(1, |n| n.get());
    let held: Vec<StoreLock> = (0..pool)
        .map(|index| {
            StoreLock::try_acquire(&permits.join(index.to_string()))
                .unwrap()
                .expect("a free slot")
        })
        .collect();

    build(&b, &cache, Duration::from_secs(60));
    drop(held);

    let event = last_event(&cache, &b);
    let log = std::fs::read_to_string(b.with_extension("log")).unwrap_or_default();
    assert_eq!(event["result"], "local_hit", "{event}\n{log}");
    assert!(
        log.contains("pooled: the pool is busy; keying with the pre-pass"),
        "pooled did not take the path under test:\n{log}"
    );
    let waited = event["permit_wait_ms"].as_u64().unwrap_or(0);
    assert!(
        waited < 5_000,
        "the hit waited {waited} ms for a compile permit: {event}\n{log}"
    );
}
