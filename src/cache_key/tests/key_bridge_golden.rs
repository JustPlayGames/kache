//! Golden keys for the key bridges (lane KACHE-W21): for each kind of rustc
//! invocation a bridge adopts, the legacy key this release computes must be
//! the key the bridged release really computed for the same invocation.
//!
//! The goldens were captured by running [`golden_keys`] in the trees of the
//! releases they name (tasty.56 = fork tasty-v83 f5cced6d, and tasty-v84
//! eb4c5109, both key version 33; `cargo test --lib golden_capture --
//! --ignored --nocapture`), not computed here. The fixture is hermetic on
//! Linux x86_64: a stand-in rustc, clippy-driver, linker, `getconf` and `ldd`
//! on a private `PATH`, fixed sources and environment, every path under one
//! normalised workspace; the capture ran it from two directories and
//! printed the same keys. Other hosts key the linker and libc differently
//! and need goldens captured there.
//!
//! A release build (no `key-bump-trial`: key version 33, no bridge) must key
//! each of them exactly as tasty.56 did: its keys are the goldens themselves,
//! so the machinery ships without invalidating a single entry (lane
//! KACHE-W22).
//!
//! A bump that changes how one of these kinds keys must leave it out of its
//! bridge (`KeyBridge::applies`), and then its legacy key is `None`; a bump
//! that changes nothing for it must leave its golden alone. Never update a
//! golden to make this pass: recapture it from the bridged release.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use super::*;

/// The bridged release's key version these goldens belong to.
const GOLDEN_VERSION: u32 = 33;

/// (kind, links, key under tasty.56 / v84).
const GOLDENS: &[(&str, bool, &str)] = &[
    ("rlib_check", false, "42abf4e800cda9d2abcbef10bed645b238ac6bc40d63035af77f26b764b6c3fb"),
    ("rlib_build", false, "b348bd95305e4aa7f881cc895567b062a622bbd28537853a7ccf55c477a9ee55"),
    ("rlib_with_extern", false, "6bf39934fd39a373663552602dfae9e1ee851c61ed61681259425147282c10a7"),
    ("proc_macro", true, "42214a1d15c4548ada0a266bd1d26f116a235216e3f14fc0d973955c07b3ee75"),
    ("bin", true, "9efc093b661ce4e42e764919f36e3e4ead1149ae06569f3ac1e823c62e3e4f07"),
    ("build_script_build", true, "d3b2b8caa3c769b8890909f2f0308e55f5acefed69c6ebc4396b0288c8f9c8d1"),
    ("clippy_check", false, "9d9819d8fea8aacb50d1bd11d3a255f55f8d99bb5d83f8b0c99cd0c2e3d98d57"),
];

/// `(kind, key, legacy key)` of one golden invocation.
type GoldenKey = (&'static str, String, Option<(u32, String)>);

const RUSTC: &str = "#!/bin/sh\n\
case \"$*\" in\n\
  *--print*sysroot*) echo \"${0%/bin/rustc}/sysroot\" ;;\n\
  *-vV*|*--version*) printf 'rustc 1.99.0 (golden 2026-10-01)\\nbinary: rustc\\ncommit-hash: 0123456789abcdef0123456789abcdef01234567\\ncommit-date: 2026-10-01\\nhost: x86_64-unknown-linux-gnu\\nrelease: 1.99.0\\nLLVM version: 21.1.0\\n' ;;\n\
  *) exit 1 ;;\n\
esac\n";
const CLIPPY: &str = "#!/bin/sh\necho 'clippy 0.1.99 (golden 2026-10-01)'\n";
const LINKER: &str = "#!/bin/sh\n\
bin=\"${0%/cc}\"; lib=\"${0%/bin/cc}/lib\"\n\
for arg in \"$@\"; do case \"$arg\" in\n\
  -print-search-dirs) printf 'programs: =%s\\nlibraries: =%s\\n' \"$bin\" \"$lib\"; exit 0 ;;\n\
  -print-file-name=*) n=\"${arg#-print-file-name=}\"; if [ -f \"$lib/$n\" ]; then echo \"$lib/$n\"; else echo \"$n\"; fi; exit 0 ;;\n\
  -print-prog-name=*) n=\"${arg#-print-prog-name=}\"; if [ -x \"$bin/$n\" ]; then echo \"$bin/$n\"; else echo \"$n\"; fi; exit 0 ;;\n\
  --version|-v|-dumpversion) echo 'cc (golden) 14.0.0'; exit 0 ;;\n\
esac; done\n";
const LD: &str = "#!/bin/sh\necho 'GNU ld (golden) 2.99'\n";
/// The C runtime the stand-in linker driver places.
const CRT: &[&str] = &[
    "Scrt1.o",
    "crt1.o",
    "crti.o",
    "crtn.o",
    "crtbeginS.o",
    "crtendS.o",
    "libc.so.6",
];
const GETCONF: &str = "#!/bin/sh\necho 'glibc 2.99'\n";
const LDD: &str = "#!/bin/sh\necho 'ldd (GNU libc) 2.99'\n";

/// The key of each golden invocation, computed in a fresh workspace under
/// `root`: `(kind, key, legacy key)`.
/// `scope` is the bridge scope (`KACHE_KEY_BRIDGE_SCOPE`) to compute under.
fn golden_keys(
    root: &Path,
    scope: Option<&str>,
) -> Vec<GoldenKey> {
    let _lock = key_test_lock();
    let saved: Vec<_> = ["KACHE_KEY_BRIDGE", "KACHE_KEY_BRIDGE_SCOPE"]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
    // SAFETY: the process-state lock serializes environment edits.
    unsafe {
        std::env::remove_var("KACHE_KEY_BRIDGE");
        match scope {
            Some(scope) => std::env::set_var("KACHE_KEY_BRIDGE_SCOPE", scope),
            None => std::env::remove_var("KACHE_KEY_BRIDGE_SCOPE"),
        }
    }
    let keys = golden_keys_locked(root);
    for (name, value) in saved {
        // SAFETY: the process-state lock is still held.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
    keys
}

fn golden_keys_locked(root: &Path) -> Vec<GoldenKey> {
    let ws = root.join("ws");
    let bin = ws.join("toolchain/bin");
    for dir in ["src", "target/debug/deps", "home", "tmp", "cache", "toolchain/sysroot"] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
    }
    std::fs::create_dir_all(&bin).unwrap();
    for (name, script) in [
        ("rustc", RUSTC),
        ("clippy-driver", CLIPPY),
        ("cc", LINKER),
        ("ld", LD),
        ("getconf", GETCONF),
        ("ldd", LDD),
    ] {
        kache_fs::testutil::write_executable(&bin.join(name), script);
    }
    std::fs::create_dir_all(ws.join("toolchain/lib")).unwrap();
    for name in CRT {
        std::fs::write(ws.join("toolchain/lib").join(name), format!("golden {name}")).unwrap();
    }
    let ws = ws.canonicalize().unwrap();
    let bin = ws.join("toolchain/bin");
    let lib_rs = ws.join("src/lib.rs");
    let main_rs = ws.join("src/main.rs");
    let build_rs = ws.join("build.rs");
    std::fs::write(&lib_rs, "pub fn golden() -> u32 { 33 }\n").unwrap();
    std::fs::write(&main_rs, "fn main() { println!(\"golden\"); }\n").unwrap();
    std::fs::write(&build_rs, "fn main() {}\n").unwrap();
    let deps = ws.join("target/debug/deps");
    let dep_rmeta = deps.join("libgolden_dep-00000000000000aa.rmeta");
    std::fs::write(&dep_rmeta, b"golden dependency metadata").unwrap();

    let invocation = crate::invocation::InvocationEnv::from_parts(
        [
            ("PATH", bin.display().to_string()),
            ("CARGO", ws.join("toolchain/bin/cargo").display().to_string()),
            ("CARGO_PKG_NAME", "golden".to_string()),
            ("CARGO_PKG_VERSION", "0.1.0".to_string()),
            ("CARGO_MANIFEST_DIR", ws.display().to_string()),
            ("CARGO_CRATE_NAME", "golden".to_string()),
        ],
        Some(ws.clone()),
        Some(ws.join("home")),
        ws.join("tmp"),
    )
    .with_cache_dir(Some(ws.join("cache")));
    let file_hasher = FileHasher::new_for(&invocation);
    let normalizer = PathNormalizer::from_invocation(Some(&ws), &invocation);
    let env = KeyEnv::from_invocation(&invocation);

    let rustc = bin.join("rustc").display().to_string();
    let common = |crate_name: &str, source: &Path, crate_type: &str, emit: &str| {
        let mut argv = vec![
            rustc.clone(),
            "--crate-name".into(),
            crate_name.into(),
            "--edition=2021".into(),
            source.display().to_string(),
            "--error-format=json".into(),
            "--json=diagnostic-rendered-ansi,artifacts,future-incompat".into(),
            "--crate-type".into(),
            crate_type.into(),
            format!("--emit={emit}"),
            "-C".into(),
            "embed-bitcode=no".into(),
            "-C".into(),
            "debuginfo=2".into(),
            "--cfg".into(),
            "feature=\"default\"".into(),
            "-C".into(),
            "metadata=0123456789abcdef".into(),
            "-C".into(),
            "extra-filename=-0123456789abcdef".into(),
            "--out-dir".into(),
            deps.display().to_string(),
            "-L".into(),
            format!("dependency={}", deps.display()),
        ];
        if crate_type != "lib" {
            argv.push("-C".into());
            argv.push(format!("linker={}", bin.join("cc").display()));
        }
        argv
    };
    let mut kinds: Vec<(&'static str, Vec<String>, PathBuf)> = vec![
        (
            "rlib_check",
            common("golden", &lib_rs, "lib", "dep-info,metadata"),
            lib_rs.clone(),
        ),
        (
            "rlib_build",
            common("golden", &lib_rs, "lib", "dep-info,metadata,link"),
            lib_rs.clone(),
        ),
        (
            "rlib_with_extern",
            [
                common("golden", &lib_rs, "lib", "dep-info,metadata,link"),
                vec![
                    "--extern".into(),
                    format!("golden_dep={}", dep_rmeta.display()),
                ],
            ]
            .concat(),
            lib_rs.clone(),
        ),
        (
            "proc_macro",
            [
                common("golden", &lib_rs, "proc-macro", "dep-info,link"),
                vec![
                    "--extern".into(),
                    "proc_macro".into(),
                    "-C".into(),
                    "prefer-dynamic".into(),
                ],
            ]
            .concat(),
            lib_rs.clone(),
        ),
        (
            "bin",
            common("golden", &main_rs, "bin", "dep-info,link"),
            main_rs.clone(),
        ),
        (
            "build_script_build",
            common("build_script_build", &build_rs, "bin", "dep-info,link"),
            build_rs.clone(),
        ),
    ];
    let mut clippy = common("golden", &lib_rs, "lib", "dep-info,metadata");
    clippy.insert(0, bin.join("clippy-driver").display().to_string());
    kinds.push(("clippy_check", clippy, lib_rs.clone()));

    kinds
        .into_iter()
        .map(|(kind, argv, source)| {
            let args = RustcArgs::parse_for(&argv, &invocation).unwrap();
            provide_dep_info(
                DepInfo {
                    source_files: vec![source],
                    env_deps: vec![],
                },
                None,
            );
            let (key, outputs) =
                compute_cache_key_with_outputs(&args, &file_hasher, &normalizer, &env);
            let key = key.unwrap_or_else(|e| panic!("{kind}: {e:#}"));
            (kind, key, outputs.legacy_key)
        })
        .collect()
}

/// Prints the keys of [`golden_keys`] in the tree it runs in, twice from
/// two directories; the capture of [`GOLDENS`].
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore = "capture: run in the bridged release's tree"]
fn golden_capture() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let first = golden_keys(a.path(), None);
    let second = golden_keys(&b.path().join("elsewhere"), None);
    for ((kind, key, _), (_, again, _)) in first.iter().zip(&second) {
        assert_eq!(key, again, "{kind}: the fixture is not hermetic");
        println!("GOLDEN {kind} {key}");
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn each_bridged_kind_computes_the_key_the_bridged_release_computed() {
    let dir = tempfile::tempdir().unwrap();
    let keys = golden_keys(dir.path(), None);
    assert_eq!(keys.len(), GOLDENS.len());
    if CACHE_KEY_VERSION == GOLDEN_VERSION {
        // A release with the machinery and no bump: every component still
        // at the goldens' version, nothing bridged.
        for version in [CC_KEY_VERSION, BUILD_SCRIPT_KEY_VERSION, PREDICTION_KEY_VERSION] {
            assert_eq!(version, GOLDEN_VERSION);
        }
        assert!(bridged_key_versions().is_empty());
    }
    for ((kind, key, legacy), (golden_kind, links, golden)) in keys.iter().zip(GOLDENS) {
        assert_eq!(kind, golden_kind);
        let bridge = KEY_BRIDGES.iter().find(|bridge| bridge.from == GOLDEN_VERSION);
        if CACHE_KEY_VERSION == GOLDEN_VERSION || bridge.is_none() {
            // No bump since the capture: the key itself is the golden. Once
            // the bridge from it is dropped there is nothing to check.
            if CACHE_KEY_VERSION == GOLDEN_VERSION {
                assert_eq!(key, golden, "{kind}: key changed without a key bump");
                assert_eq!(legacy, &None, "{kind}: no bump, nothing to bridge");
            }
            continue;
        }
        let Some((version, legacy)) = legacy else {
            panic!("{kind}: the bridge from {GOLDEN_VERSION} computed no legacy key");
        };
        assert_eq!(*version, GOLDEN_VERSION, "{kind}");
        assert_eq!(
            legacy, golden,
            "{kind} (links: {links}): the legacy key is not the key the bridged release computed; \
             leave this kind out of the bridge or fix the derivation"
        );
        assert_ne!(key, golden, "{kind}: the new key must differ from the old one");
    }
}

/// A bridge whose scope leaves linking units out computes no legacy key for
/// them, and the same golden for the rest (a trial bump only: a release
/// build has no bridge).
#[cfg(all(target_os = "linux", target_arch = "x86_64", feature = "key-bump-trial"))]
#[test]
fn a_bridge_scoped_to_nonlinking_units_leaves_linking_ones_unbridged() {
    let dir = tempfile::tempdir().unwrap();
    let keys = golden_keys(dir.path(), Some("nonlinking"));
    for ((kind, _, legacy), (_, links, golden)) in keys.iter().zip(GOLDENS) {
        if *links {
            assert_eq!(legacy, &None, "{kind}");
        } else {
            assert_eq!(
                legacy.as_ref().map(|(_, key)| key.as_str()),
                Some(*golden),
                "{kind}"
            );
        }
    }
}
