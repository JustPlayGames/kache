//! A daemon killed while it runs a compile it was handed: the build goes on.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;

const FRONTEND: &str = env!("CARGO_BIN_EXE_kache");

/// The fake compiler: its first compile writes a half output and an `.rmeta`
/// and then runs until it is killed; the next one records what it found and
/// writes the whole output.
const COMPILER: &str = r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "-vV" ]; then
    printf 'rustc 1.99.1 (fake)\nbinary: rustc\ncommit-hash: fake\ncommit-date: 2026-10-01\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.1\nLLVM version: 21.1.0\n'
    exit 0
  fi
done
mkdir -p out
if [ ! -e "$STATE/first" ]; then
  : > "$STATE/first"
  printf partial > out/lost_unit
  printf meta > out/lost_unit.rmeta
  printf '%s' "$$" > "$STATE/started"
  i=0
  while [ $i -lt 6000 ]; do sleep .01; i=$((i+1)); done
  exit 3
fi
{
  if [ -e out/lost_unit ]; then echo output-left; else echo output-removed; fi
  if [ -e out/lost_unit.rmeta ]; then echo rmeta-kept; else echo rmeta-removed; fi
} > "$STATE/second"
printf complete > out/lost_unit
exit 0
"#;

struct Fixture {
    root: tempfile::TempDir,
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        for dir in ["home", "state"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }
        std::fs::write(
            root.path().join("config.toml"),
            "[cache]\nlocal_only=true\nauto_gc=false\nscheduler=false\nprefetch_enabled=false\n",
        )
        .unwrap();
        std::fs::write(root.path().join("input.rs"), "fn main() {}\n").unwrap();
        kache_fs::testutil::write_executable(&root.path().join("rustc"), COMPILER);
        Self { root, runtime }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(FRONTEND);
        command
            .env_clear()
            .env("PATH", env!("PATH"))
            .env("HOME", self.path("home"))
            .env("KACHE_CONFIG", self.path("config.toml"))
            .env("KACHE_HOST_CONFIG", "")
            .env("KACHE_CACHE_DIR", self.path("cache"))
            .env("KACHE_RUNTIME_DIR", &self.runtime)
            .env("KACHE_CACHE_EXECUTABLES", "false")
            .env("KACHE_LOG", "off")
            .env("KACHE_DAEMON_IDLE_TIMEOUT", "120")
            .env("STATE", self.path("state"))
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        command
    }

    fn compile(&self) -> Child {
        self.command()
            .arg(self.path("rustc"))
            .args([
                "--crate-name",
                "lost_unit",
                "input.rs",
                "--crate-type",
                "bin",
                "--emit=link",
                "-o",
                "out/lost_unit",
            ])
            .stdout(std::fs::File::create(self.path("stdout")).unwrap())
            .stderr(std::fs::File::create(self.path("stderr")).unwrap())
            .spawn()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.command().args(args).output().unwrap();
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?}: {error}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn daemon_pid(&self) -> libc::pid_t {
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.runtime.join("daemon.state.json")).unwrap())
                .unwrap();
        state["pid"].as_i64().unwrap() as libc::pid_t
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        common::stop_daemon(self.command().args(["daemon", "stop"]), &self.runtime);
    }
}

fn wait_for(path: &Path, child: &mut Child) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the compile ended before its compiler started"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the compiler never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_with_deadline(child: &mut Child, deadline: Duration) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the compile did not end after the daemon was killed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A daemon killed in the middle of a compile it was handed used to fail the
/// build with "incomplete compiler execution response". Its client now runs
/// the compile again itself, once, after removing what the lost attempt left
/// half written (all but the `.rmeta`, which dependent compiles may read).
#[test]
fn a_daemon_killed_mid_compile_leaves_the_build_successful() {
    let fixture = Fixture::new();
    let started = fixture
        .command()
        .args(["daemon", "start"])
        .status()
        .unwrap();
    assert!(started.success());
    let doctor = fixture.json(&["doctor", "--json"]);
    let handoff = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["label"] == "Compiler execution")
        .is_some_and(|check| check["pass"] == true);
    if !handoff {
        eprintln!("daemon compiler execution is unavailable here; nothing to hand over");
        return;
    }

    let mut compile = fixture.compile();
    wait_for(&fixture.path("state/started"), &mut compile);
    let daemon = fixture.daemon_pid();
    assert_eq!(unsafe { libc::kill(daemon, libc::SIGKILL) }, 0);

    let exit = wait_with_deadline(&mut compile, Duration::from_secs(60));
    let stderr = std::fs::read_to_string(fixture.path("stderr")).unwrap();
    assert!(exit.success(), "{exit}: {stderr}");
    assert_eq!(
        std::fs::read_to_string(fixture.path("out/lost_unit")).unwrap(),
        "complete"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path("state/second")).unwrap(),
        "output-removed\nrmeta-kept\n"
    );
    // The lost attempt's compiler was killed with its client's broker, not
    // left running beside the second one.
    let first: libc::pid_t = std::fs::read_to_string(fixture.path("state/started"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(first, 0) },
        -1,
        "first compiler still runs"
    );
}
