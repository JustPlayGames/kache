//! Compiles the build-script launcher (`launcher/build_script.rs`) for the
//! target kache is being built for. `src/build_script.rs` embeds the binary and
//! installs a copy wherever Cargo expects a build script it caches.

use std::path::{Path, PathBuf};

#[path = "launcher/compile.rs"]
mod launcher;

const SOURCE: &str = "launcher/build_script.rs";
const WINDOWS_SOURCE: &str = "launcher/build_script_windows.rs";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed={WINDOWS_SOURCE}");
    println!("cargo:rerun-if-changed=launcher/compile.rs");
    let out =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("build-script-launcher");
    if std::env::var("CARGO_CFG_WINDOWS").is_ok()
        && std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc")
    {
        build_windows_launcher(&out);
        return;
    }
    // Launchers are installed on Unix and Windows MSVC only; elsewhere
    // nothing reads the bytes.
    if std::env::var_os("CARGO_CFG_UNIX").is_none() {
        std::fs::write(&out, b"").expect("writing the empty launcher");
        return;
    }

    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("TARGET");
    let mut link_options = Vec::new();
    // Link the launcher the way Cargo links kache for this target.
    if let Some(linker) = std::env::var_os("RUSTC_LINKER") {
        let mut flag = std::ffi::OsString::from("linker=");
        flag.push(linker);
        link_options.push("-C".into());
        link_options.push(flag);
    }
    let encoded = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    link_options.extend(
        link_flags(encoded.split('\x1f').filter(|flag| !flag.is_empty()))
            .into_iter()
            .map(std::ffi::OsString::from),
    );
    // The launcher's own `link(name = "c")` puts libc before the standard
    // rlibs on the link line. nixpkgs builds compiler_builtins' C code with a
    // stack protector, and on aarch64 glibc exports `__stack_chk_guard` from
    // ld.so, which the linker then cannot reach (NixOS/nixpkgs#561411). A
    // std binary gets libc again after its rlibs; this one needs it asked for.
    if target_is_glibc() {
        link_options.extend(["-C".into(), "link-arg=-lc".into()]);
    }

    let result = launcher::build(&rustc, &target, Path::new(SOURCE), &out, &link_options)
        .expect("running rustc for the build-script launcher");
    assert!(
        result.status.success(),
        "compiling {SOURCE} for {target} failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
}

/// The Windows launcher, a small `std` program, built for kache's target
/// with the linker, link flags and search paths Cargo gives kache (a cross
/// build's `lld-link` and its `-L` SDK folders; `+crt-static`).
fn build_windows_launcher(out: &Path) {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("TARGET");
    let mut command = std::process::Command::new(&rustc);
    command
        .args(["--edition", "2024", "--crate-type", "bin"])
        .args(["--crate-name", "kache_build_script_launcher"])
        .args(["--target", &target])
        .args(["-Copt-level=s", "-Cpanic=abort", "-Ccodegen-units=1"])
        .args(["-Cdebuginfo=0", "-Cstrip=symbols"])
        // Copied into every target folder, it must not need the VC++
        // runtime's DLLs whatever kache itself links against.
        .arg("-Ctarget-feature=+crt-static");
    if let Some(linker) = std::env::var_os("RUSTC_LINKER") {
        let mut flag = std::ffi::OsString::from("-Clinker=");
        flag.push(linker);
        command.arg(flag);
    }
    let encoded = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let flags: Vec<&str> = encoded
        .split('\x1f')
        .filter(|flag| !flag.is_empty())
        .collect();
    command.args(link_flags(flags.iter().copied()));
    command.args(search_paths(flags.iter().copied()));
    // Its own folder: rustc writes the image's `.pdb` beside it.
    let directory = out.with_file_name("build-script-launcher-windows");
    std::fs::create_dir_all(&directory).expect("creating the launcher folder");
    let image = directory.join("launcher.exe");
    let result = command
        .arg("-o")
        .arg(&image)
        .arg(WINDOWS_SOURCE)
        .output()
        .expect("running rustc for the Windows build-script launcher");
    assert!(
        result.status.success(),
        "compiling {WINDOWS_SOURCE} for {target} failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::copy(&image, out).expect("copying the Windows launcher");
}

/// The `-L` search paths in the target's rustflags (cargo-xwin names the
/// Windows SDK and CRT library folders there).
fn search_paths<'a>(mut flags: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut kept = Vec::new();
    while let Some(flag) = flags.next() {
        match flag.strip_prefix("-L") {
            Some("") => match flags.next() {
                Some(path) => kept.push(format!("-L{path}")),
                None => break,
            },
            Some(path) => kept.push(format!("-L{path}")),
            None => {}
        }
    }
    kept
}

/// Whether Cargo is building for a glibc Linux target.
fn target_is_glibc() -> bool {
    let cfg = |name: &str| std::env::var(name).unwrap_or_default();
    cfg("CARGO_CFG_TARGET_OS") == "linux" && cfg("CARGO_CFG_TARGET_ENV") == "gnu"
}

/// The `-C` options from the target's rustflags that decide how a binary
/// links. Everything else (coverage instrumentation, lints, cfgs) is kache's
/// own business, not the launcher's.
fn link_flags<'a>(mut flags: impl Iterator<Item = &'a str>) -> Vec<String> {
    const LINK_OPTIONS: &[&str] = &[
        "linker",
        "linker-flavor",
        "link-arg",
        "link-args",
        "link-self-contained",
        "target-feature",
    ];
    let mut kept = Vec::new();
    while let Some(flag) = flags.next() {
        let option = match flag.strip_prefix("-C") {
            Some("") => match flags.next() {
                Some(option) => option,
                None => break,
            },
            Some(option) => option,
            None => continue,
        };
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if LINK_OPTIONS.contains(&name) {
            kept.push(format!("-C{option}"));
        }
    }
    kept
}
