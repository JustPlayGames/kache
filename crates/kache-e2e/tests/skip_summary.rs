use std::process::Command;

#[test]
fn lenient_skip_summary_reports_the_fixture_count() {
    let temp = tempfile::tempdir().unwrap();
    let scenario = temp.path().join("e2e-missing-tool");
    std::fs::create_dir_all(scenario.join("source")).unwrap();
    std::fs::write(
        scenario.join("scenario.toml"),
        r#"
name = "e2e-missing-tool"
tags = ["suite:e2e"]
requires = ["kache-e2e-tool-that-does-not-exist"]

[source]
kind = "fixture"
path = "source"

[commands]
build = "unused"
clean = "unused"
"#,
    )
    .unwrap();

    let output = Command::new(scenario_bin())
        .arg("--kache")
        .arg(scenario_bin())
        .arg("--scenarios")
        .arg(temp.path())
        .arg("--select")
        .arg("suite:e2e")
        .arg("--out")
        .arg(temp.path().join("results.json"))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("PASS: all executed fixtures green (1 skipped)")
    );
}

/// The `kache-scenario` binary Cargo built for these tests, read when the
/// test runs (a nextest archive moves it), never through `env!`.
fn scenario_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var_os("CARGO_BIN_EXE_kache-scenario")
            .expect("CARGO_BIN_EXE_kache-scenario is set by `cargo test` and `cargo nextest run`"),
    )
}
