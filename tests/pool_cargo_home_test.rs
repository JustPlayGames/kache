//! Hits across the members of a worktree pool.
//!
//! Each member builds with a Cargo home of its own whose `registry` and `git`
//! link into one shared home. Cargo spells a downloaded package's directory
//! through the member's home, so a package that bakes `CARGO_MANIFEST_DIR`
//! (merman-render, merman-core) keyed apart in every member, and so did every
//! crate built on it.
//!
//! The fixture package comes from a local registry, so Cargo extracts it under
//! the shared home's `registry/src/` through the first member's link, and both
//! members resolve it to one unit, as they do a crates.io package.

#![cfg(unix)]

use serde_json::Value;
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

mod common;
use common::{build_kache, hermetic_command, isolated_config_path, kache_binary};

fn write(path: &Path, body: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A local registry holding `bakes`, a lib that returns its own
/// `CARGO_MANIFEST_DIR`.
fn write_registry(base: &Path) -> PathBuf {
    let source = base.join("bakes-source");
    write(
        &source.join("Cargo.toml"),
        "[package]\nname = \"bakes\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         description = \"fixture\"\nlicense = \"MIT\"\n\n[workspace]\n",
    );
    write(
        &source.join("src/lib.rs"),
        "pub fn manifest_dir() -> &'static str {\n    env!(\"CARGO_MANIFEST_DIR\")\n}\n",
    );
    let output = Command::new("cargo")
        .args(["package", "--offline", "--allow-dirty", "--no-verify"])
        .current_dir(&source)
        .env("CARGO_HOME", base.join("packaging.cargo"))
        .env("CARGO_TARGET_DIR", source.join("target"))
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .output()
        .unwrap();
    assert!(output.status.success(), "cargo package: {output:?}");
    let bytes = std::fs::read(source.join("target/package/bakes-0.1.0.crate")).unwrap();
    let checksum: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let registry = base.join("registry");
    write(&registry.join("bakes-0.1.0.crate"), &bytes);
    write(
        &registry.join("index/ba/ke/bakes"),
        format!(
            "{{\"name\":\"bakes\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{checksum}\",\
             \"features\":{{}},\"yanked\":false}}\n"
        ),
    );
    registry
}

/// A member: its Cargo home linking the shared one's downloads and naming the
/// registry, and a workspace whose app prints what the package baked.
fn write_member(base: &Path, member: &str, shared: &Path, registry: &Path) -> (PathBuf, PathBuf) {
    let home = base.join(format!("{member}.cargo"));
    std::fs::create_dir_all(&home).unwrap();
    for sub in ["registry", "git"] {
        std::os::unix::fs::symlink(shared.join(sub), home.join(sub)).unwrap();
    }
    write(
        &home.join("config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n\n\
             [source.fixture]\nlocal-registry = {:?}\n",
            registry.to_str().unwrap()
        ),
    );
    let workspace = base.join(member);
    write(
        &workspace.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nbakes = \"0.1.0\"\n\n[workspace]\n",
    );
    write(
        &workspace.join("src/main.rs"),
        "fn main() {\n    println!(\"{}\", bakes::manifest_dir());\n}\n",
    );
    (home, workspace)
}

fn cargo_build(workspace: &Path, home: &Path, cache: &Path) -> Output {
    let mut command = hermetic_command("cargo", cache, Some(&isolated_config_path(cache)));
    command
        .args(["build", "--offline"])
        .current_dir(workspace)
        .env("CARGO_HOME", home)
        .env("RUSTC_WRAPPER", kache_binary())
        .env("CARGO_TARGET_DIR", workspace.join("target"))
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TERM_COLOR", "never")
        .env("KACHE_LOG", "off")
        .env("KACHE_SEED_NEW_TARGETS", "0")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("KACHE_DISABLED")
        .env_remove("KACHE_LOG_FILE");
    let output = command.output().expect("run Cargo through Kache");
    assert!(
        output.status.success(),
        "building {} failed.\nstdout:\n{}\nstderr:\n{}",
        workspace.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn printed(workspace: &Path) -> String {
    let output = Command::new(workspace.join("target/debug/app"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{} failed", workspace.display());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Each event of `krate` after the first `skip`: its result, and whether it
/// ran rustc's dep-info pre-pass before the lookup.
fn results_for(cache: &Path, skip: usize, krate: &str) -> Vec<(String, bool)> {
    common::settled_events(cache)
        .lines()
        .skip(skip)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["crate_name"] == krate)
        .map(|event| {
            let result = event["result"].as_str().unwrap_or_default().to_string();
            (result, event["dep_info_runs"].as_u64().unwrap_or(0) > 0)
        })
        .collect()
}

#[test]
fn a_package_that_bakes_its_manifest_dir_hits_in_another_pool_member() {
    build_kache();
    let tmp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let cache = base.join("cache");
    let _daemon = common::DaemonGuard::new(kache_binary(), &cache);
    let shared = base.join("shared-cargo");
    for sub in ["registry", "git"] {
        std::fs::create_dir_all(shared.join(sub)).unwrap();
    }
    let registry = write_registry(&base);
    let (first_home, first) = write_member(&base, "w01", &shared, &registry);
    let (second_home, second) = write_member(&base, "w02", &shared, &registry);

    cargo_build(&first, &first_home, &cache);
    let before = common::settled_events(&cache).lines().count();
    cargo_build(&second, &second_home, &cache);

    for krate in ["bakes", "app"] {
        let results = results_for(&cache, before, krate);
        assert!(
            !results.is_empty() && results.iter().all(|(result, _)| result == "local_hit"),
            "{krate} ran rustc in the second member: {results:?}"
        );
    }
    // The first member's prediction row serves the second: its package paths
    // are cut under the registry spelling rustc read them through, not the
    // shared one CARGO_MANIFEST_DIR now names, or every registry hit in a new
    // member paid the dep-info pre-pass.
    let bakes = results_for(&cache, before, "bakes");
    assert!(
        bakes.iter().all(|(_, pre_pass)| !pre_pass),
        "bakes ran the dep-info pre-pass in the second member: {bakes:?}"
    );
    // What the package bakes is the shared directory, valid in every member.
    let baked = printed(&first);
    assert!(
        baked.starts_with(shared.join("registry/src").to_str().unwrap()),
        "{baked}"
    );
    assert_eq!(printed(&second), baked);
}
