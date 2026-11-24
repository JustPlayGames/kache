//! Adaptive incremental mode for workspace crates the Cargo command does not
//! select.

use filetime::FileTime;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// Only the env plumbing is shared; this file drives the Cargo-built binary,
// so the bootstrap helpers in `common` stay unused here.
#[allow(dead_code)]
mod common;
use common::{hermetic_command, settle_writes};

fn kache_binary() -> std::path::PathBuf {
    common::bin_exe("kache")
}

/// Events Kache recorded for `crate_name`.
fn crate_events(cache_dir: &Path, crate_name: &str) -> Vec<Value> {
    common::settled_events(cache_dir)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["crate_name"] == crate_name)
        .collect()
}

fn assert_passthrough(event: &Value, reason: &str) {
    assert_eq!(event["result"], "passthrough", "event: {event:#}");
    assert!(
        event["passthrough_reason"]
            .as_str()
            .is_some_and(|value| value.contains(reason)),
        "expected passthrough reason containing {reason:?}; event: {event:#}",
    );
}

/// A virtual workspace whose binary `adaptive-app` prints what `adaptive-lib`
/// returns through `adaptive-mid`.
fn create_workspace(project: &Path) {
    for member in ["lib", "mid", "app"] {
        fs::create_dir_all(project.join(member).join("src")).unwrap();
    }
    let package = |name: &str, dependency: Option<(&str, &str)>| {
        let mut manifest =
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        if let Some((dependency, path)) = dependency {
            manifest.push_str(&format!(
                "\n[dependencies]\n{dependency} = {{ path = \"{path}\" }}\n"
            ));
        }
        manifest
    };
    fs::write(
        project.join("Cargo.toml"),
        "[workspace]\nmembers = [\"lib\", \"mid\", \"app\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        project.join("lib/Cargo.toml"),
        package("adaptive-lib", None),
    )
    .unwrap();
    fs::write(
        project.join("mid/Cargo.toml"),
        package("adaptive-mid", Some(("adaptive-lib", "../lib"))),
    )
    .unwrap();
    fs::write(
        project.join("mid/src/lib.rs"),
        "pub fn answer() -> u64 { adaptive_lib::answer() }\n",
    )
    .unwrap();
    fs::write(
        project.join("app/Cargo.toml"),
        package("adaptive-app", Some(("adaptive-mid", "../mid"))),
    )
    .unwrap();
    fs::write(
        project.join("app/src/main.rs"),
        "fn main() { print!(\"{}\", adaptive_mid::answer()); }\n",
    )
    .unwrap();
}

/// `dir` spelled as Cargo spells the package directories in it. Cargo
/// resolves its working directory, and macOS reaches the temporary directory
/// through the `/var` link, so a target directory under the unresolved
/// spelling would not lie inside the workspace Kache finds, and the unit's
/// records would get no workspace guard. Windows keeps the spelling it was
/// given.
fn checkout_path(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.to_path_buf()
    } else {
        dir.canonicalize().unwrap()
    }
}

/// Keep a remote configured in the developer's shell out of the fixture.
fn remove_remote_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("KACHE_S3_") || name.starts_with("AWS_"))
        {
            command.env_remove(name);
        }
    }
}

/// Remove `crate_name`'s cache entries, as an eviction would.
fn purge_crate(project: &Path, cache_dir: &Path, crate_name: &str) {
    let mut command = hermetic_command(
        kache_binary(),
        cache_dir,
        Some(&project.join("missing-kache.toml")),
    );
    command.args(["clean", "--crate", crate_name, "--yes"]);
    remove_remote_env(&mut command);
    let output = command.output().expect("failed to run kache clean");
    assert!(
        output.status.success(),
        "kache clean --crate {crate_name} failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `cargo build -p <package>` in `project` through Kache, local only, with
/// every setting that changes adaptive mode, deferral or what is stored left
/// at its default.
fn build_command(project: &Path, cache_dir: &Path, target_dir: &Path, package: &str) -> Command {
    let mut command = hermetic_command(
        "cargo",
        cache_dir,
        Some(&project.join("missing-kache.toml")),
    );
    command
        .args(["build", "--offline", "--quiet", "-p", package])
        .current_dir(project)
        .env("RUSTC_WRAPPER", kache_binary())
        .env("CARGO_TARGET_DIR", target_dir)
        .env("CARGO_INCREMENTAL", "1")
        .env("KACHE_LOCAL_ONLY", "1");
    for name in [
        // Cargo sets it for selected units only; an inherited value would
        // reach every unit and hide what this test is about.
        "CARGO_PRIMARY_PACKAGE",
        "RUSTC_WORKSPACE_WRAPPER",
        "KACHE_ADAPTIVE_INCREMENTAL",
        "KACHE_CLEAN_INCREMENTAL",
        "KACHE_DEFERRED_DISCOVERY",
        "KACHE_DISABLED",
        "KACHE_FALLBACK",
        "KACHE_INCREMENTAL_CRATES",
        "KACHE_MIN_STORE_COMPILE_MS",
        "KACHE_MODIFIED_INPUT_GUARD",
        "KACHE_PRESERVE_INCREMENTAL",
        "KACHE_READONLY_STORE",
        "KACHE_SCHEDULER",
    ] {
        command.env_remove(name);
    }
    remove_remote_env(&mut command);
    command
}

/// Write `answer` into the library and run `cargo build -p adaptive-app`,
/// which selects neither the library nor the middle crate. Runs the app to
/// prove it linked this build's library, then returns this build's single
/// event for the library and for the middle crate.
fn build_unselected(
    project: &Path,
    cache_dir: &Path,
    target_dir: &Path,
    source_mtime: Option<i64>,
    answer: u64,
) -> (Value, Value) {
    let source = project.join("lib/src/lib.rs");
    fs::write(&source, format!("pub fn answer() -> u64 {{ {answer} }}\n")).unwrap();
    if let Some(mtime) = source_mtime {
        filetime::set_file_mtime(&source, FileTime::from_unix_time(mtime, 0)).unwrap();
    }
    // A compile that runs before its key is not stored when a source was
    // written just before the build started, either.
    settle_writes(&[project]);
    let before_lib = crate_events(cache_dir, "adaptive_lib").len();
    let before_mid = crate_events(cache_dir, "adaptive_mid").len();
    let output = build_command(project, cache_dir, target_dir, "adaptive-app")
        .output()
        .expect("failed to build the workspace fixture");
    assert!(
        output.status.success(),
        "workspace build failed for variant {answer}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let app = target_dir.join(format!(
        "debug/adaptive-app{}",
        std::env::consts::EXE_SUFFIX
    ));
    let run = Command::new(&app)
        .output()
        .expect("failed to run the workspace binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), answer.to_string());

    let one_new = |crate_name: &str, before: usize| {
        let events = crate_events(cache_dir, crate_name);
        assert_eq!(events.len(), before + 1, "{crate_name} events: {events:#?}");
        events.into_iter().nth(before).unwrap()
    };
    (
        one_new("adaptive_lib", before_lib),
        one_new("adaptive_mid", before_mid),
    )
}

/// A library the command does not select is a unit someone edits, like a
/// selected one: `cargo build -p adaptive-app` seeds it after an edit and then
/// compiles it on the active lane. The unselected crate between it and the
/// app, whose only change is the library it links, keeps its graph.
#[test]
fn unselected_workspace_crates_seed_then_stay_active() {
    let checkout = tempfile::tempdir().unwrap();
    let project = checkout_path(checkout.path());
    let cache = tempfile::tempdir().unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache.path());
    let target = project.join("target");
    create_workspace(&project);
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3_600;
    let build = |mtime: Option<i64>, answer| {
        build_unselected(&project, cache.path(), &target, mtime, answer)
    };

    // Builds without policy state keep real mtimes: such a unit compiles
    // before its key, and a source stamped after the build started would be
    // neither stored nor learned from.
    let (lib, mid) = build(None, 1);
    assert_eq!(lib["result"], "miss", "event: {lib:#}");
    assert_eq!(
        lib["dep_info_runs"], 0,
        "a unit without policy state compiles before its key: {lib:#}"
    );
    assert_eq!(mid["result"], "miss", "event: {mid:#}");

    // Future, increasing mtimes make Cargo rebuild without sleeping.
    // The crate above it builds on what that compile wrote, which no store
    // holds: it keeps its graph for good.
    let (lib, mid) = build(Some(future), 2);
    assert_passthrough(&lib, "adaptive seed");
    assert_passthrough(&mid, "incremental kept: builds on adaptive_lib");

    let (lib, mid) = build(Some(future + 1), 3);
    assert_passthrough(&lib, "adaptive active");
    assert_eq!(lib["key_ms"], 0, "event: {lib:#}");
    assert_passthrough(&mid, "incremental kept: builds on adaptive_lib");

    // With the policy state gone, the unit cannot seed. Its record from the
    // first build no longer applies, because the workspace changed since, so
    // the unit compiles before its key, as on its first build, instead of
    // paying the dep-info pre-pass first.
    fs::remove_dir_all(target.join("debug/incremental.kache-auto")).unwrap();
    let (lib, _mid) = build(None, 4);
    assert_eq!(lib["result"], "miss", "event: {lib:#}");
    assert_eq!(
        lib["dep_info_runs"], 0,
        "a unit whose policy state is gone compiles before its key: {lib:#}"
    );

    // That build taught the unit again and recorded its closure. The same
    // source leaves the workspace as that record saw it, so the record
    // predicts the key the unit was last built under, which cannot seed, and
    // its entry is gone. No lane needs the key first any more, so the unit
    // compiles while the key is re-derived.
    purge_crate(&project, cache.path(), "adaptive_lib");
    let (lib, _mid) = build(None, 4);
    assert_eq!(lib["result"], "miss", "event: {lib:#}");
    assert_eq!(
        lib["dep_info_runs"], 0,
        "a missed prediction that cannot seed compiles while re-deriving: {lib:#}"
    );
}

/// The middle crate also builds a `staticlib` and `cdylib`, so Cargo gives it
/// no extra filename. Edits to the library below it still put it in
/// incremental mode. It links what an incremental compile wrote, so no store
/// holds it (`a_link_of_incrementally_compiled_libraries_is_not_stored`): it
/// keeps its graph, with no key and no entry, a lane that only a unit with an
/// id (here its `-C metadata`) takes.
#[test]
fn an_unselected_cdylib_crate_seeds_then_stays_active() {
    let checkout = tempfile::tempdir().unwrap();
    let project = checkout_path(checkout.path());
    let cache = tempfile::tempdir().unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache.path());
    let target = project.join("target");
    create_workspace(&project);
    let manifest = project.join("mid/Cargo.toml");
    let mut contents = fs::read_to_string(&manifest).unwrap();
    contents.push_str("\n[lib]\ncrate-type = [\"lib\", \"staticlib\", \"cdylib\"]\n");
    fs::write(&manifest, contents).unwrap();
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3_600;
    let build = |mtime: Option<i64>, answer| {
        build_unselected(&project, cache.path(), &target, mtime, answer)
    };

    let (_lib, mid) = build(None, 1);
    assert_eq!(mid["result"], "miss", "event: {mid:#}");

    for (mtime, answer) in [(future, 2), (future + 1, 3)] {
        let (_lib, mid) = build(Some(mtime), answer);
        assert_passthrough(&mid, "incremental kept: builds on adaptive_lib");
    }
}

/// A link that takes an rlib an incremental compile wrote keys on bytes no
/// other build writes, so it compiles with no key and no entry, from a graph
/// it keeps. An example shows it: examples never take an incremental lane of
/// their own, so each edit used to store a new one. A fresh target builds the same sources
/// without incremental state and keys and stores the link as before, and the
/// next fresh target hits it.
#[test]
fn a_link_of_incrementally_compiled_libraries_is_not_stored() {
    let checkout = tempfile::tempdir().unwrap();
    let project = checkout_path(checkout.path());
    let cache = tempfile::tempdir().unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache.path());
    create_workspace(&project);
    fs::create_dir_all(project.join("app/examples")).unwrap();
    fs::write(
        project.join("app/examples/demo.rs"),
        "fn main() { print!(\"{}\", adaptive_mid::answer()); }\n",
    )
    .unwrap();
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3_600;
    let build = |target: &str, mtime: Option<i64>, answer: u64| {
        let source = project.join("lib/src/lib.rs");
        fs::write(&source, format!("pub fn answer() -> u64 {{ {answer} }}\n")).unwrap();
        if let Some(mtime) = mtime {
            filetime::set_file_mtime(&source, FileTime::from_unix_time(mtime, 0)).unwrap();
        }
        settle_writes(&[project.as_path()]);
        let before = |name| crate_events(cache.path(), name).len();
        let counts = ["adaptive_lib", "adaptive_mid", "demo"].map(before);
        let target = project.join(target);
        let output = build_command(&project, cache.path(), &target, "adaptive-app")
            .args(["--example", "demo"])
            .output()
            .expect("failed to build the example");
        assert!(
            output.status.success(),
            "example build failed for variant {answer}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        let run = Command::new(target.join(format!(
            "debug/examples/demo{}",
            std::env::consts::EXE_SUFFIX
        )))
        .output()
        .expect("failed to run the example");
        assert_eq!(String::from_utf8_lossy(&run.stdout), answer.to_string());
        ["adaptive_lib", "adaptive_mid", "demo"]
            .into_iter()
            .zip(counts)
            .map(|(name, before)| {
                let events = crate_events(cache.path(), name);
                assert_eq!(events.len(), before + 1, "{name} events: {events:#?}");
                events.into_iter().nth(before).unwrap()
            })
            .collect::<Vec<_>>()
    };

    let events = build("target", None, 1);
    assert_eq!(events[2]["result"], "miss", "event: {:#}", events[2]);

    // The edit loop: the library seeds, then stays active, the crate above it
    // keeps its graph, and the example links what they wrote.
    for (mtime, answer, lane) in [
        (future, 2, "adaptive seed"),
        (future + 1, 3, "adaptive active"),
    ] {
        let events = build("target", Some(mtime), answer);
        assert_passthrough(&events[0], lane);
        assert_passthrough(&events[1], "incremental kept: builds on adaptive_lib");
        let demo = &events[2];
        assert_passthrough(demo, "incremental kept: builds on adaptive_mid");
        assert_eq!(demo["key_ms"], 0, "event: {demo:#}");
        assert_eq!(demo["dep_info_runs"], 0, "event: {demo:#}");
    }

    let events = build("fresh", None, 3);
    assert_eq!(events[0]["result"], "miss", "event: {:#}", events[0]);
    assert_eq!(events[2]["result"], "miss", "event: {:#}", events[2]);
    let events = build("fresher", None, 3);
    assert_eq!(events[2]["result"], "local_hit", "event: {:#}", events[2]);
}

/// A rustc that writes `$RACE_NEXT` over `$RACE_SOURCE` once, just before
/// the compile of `$RACE_CRATE` reads it: a save that lands after Kache took
/// the unit's key. The dep-info pre-pass passes `--emit dep-info` as two
/// arguments and is left alone.
#[cfg(unix)]
const RACE_RUSTC: &str = r#"#!/bin/sh
case " $* " in
  *" --crate-name $RACE_CRATE "*)
    case " $* " in
      *" --emit=dep-info,"*)
        if [ -f "$RACE_NEXT" ]; then
          cat "$RACE_NEXT" > "$RACE_SOURCE"
          rm -f "$RACE_NEXT"
        fi
        ;;
    esac
    ;;
esac
exec "$REAL_RUSTC" "$@"
"#;

/// The rustc the shim hands each compile to.
#[cfg(unix)]
fn real_rustc() -> PathBuf {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("failed to ask rustc for its sysroot");
    assert!(output.status.success(), "rustc --print sysroot failed");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()).join("bin/rustc")
}

/// A workspace whose binary `race-app` depends on `race-lib`, built through
/// [`RACE_RUSTC`].
#[cfg(unix)]
struct RaceWorkspace {
    _daemon: common::DaemonGuard,
    _checkout: tempfile::TempDir,
    cache: tempfile::TempDir,
    tools: tempfile::TempDir,
    project: PathBuf,
    /// Set for every build, after the defaults.
    env: Vec<(&'static str, &'static str)>,
}

#[cfg(unix)]
impl RaceWorkspace {
    fn new(lib: &str, main: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let checkout = tempfile::tempdir().unwrap();
        let project = checkout_path(checkout.path());
        for member in ["lib", "app"] {
            fs::create_dir_all(project.join(member).join("src")).unwrap();
        }
        let package = "version = \"0.1.0\"\nedition = \"2024\"\n";
        fs::write(
            project.join("Cargo.toml"),
            "[workspace]\nmembers = [\"lib\", \"app\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            project.join("lib/Cargo.toml"),
            format!("[package]\nname = \"race-lib\"\n{package}"),
        )
        .unwrap();
        fs::write(
            project.join("app/Cargo.toml"),
            format!(
                "[package]\nname = \"race-app\"\n{package}\n[dependencies]\n\
                 race-lib = {{ path = \"../lib\" }}\n"
            ),
        )
        .unwrap();
        fs::write(project.join("lib/src/lib.rs"), lib).unwrap();
        fs::write(project.join("app/src/main.rs"), main).unwrap();
        fs::write(project.join("README.md"), "race\n").unwrap();
        let tools = tempfile::tempdir().unwrap();
        let shim = tools.path().join("rustc");
        fs::write(&shim, RACE_RUSTC).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let cache = tempfile::tempdir().unwrap();
        Self {
            _daemon: common::DaemonGuard::new(kache_binary(), cache.path()),
            _checkout: checkout,
            cache,
            tools,
            project,
            env: Vec::new(),
        }
    }

    fn with_env(mut self, name: &'static str, value: &'static str) -> Self {
        self.env.push((name, value));
        self
    }

    /// Run `cargo build -p race-app` with `RACE_FLAG=flag`, the app's
    /// output and this build's event for `crate_name`. With `save`,
    /// `source`, a path in the project, becomes `save` just before the
    /// compile of `crate_name` reads it.
    fn build(
        &self,
        flag: &str,
        crate_name: &str,
        source: &str,
        save: Option<&str>,
    ) -> (String, Value) {
        let next = self.tools.path().join("next");
        if let Some(content) = save {
            fs::write(&next, content).unwrap();
        }
        settle_writes(&[&self.project]);
        let target = self.project.join("target");
        let before = crate_events(self.cache.path(), crate_name).len();
        let mut command = build_command(&self.project, self.cache.path(), &target, "race-app");
        command
            .env("RUSTC", self.tools.path().join("rustc"))
            .env("REAL_RUSTC", real_rustc())
            .env("RACE_CRATE", crate_name)
            .env("RACE_SOURCE", self.project.join(source))
            .env("RACE_NEXT", &next)
            .env("RACE_FLAG", flag)
            .env("KACHE_CACHE_EXECUTABLES", "1")
            .envs(self.env.iter().copied());
        let output = command.output().expect("failed to build the race fixture");
        assert!(
            output.status.success(),
            "race fixture build failed\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(!next.exists(), "the compile of {crate_name} never ran");
        let run = Command::new(target.join("debug/race-app"))
            .output()
            .expect("failed to run the race fixture");
        let events = crate_events(self.cache.path(), crate_name);
        assert_eq!(events.len(), before + 1, "{crate_name} events: {events:#?}");
        (
            String::from_utf8_lossy(&run.stdout).into_owned(),
            events.into_iter().nth(before).unwrap(),
        )
    }
}

/// A unit with policy state took its key from the pre-pass, before the
/// compile, and a save landed after that key. The compile must not be stored
/// under it.
#[cfg(unix)]
fn assert_unstored_after_a_save(event: &Value) {
    assert_unstored_after_a_save_keyed(event, 1);
}

/// [`assert_unstored_after_a_save`] for a unit whose key came from the
/// pre-pass (`dep_info_runs` 1) or from its own compile's dep-info (0).
#[cfg(unix)]
fn assert_unstored_after_a_save_keyed(event: &Value, dep_info_runs: u64) {
    assert_eq!(
        event["dep_info_runs"], dep_info_runs,
        "where the key came from: {event:#}"
    );
    assert_eq!(event["result"], "skipped", "event: {event:#}");
    assert_eq!(event["skip_reason"], "inputs-changed", "event: {event:#}");
}

/// A learning library starts cold when its workspace guard changes. A changed
/// environment prevents seed qualification, and a save during that compile
/// must leave no stored artifact or reusable private state.
#[cfg(unix)]
#[test]
fn a_save_while_a_library_with_policy_state_compiles_is_not_stored() {
    let lib = |answer: u64| {
        format!(
            "pub fn answer() -> u64 {{ {answer} }}\n\
             pub fn flag() -> &'static str {{ option_env!(\"RACE_FLAG\").unwrap_or(\"none\") }}\n"
        )
    };
    let fixture = RaceWorkspace::new(
        &lib(1),
        "fn main() { print!(\"{}\", race_lib::answer()); }\n",
    );
    let build = |flag, save: Option<&str>| fixture.build(flag, "race_lib", "lib/src/lib.rs", save);

    let (printed, lib_event) = build("a", None);
    assert_eq!(printed, "1");
    assert_eq!(lib_event["result"], "miss", "event: {lib_event:#}");

    fs::write(fixture.project.join("README.md"), "race, edited\n").unwrap();
    let (printed, lib_event) = build("b", Some(&lib(2)));
    assert_eq!(printed, "2", "rustc read the saved source");
    assert_passthrough(&lib_event, "adaptive seed");
    assert_eq!(lib_event["compiler_runs"], 1, "event: {lib_event:#}");
    assert_eq!(lib_event["dep_info_runs"], 0, "event: {lib_event:#}");

    fs::write(fixture.project.join("lib/src/lib.rs"), lib(1)).unwrap();
    let (printed, lib_event) = build("b", None);
    assert_eq!(lib_event["result"], "miss", "event: {lib_event:#}");
    assert_eq!(printed, "1", "the revert compiled");
}

/// The same for a binary. Its compile links, so it cannot be stopped on a
/// hit, but it still compiles before keying again.
#[cfg(unix)]
#[test]
fn a_save_while_a_binary_with_policy_state_compiles_is_not_stored() {
    let main = |word: &str| {
        format!(
            "fn main() {{ print!(\"{word} {{}}\", option_env!(\"RACE_FLAG\").unwrap_or(\"none\")); }}\n"
        )
    };
    let fixture = RaceWorkspace::new("pub fn answer() -> u64 { 1 }\n", &main("old"));
    let build = |flag, save: Option<&str>| fixture.build(flag, "race_app", "app/src/main.rs", save);

    let (printed, app_event) = build("a", None);
    assert_eq!(printed, "old a");
    assert_eq!(app_event["result"], "miss", "event: {app_event:#}");

    fs::write(fixture.project.join("README.md"), "race, edited\n").unwrap();
    let (printed, app_event) = build("b", Some(&main("new")));
    assert_eq!(printed, "new b", "rustc read the saved source");
    assert_unstored_after_a_save(&app_event);

    fs::write(fixture.project.join("app/src/main.rs"), main("old")).unwrap();
    let (printed, app_event) = build("b", None);
    assert_eq!(app_event["result"], "miss", "event: {app_event:#}");
    assert_eq!(printed, "old b", "the revert compiled");
}

/// A binary whose predicted key missed, as after an eviction, re-derives
/// that key from its own compile's dep-info on Linux, where a compile that
/// links can be stopped on a hit; elsewhere with the pre-pass first, and when
/// the key comes out the same and no seed can start, since the unit was last
/// built under it, the binary still compiles before keying again. Either
/// way a save during the compile is not stored.
#[cfg(unix)]
#[test]
fn a_save_while_a_binary_re_derives_its_key_is_not_stored() {
    let main = |word: &str| format!("fn main() {{ print!(\"{word}\"); }}\n");
    let fixture = RaceWorkspace::new("pub fn answer() -> u64 { 1 }\n", &main("old"));
    let build = |save: Option<&str>| fixture.build("a", "race_app", "app/src/main.rs", save);

    let (printed, app_event) = build(None);
    assert_eq!(printed, "old");
    assert_eq!(app_event["result"], "miss", "event: {app_event:#}");

    // The same bytes again: Cargo rebuilds, the first build's record still
    // applies, and it predicts the key whose entry is gone.
    fs::write(fixture.project.join("app/src/main.rs"), main("old")).unwrap();
    purge_crate(&fixture.project, fixture.cache.path(), "race_app");
    let (printed, app_event) = build(Some(&main("new")));
    assert_eq!(printed, "new", "rustc read the saved source");
    assert_unstored_after_a_save_keyed(
        &app_event,
        if cfg!(target_os = "linux") { 0 } else { 1 },
    );

    fs::write(fixture.project.join("app/src/main.rs"), main("old")).unwrap();
    let (printed, app_event) = build(None);
    assert_eq!(app_event["result"], "miss", "event: {app_event:#}");
    assert_eq!(printed, "old", "the revert compiled");
}

/// With deferred discovery off, every unit keys before it compiles. The
/// modified-input guard then refuses the store when a keyed input moved
/// after the key read it, though its stamp was old at that point, so no
/// wall-clock flag tripped.
#[cfg(unix)]
#[test]
fn the_modified_input_guard_refuses_a_save_after_a_key_taken_first() {
    let lib = |answer: u64| {
        format!(
            "pub fn answer() -> u64 {{ {answer} }}\n\
             pub fn flag() -> &'static str {{ option_env!(\"RACE_FLAG\").unwrap_or(\"none\") }}\n"
        )
    };
    let fixture = RaceWorkspace::new(
        &lib(1),
        "fn main() { print!(\"{}\", race_lib::answer()); }\n",
    )
    .with_env("KACHE_DEFERRED_DISCOVERY", "0")
    .with_env("KACHE_ADAPTIVE_INCREMENTAL", "0")
    .with_env("KACHE_MODIFIED_INPUT_GUARD", "1");
    let build = |flag, save: Option<&str>| fixture.build(flag, "race_lib", "lib/src/lib.rs", save);

    let (printed, lib_event) = build("a", None);
    assert_eq!(printed, "1");
    assert_eq!(lib_event["result"], "miss", "event: {lib_event:#}");

    // The flag makes Cargo rebuild the library and keeps its record out.
    let (printed, lib_event) = build("b", Some(&lib(2)));
    assert_eq!(printed, "2", "rustc read the saved source");
    assert_unstored_after_a_save(&lib_event);

    fs::write(fixture.project.join("lib/src/lib.rs"), lib(1)).unwrap();
    let (printed, lib_event) = build("b", None);
    assert_eq!(lib_event["result"], "miss", "event: {lib_event:#}");
    assert_eq!(printed, "1", "the revert compiled");
}

/// A rustc that, before each compile of `$FLIGHT_CRATE` (not the dep-info
/// pre-pass), appends to `$FLIGHT_LOG` what `$FLIGHT_PROBE` prints for
/// `$FLIGHT_DIR`. Every other unit of the fixture depends on that crate, so
/// no other wrapper runs then.
#[cfg(unix)]
const FLIGHT_RUSTC: &str = r#"#!/bin/sh
case " $* " in
  *" --crate-name $FLIGHT_CRATE "*)
    case " $* " in
      *" --emit=dep-info,"*) "$FLIGHT_PROBE" "$FLIGHT_DIR" >> "$FLIGHT_LOG" ;;
    esac
    ;;
esac
exec "$REAL_RUSTC" "$@"
"#;

/// Prints how many files in the directory it is given another process holds
/// locked, as a peer joining one of those discovery flights finds them.
#[cfg(unix)]
const FLIGHT_PROBE: &str = r#"fn main() {
    let dir = std::env::args().nth(1).expect("a directory");
    let held = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())
                .is_ok_and(|file| file.try_lock().is_err())
        })
        .count();
    println!("{held}");
}
"#;

/// A peer in another checkout that finds no record for a unit waits on the
/// unit's discovery flight for the entry and the record its holder leaves.
/// A seed leaves neither, so it releases the flight before it compiles,
/// while a compile whose result is stored keeps holding it.
#[cfg(unix)]
#[test]
fn a_seed_releases_the_discovery_flight_before_it_compiles() {
    use std::os::unix::fs::PermissionsExt;
    let checkout = tempfile::tempdir().unwrap();
    let project = checkout_path(checkout.path());
    let cache = tempfile::tempdir().unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache.path());
    let tools = tempfile::tempdir().unwrap();
    let target = project.join("target");
    create_workspace(&project);
    let probe = tools.path().join("probe");
    let probe_source = tools.path().join("probe.rs");
    fs::write(&probe_source, FLIGHT_PROBE).unwrap();
    let compiled = Command::new(real_rustc())
        .args(["--edition", "2021", "-o"])
        .arg(&probe)
        .arg(&probe_source)
        .output()
        .expect("failed to compile the flight probe");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let shim = tools.path().join("rustc");
    fs::write(&shim, FLIGHT_RUSTC).unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let log = tools.path().join("held");
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3_600;
    // This build's event for the library, and how many flights another
    // process held when the library's compile started. The whole-workspace
    // guard is pinned: it digests every file by content, so each edit voids
    // the record and the unit discovers again. The scoped guard (the
    // default) counts Rust sources by name and the inputs guard names the
    // closure's directories, so both keep a record through an edit to a file
    // of its closure, and the keyed miss of step 3 would be a predicted key
    // that holds no flight (none is needed: a peer validates the same record
    // and does not wait).
    let build = |mtime: Option<i64>, answer: u64, env: &[(&str, &str)]| {
        let source = project.join("lib/src/lib.rs");
        fs::write(&source, format!("pub fn answer() -> u64 {{ {answer} }}\n")).unwrap();
        if let Some(mtime) = mtime {
            filetime::set_file_mtime(&source, FileTime::from_unix_time(mtime, 0)).unwrap();
        }
        settle_writes(&[&project]);
        let before = crate_events(cache.path(), "adaptive_lib").len();
        let probed = fs::read_to_string(&log).unwrap_or_default().lines().count();
        let output = build_command(&project, cache.path(), &target, "adaptive-app")
            .env("RUSTC", &shim)
            .env("REAL_RUSTC", real_rustc())
            .env("FLIGHT_CRATE", "adaptive_lib")
            .env("FLIGHT_PROBE", &probe)
            .env("FLIGHT_DIR", cache.path().join("scheduler/discovery"))
            .env("FLIGHT_LOG", &log)
            .env("KACHE_WORKSPACE_GUARD", "workspace")
            .envs(env.iter().copied())
            .output()
            .expect("failed to build the workspace fixture");
        assert!(
            output.status.success(),
            "workspace build failed for variant {answer}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        let events = crate_events(cache.path(), "adaptive_lib");
        assert_eq!(events.len(), before + 1, "library events: {events:#?}");
        let held: Vec<String> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .skip(probed)
            .map(String::from)
            .collect();
        assert_eq!(held.len(), 1, "one compile of the library: {held:?}");
        (events[before].clone(), held[0].clone())
    };

    // No policy state: the library holds the flight it found no record
    // under while it compiles before its key, and stores the result.
    let (lib, held) = build(None, 1, &[]);
    assert_eq!(lib["result"], "miss", "event: {lib:#}");
    assert_eq!(lib["dep_info_runs"], 0, "event: {lib:#}");
    assert_eq!(held, "1", "a compile before the key holds the flight");

    // The edit voids the record. The seed releases the flight, then derives
    // its key from the compile's emitted dep-info.
    let (lib, held) = build(Some(future), 2, &[]);
    assert_passthrough(&lib, "adaptive seed");
    assert_eq!(lib["dep_info_runs"], 0, "event: {lib:#}");
    assert_eq!(held, "0", "the seed released the flight");

    // With no lane to seed and no compile before the key, the library
    // compiles after both lookups miss, and stores the result. Once it has
    // its key it lets the discovery flight go: the key's compile flight
    // holds same-key peers, and peers with another key no longer queue
    // behind this compile (a9086f78).
    let (lib, held) = build(
        Some(future + 1),
        3,
        &[
            ("KACHE_ADAPTIVE_INCREMENTAL", "0"),
            ("KACHE_DEFERRED_DISCOVERY", "0"),
        ],
    );
    assert_eq!(lib["result"], "miss", "event: {lib:#}");
    assert_eq!(lib["dep_info_runs"], 1, "event: {lib:#}");
    assert_eq!(held, "0", "a keyed miss lets the discovery flight go");
}

/// `git <args>` in `repo`, with the identity a commit needs.
fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Kache Test",
            "-c",
            "user.email=kache@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A checkout keeps warm what it changed, and restores what the shared tree
/// holds. Its first private compile of a unit starts from the graph a folder
/// kept for others (`keep_incremental = "all"`, a build machine's) holds, and
/// each later one from its own, which a restore of the shared tree leaves in
/// place: after a pull, the next edit is warm again.
#[test]
fn a_checkout_keeps_its_own_edits_warm_and_restores_the_shared_tree() {
    if Command::new("git")
        .arg("--version")
        .output()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return;
    }
    let checkouts = tempfile::tempdir().unwrap();
    let root = checkout_path(checkouts.path());
    let (station, first, second) = (
        root.join("station"),
        root.join("first"),
        root.join("second"),
    );
    let cache = tempfile::tempdir().unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache.path());
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3_600;
    let write = |project: &Path, mtime: Option<i64>, answer: u64| {
        let source = project.join("lib/src/lib.rs");
        fs::write(&source, format!("pub fn answer() -> u64 {{ {answer} }}\n")).unwrap();
        if let Some(mtime) = mtime {
            filetime::set_file_mtime(&source, FileTime::from_unix_time(mtime, 0)).unwrap();
        }
    };
    // This build's events for the library and the crate above it.
    let build = |project: &Path, target: &str, keep: &str, answer: u64| {
        settle_writes(&[project]);
        let before =
            ["adaptive_lib", "adaptive_mid"].map(|name| crate_events(cache.path(), name).len());
        let target = project.join(target);
        let output = build_command(project, cache.path(), &target, "adaptive-app")
            .env("KACHE_SHARED_REF", "shared")
            .env("KACHE_KEEP_INCREMENTAL", keep)
            .output()
            .expect("failed to build the workspace fixture");
        assert!(
            output.status.success(),
            "build failed for variant {answer}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        let run = Command::new(target.join(format!(
            "debug/adaptive-app{}",
            std::env::consts::EXE_SUFFIX
        )))
        .output()
        .expect("failed to run the workspace binary");
        assert_eq!(String::from_utf8_lossy(&run.stdout), answer.to_string());
        ["adaptive_lib", "adaptive_mid"]
            .into_iter()
            .zip(before)
            .map(|(name, before)| {
                let events = crate_events(cache.path(), name);
                assert_eq!(events.len(), before + 1, "{name} events: {events:#?}");
                events.into_iter().nth(before).unwrap()
            })
            .collect::<Vec<_>>()
    };
    let assert_kept = |event: &Value, seed: &str| {
        assert_passthrough(event, "incremental kept");
        assert_eq!(
            event["incremental_seed_attempt"].as_str().unwrap_or(""),
            seed,
            "event: {event:#}"
        );
    };

    for project in [&station, &first, &second] {
        create_workspace(project);
        write(project, None, 1);
    }
    fs::write(first.join(".gitignore"), "/target*\n").unwrap();
    git(&first, &["init", "-q"]);
    git(&first, &["add", "-A"]);
    git(&first, &["commit", "-q", "-m", "one"]);
    git(&first, &["branch", "shared"]);

    // The build machine keeps every graph; it stores none of them.
    for event in build(&station, "target", "all", 1) {
        assert_kept(&event, "no-donor");
    }
    // The checkout at the shared tree compiles it for the store.
    let events = build(&first, "target", "private", 1);
    assert_eq!(events[0]["result"], "miss", "event: {:#}", events[0]);

    // Its first edit starts from the build machine's graphs, and every later
    // one from its own.
    write(&first, Some(future + 1), 2);
    for event in build(&first, "target", "private", 2) {
        assert_kept(&event, "imported");
        assert!(
            event["incremental_seed"]
                .as_str()
                .is_some_and(|donor| Path::new(donor).starts_with(&station)),
            "event: {event:#}"
        );
    }
    write(&first, Some(future + 2), 3);
    for event in build(&first, "target", "private", 3) {
        assert_kept(&event, "");
    }

    // A commit another checkout stored, which the shared ref moved to, is
    // restored, and what is built on it.
    write(&second, None, 7);
    let events = build(&second, "target-7", "private", 7);
    assert_eq!(events[0]["result"], "miss", "event: {:#}", events[0]);
    write(&first, Some(future + 3), 7);
    git(&first, &["commit", "-q", "-a", "-m", "seven"]);
    git(&first, &["branch", "-f", "shared", "HEAD"]);
    let events = build(&first, "target", "private", 7);
    assert_eq!(events[0]["result"], "local_hit", "event: {:#}", events[0]);
    assert_eq!(events[1]["result"], "local_hit", "event: {:#}", events[1]);

    // The restore left the checkout's graphs: its next edit needs no donor.
    write(&first, Some(future + 4), 8);
    for event in build(&first, "target", "private", 8) {
        assert_kept(&event, "");
    }
}

/// A graph made against another build of a library is imported, builds
/// right, and its event says so; a donor made against the receiver's own
/// libraries goes first. A checkout whose shared tree holds another library
/// made its graph of the crate above against that one: a checkout at the
/// first commit imports it as made against other libraries. A second
/// checkout at the first commit restored the same library from the store,
/// so it imports the first's graph rather than the other's. A folder kept
/// for others compiled the library itself, incrementally: another build of
/// the same crate, which counts as the same library, and a folder kept for
/// others goes before a checkout, so a third checkout imports from there.
#[test]
fn a_graph_made_against_another_library_is_imported_and_told_apart() {
    if Command::new("git")
        .arg("--version")
        .output()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return;
    }
    let checkouts = tempfile::tempdir().unwrap();
    let root = checkout_path(checkouts.path());
    let [other, first, second, station, third] =
        ["other", "first", "second", "station", "third"].map(|name| root.join(name));
    let cache = tempfile::tempdir().unwrap();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache.path());
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3_600;
    // This build's event for the middle crate.
    let build = |project: &Path, keep: &str, answer: u64| {
        settle_writes(&[project]);
        let before = crate_events(cache.path(), "adaptive_mid").len();
        let target = project.join("target");
        let output = build_command(project, cache.path(), &target, "adaptive-app")
            .env("KACHE_SHARED_REF", "shared")
            .env("KACHE_KEEP_INCREMENTAL", keep)
            .output()
            .expect("failed to build the workspace fixture");
        assert!(
            output.status.success(),
            "build failed for variant {answer}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        let run = Command::new(target.join(format!(
            "debug/adaptive-app{}",
            std::env::consts::EXE_SUFFIX
        )))
        .output()
        .expect("failed to run the workspace binary");
        assert_eq!(String::from_utf8_lossy(&run.stdout), answer.to_string());
        let events = crate_events(cache.path(), "adaptive_mid");
        assert_eq!(events.len(), before + 1, "events: {events:#?}");
        events.into_iter().nth(before).unwrap()
    };
    let edit_middle = |project: &Path, mtime: i64| {
        let source = project.join("mid/src/lib.rs");
        fs::write(
            &source,
            "pub fn answer() -> u64 { adaptive_lib::answer() + 1 }\n",
        )
        .unwrap();
        filetime::set_file_mtime(&source, FileTime::from_unix_time(mtime, 0)).unwrap();
    };
    let assert_seed = |event: &Value, seed: &str, donor: Option<&Path>| {
        assert_passthrough(event, "incremental kept: changed here");
        assert_eq!(
            event["incremental_seed_attempt"].as_str().unwrap_or(""),
            seed,
            "event: {event:#}"
        );
        match donor {
            Some(donor) => assert!(
                event["incremental_seed"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).starts_with(donor)),
                "event: {event:#}"
            ),
            None => assert!(event["incremental_seed"].is_null(), "event: {event:#}"),
        }
    };

    for (project, answer) in [(&other, 7), (&first, 1), (&second, 1), (&station, 1), (&third, 1)] {
        create_workspace(project);
        fs::write(
            project.join("lib/src/lib.rs"),
            format!("pub fn answer() -> u64 {{ {answer} }}\n"),
        )
        .unwrap();
        fs::write(project.join(".gitignore"), "/target*\n").unwrap();
        git(project, &["init", "-q"]);
        git(project, &["add", "-A"]);
        git(project, &["commit", "-q", "-m", "one"]);
        git(project, &["branch", "shared"]);
    }

    // Another commit's checkout stores its library and edits the crate above.
    assert_eq!(build(&other, "private", 7)["result"], "miss");
    edit_middle(&other, future + 1);
    assert_seed(&build(&other, "private", 8), "no-donor", None);

    // Its graph was made against another library than this checkout's.
    assert_eq!(build(&first, "private", 1)["result"], "miss");
    edit_middle(&first, future + 2);
    assert_seed(
        &build(&first, "private", 2),
        "imported-other-libraries",
        Some(&other),
    );

    // The first checkout's graph was made against the store's library.
    assert_eq!(build(&second, "private", 1)["result"], "local_hit");
    edit_middle(&second, future + 3);
    assert_seed(&build(&second, "private", 2), "imported", Some(&first));

    // The folder kept for others compiled the library itself.
    assert_passthrough(&build(&station, "all", 1), "incremental kept: every unit");
    assert_eq!(build(&third, "private", 1)["result"], "local_hit");
    edit_middle(&third, future + 4);
    assert_seed(&build(&third, "private", 2), "imported", Some(&station));
}

/// `cargo build -p <package>` of `project` into its `target`, through Kache
/// with the shared ref `shared` and `keep_incremental` set to `keep`.
fn build_package(project: &Path, cache_dir: &Path, package: &str, keep: &str) {
    settle_writes(&[project]);
    let output = build_command(project, cache_dir, &project.join("target"), package)
        .env("KACHE_SHARED_REF", "shared")
        .env("KACHE_KEEP_INCREMENTAL", keep)
        .output()
        .expect("failed to build the workspace fixture");
    assert!(
        output.status.success(),
        "cargo build -p {package} failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// What the app last built in `project` prints.
fn app_answer(project: &Path) -> String {
    let app = project.join(format!(
        "target/debug/adaptive-app{}",
        std::env::consts::EXE_SUFFIX
    ));
    let run = Command::new(&app)
        .output()
        .expect("failed to run the workspace binary");
    String::from_utf8_lossy(&run.stdout).into_owned()
}

fn write_answer(project: &Path, answer: u64) {
    fs::write(
        project.join("lib/src/lib.rs"),
        format!("pub fn answer() -> u64 {{ {answer} }}\n"),
    )
    .unwrap();
}

/// Commit `project` as it is and point the shared ref at it.
fn commit_shared(project: &Path) {
    fs::write(project.join(".gitignore"), "/target*\n").unwrap();
    git(project, &["init", "-q"]);
    git(project, &["add", "-A"]);
    git(project, &["commit", "-q", "-m", "one"]);
    git(project, &["branch", "shared"]);
}

fn git_missing() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// The one event `build` adds for `crate_name`, if it adds one.
fn new_event(cache_dir: &Path, crate_name: &str, build: impl FnOnce()) -> Option<Value> {
    let before = crate_events(cache_dir, crate_name).len();
    build();
    let events = crate_events(cache_dir, crate_name);
    assert!(events.len() <= before + 1, "{crate_name} events: {events:#?}");
    events.into_iter().nth(before)
}

/// Cargo tells a library built on another fresh across its runs while it is
/// newer than that library's metadata. A compile on a kept graph can take
/// its metadata from the graph unchanged: rustc links the file it saved
/// there, whose time is when it was made. A graph imported from a folder
/// kept for others made it before this checkout built the crate above on
/// another library: the library built alone, then the crate above alone, the
/// crate above is compiled again. When the library's metadata is the graph's
/// own file, a compile that leaves it unchanged keeps it as it is, and the
/// crate above stays fresh.
#[test]
fn metadata_from_an_imported_graph_is_newer_than_what_was_built_on_the_old() {
    if git_missing() {
        return;
    }
    let checkouts = tempfile::tempdir().unwrap();
    let root = checkout_path(checkouts.path());
    let [station, first] = ["station", "first"].map(|name| root.join(name));
    let cache = tempfile::tempdir().unwrap();
    let cache = cache.path();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache);
    for project in [&station, &first] {
        create_workspace(project);
    }

    // The folder kept for others compiled the library as 2.
    write_answer(&station, 2);
    build_package(&station, cache, "adaptive-app", "all");
    // The checkout's shared tree holds 1, and the crates above are built on
    // it after that.
    write_answer(&first, 1);
    commit_shared(&first);
    build_package(&first, cache, "adaptive-app", "private");
    assert_eq!(app_answer(&first), "1");

    // Its edit is what the folder kept for others compiled.
    write_answer(&first, 2);
    let lib = new_event(cache, "adaptive_lib", || {
        build_package(&first, cache, "adaptive-lib", "private")
    })
    .expect("the library is compiled");
    assert_passthrough(&lib, "incremental kept: changed here");
    assert_eq!(lib["incremental_seed_attempt"], "imported", "event: {lib:#}");
    let mid = new_event(cache, "adaptive_mid", || {
        build_package(&first, cache, "adaptive-app", "private")
    });
    assert!(
        mid.is_some(),
        "the crate built on the library's old metadata was left as it was"
    );
    assert_eq!(app_answer(&first), "2");

    // The same source again: the graph's own metadata, unchanged.
    write_answer(&first, 2);
    let lib = new_event(cache, "adaptive_lib", || {
        build_package(&first, cache, "adaptive-lib", "private")
    })
    .expect("the library is compiled");
    assert_passthrough(&lib, "incremental kept: changed here");
    assert_eq!(
        lib["incremental_seed_attempt"].as_str().unwrap_or(""),
        "",
        "event: {lib:#}"
    );
    let mid = new_event(cache, "adaptive_mid", || {
        build_package(&first, cache, "adaptive-app", "private")
    });
    assert!(mid.is_none(), "unchanged metadata needs no rebuild: {mid:#?}");
    assert_eq!(app_answer(&first), "2");
}

/// A restore of the shared tree leaves a checkout's kept graph, so the
/// graph's metadata is not what the target holds after it: compiled again
/// on that graph, the library's metadata is newer than the crates the
/// restore's build put above it.
#[test]
fn metadata_from_a_kept_graph_is_newer_than_what_a_restore_built_on() {
    if git_missing() {
        return;
    }
    let checkout = tempfile::tempdir().unwrap();
    let project = checkout_path(checkout.path());
    let cache = tempfile::tempdir().unwrap();
    let cache = cache.path();
    let _daemon = common::DaemonGuard::new(kache_binary(), cache);
    create_workspace(&project);
    write_answer(&project, 1);
    commit_shared(&project);
    let build = |package: &str| build_package(&project, cache, package, "private");

    build("adaptive-app");
    write_answer(&project, 2);
    build("adaptive-app");
    assert_eq!(app_answer(&project), "2");
    write_answer(&project, 1);
    let lib = new_event(cache, "adaptive_lib", || build("adaptive-app")).unwrap();
    assert_eq!(lib["result"], "local_hit", "event: {lib:#}");
    assert_eq!(app_answer(&project), "1");

    // The edit again, on the graph the restore left.
    write_answer(&project, 2);
    let lib = new_event(cache, "adaptive_lib", || build("adaptive-lib")).unwrap();
    assert_passthrough(&lib, "incremental kept: changed here");
    assert_eq!(
        lib["incremental_seed_attempt"].as_str().unwrap_or(""),
        "",
        "event: {lib:#}"
    );
    let mid = new_event(cache, "adaptive_mid", || build("adaptive-app"));
    assert!(
        mid.is_some(),
        "the crate the restore's build put on the old metadata was left as it was"
    );
    assert_eq!(app_answer(&project), "2");
}

