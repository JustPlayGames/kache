//! No test daemon outlives its test, even a test killed mid-way whose runtime
//! directory stays behind.
//!
//! A killed test runs no `Drop`, so its fixture cannot stop the daemon, and a
//! runtime directory that stays (a fixed scratch folder, a private `HOME`)
//! keeps the daemon's socket, so the socket watch never fires either. One such
//! daemon ran for 22 hours on a build machine. The daemon belongs to the test
//! process instead (`KACHE_DAEMON_OWNER_PID`, set by the harness) and drains
//! once that process is gone.

mod common;

use common::{
    DAEMON_OWNER_PID_ENV, DaemonGuard, build_kache, hermetic_command, isolated_config_path,
    kache_binary, wait_for_daemon_exit,
};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the helper starts its daemon; unset, the helper does nothing.
const HELPER_DIR_ENV: &str = "KACHE_TEST_OWNER_HELPER_DIR";
const HELPER: &str = "helper_starts_a_daemon_and_hangs";

/// The test the check below kills: it starts a daemon the way any test does
/// and then hangs, as a test does until nextest's timeout kills it.
#[test]
#[ignore = "run by a_killed_test_leaves_no_daemon, in a process of its own"]
fn helper_starts_a_daemon_and_hangs() {
    let Some(dir) = std::env::var_os(HELPER_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let status = hermetic_command(kache_binary(), &dir, Some(&isolated_config_path(&dir)))
        .args(["daemon", "start"])
        // No idle backstop: only the owner's end may stop this daemon.
        .env("KACHE_DAEMON_IDLE_TIMEOUT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("running kache daemon start");
    assert!(status.success(), "kache daemon start failed: {status}");
    std::fs::write(dir.join("helper.ready"), b"").unwrap();
    std::thread::sleep(Duration::from_secs(600));
}

#[test]
fn a_killed_test_leaves_no_daemon() {
    build_kache();
    let dir = tempfile::TempDir::new().unwrap();
    // Stops the daemon if this check fails, before the folder goes.
    let _guard = DaemonGuard::new(kache_binary(), dir.path());

    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([HELPER, "--exact", "--ignored", "--nocapture"])
        .env(HELPER_DIR_ENV, dir.path())
        // The helper claims the daemon for itself, as every test process does.
        .env_remove(DAEMON_OWNER_PID_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting the helper test");

    let deadline = Instant::now() + Duration::from_secs(120);
    while !dir.path().join("helper.ready").exists() {
        if let Some(status) = helper.try_wait().unwrap() {
            panic!("the helper ended before its daemon was up: {status}");
        }
        if Instant::now() >= deadline {
            let _ = helper.kill();
            let _ = helper.wait();
            panic!("the helper's daemon was not up after 120 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        wait_for_daemon_exit(dir.path(), Duration::ZERO).is_err(),
        "the helper reported a daemon, but none holds the run lock"
    );

    // What nextest does to a test past its timeout.
    helper.kill().unwrap();
    helper.wait().unwrap();
    let killed = Instant::now();

    // The folder, and with it the daemon's socket, stays until the end.
    wait_for_daemon_exit(dir.path(), Duration::from_secs(20)).unwrap_or_else(|error| {
        panic!(
            "a daemon outlived its killed test by {:?}, its folder still there: {error}",
            killed.elapsed()
        )
    });
}
