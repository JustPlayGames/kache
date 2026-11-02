//! Link and run the launcher against the selected toolchain's real core.

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[cfg(unix)]
#[path = "../launcher/compile.rs"]
mod launcher;

#[cfg(unix)]
#[test]
fn the_launcher_links_with_current_core_and_runs_the_preserved_script() {
    let fixture = tempfile::tempdir().unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = Command::new(&rustc).arg("-vV").output().unwrap();
    assert!(version.status.success(), "{version:?}");
    let version = String::from_utf8(version.stdout).unwrap();
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("launcher/build_script.rs");
    let out = fixture.path().join("launcher");
    let mut flags: Vec<OsString> = Vec::new();
    if let Some(linker) = std::env::var_os("RUSTC_LINKER") {
        let mut flag = OsString::from("linker=");
        flag.push(linker);
        flags.extend(["-C".into(), flag]);
    }
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        flags.extend(["-C".into(), "link-arg=-lc".into()]);
    }
    let output = launcher::build(&rustc, host, &source, &out, &flags).unwrap();
    assert!(output.status.success(), "{output:?}");

    // A successful link is the symbol oracle. Executing the fallback also
    // proves that the generated launcher remains usable, rather than merely
    // carrying a chosen string somewhere in its image.
    let preserved = fixture.path().join("preserved");
    std::fs::write(&preserved, "#!/bin/sh\nexit 23\n").unwrap();
    std::fs::set_permissions(&preserved, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(fixture.path().join(".kache-launch"), b"preserved\0\0./\0").unwrap();
    let output = Command::new(&out).env_clear().output().unwrap();
    assert_eq!(output.status.code(), Some(23), "{output:?}");
}

/// A relative shim path can exceed MAX_PATH even when its resolved path
/// fits. Longer worktrees also need the resolved path's verbatim prefix.
#[cfg(windows)]
#[test]
fn the_windows_launcher_runs_pins_in_long_paths() {
    use std::fs;
    use std::os::windows::ffi::OsStrExt;

    let fixture = tempfile::tempdir().unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let source = Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("launcher/build_script_windows.rs");
    let launcher = fixture.path().join("launcher.exe");
    let probe_source = fixture.path().join("probe.rs");
    let probe = fixture.path().join("probe.exe");
    fs::write(
        &probe_source,
        r#"fn main() {
            println!("{:?}", std::env::var_os("KACHE_BUILD_SCRIPT_PATH"));
            println!("{}", std::env::args().nth(1).unwrap());
            std::process::exit(23);
        }"#,
    )
    .unwrap();
    for (source, binary) in [(&source, &launcher), (&probe_source, &probe)] {
        let output = Command::new(&rustc)
            .args(["--edition=2024", "-Ctarget-feature=+crt-static"])
            .arg(source)
            .arg("-o")
            .arg(binary)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    for shim_length in [240, 300] {
        let relative =
            Path::new(".kache-build-script-shims/0123456789abcdef0123456789abcdef/kache.exe");
        let padding = shim_length
            - fixture.path().as_os_str().encode_wide().count()
            - relative.as_os_str().encode_wide().count()
            - 2;
        let profile = fixture.path().join("p".repeat(padding));
        let unit = profile.join("build/icu_locale_fallback_data-0123456789abcdef");
        let pin = profile.join(relative);
        fs::create_dir_all(&unit).unwrap();
        fs::create_dir_all(pin.parent().unwrap()).unwrap();
        fs::copy(&probe, &pin).unwrap();
        let invoked = unit.join("build-script-build.exe");
        fs::copy(&launcher, &invoked).unwrap();
        // Cargo's ordinary path below MAX_PATH; a verbatim path when the
        // launcher itself is too deep for CreateProcess's ordinary form.
        let invoked = if shim_length > 260 {
            invoked.canonicalize().unwrap()
        } else {
            invoked
        };
        fs::copy(&probe, unit.join("real.exe")).unwrap();
        fs::write(
            unit.join(".kache-launch"),
            format!("real.exe\0{}\0../../\0", relative.display()),
        )
        .unwrap();
        assert!(
            unit.join("../../")
                .join(relative)
                .as_os_str()
                .encode_wide()
                .count()
                > 260
        );

        let output = Command::new(&invoked)
            .arg("argument with spaces")
            .env_remove("KACHE_BUILD_SCRIPT_PATH")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(23),
            "shim length {shim_length}: {output:?}"
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.starts_with("Some("),
            "the pinned shim must run: {stdout}"
        );
        assert!(stdout.ends_with("argument with spaces\n"), "{stdout}");

        fs::remove_file(&pin).unwrap();
        let output = Command::new(&invoked)
            .arg("fallback")
            .env_remove("KACHE_BUILD_SCRIPT_PATH")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(23), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "None\nfallback\n"
        );
    }
}
