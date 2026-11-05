use super::*;
use std::time::Duration;

fn later() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn at(path: &Path, seconds: i64) {
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
}

/// Cargo's encoding of a dep-info: `files` as (kind, path), no checksums.
fn encoded(files: &[(u8, &str)], env: &[(&str, Option<&str>)]) -> Vec<u8> {
    let mut out = ENCODED_HEADER.to_vec();
    out.extend((files.len() as u32).to_le_bytes());
    for (kind, path) in files {
        out.push(*kind);
        out.extend((path.len() as u32).to_le_bytes());
        out.extend(path.as_bytes());
        out.push(0);
    }
    out.extend((env.len() as u32).to_le_bytes());
    for (key, value) in env {
        out.extend((key.len() as u32).to_le_bytes());
        out.extend(key.as_bytes());
        match value {
            Some(value) => {
                out.push(1);
                out.extend((value.len() as u32).to_le_bytes());
                out.extend(value.as_bytes());
            }
            None => out.push(0),
        }
    }
    out
}

fn file(kind: PathKind, path: &str) -> DepFile {
    DepFile {
        kind,
        path: path.into(),
        checksum: None,
    }
}

#[test]
fn reads_cargos_encoded_dep_info_and_refuses_any_other_shape() {
    let bytes = encoded(
        &[(0, "src/lib.rs"), (1, "debug/build/p-1/out/x.rs")],
        &[("A", Some("1")), ("B", None)],
    );
    assert_eq!(
        EncodedDepInfo::parse(&bytes),
        Some(EncodedDepInfo {
            files: vec![
                file(PathKind::PackageRoot, "src/lib.rs"),
                file(PathKind::TargetRoot, "debug/build/p-1/out/x.rs")
            ],
            env: vec![("A".into(), Some("1".into())), ("B".into(), None)],
        })
    );
    assert_eq!(
        EncodedDepInfo::parse(&bytes).unwrap().encode(),
        Some(bytes.clone())
    );
    // As Cargo 1.99 writes `pub fn f() {}` in src/lib.rs.
    let cargo = [
        1, 0, 0, 0, 0xff, 1, 1, 0, 0, 0, 0, 10, 0, 0, 0, b's', b'r', b'c', b'/', b'l', b'i', b'b',
        b'.', b'r', b's', 0, 0, 0, 0, 0,
    ];
    assert_eq!(
        EncodedDepInfo::parse(&cargo).unwrap().files,
        vec![file(PathKind::PackageRoot, "src/lib.rs")]
    );
    // With a checksum, which a rebased copy keeps.
    let mut summed = ENCODED_HEADER.to_vec();
    summed.extend(1u32.to_le_bytes());
    summed.push(0);
    summed.extend(4u32.to_le_bytes());
    summed.extend(b"a.rs");
    summed.push(1);
    summed.extend(9u64.to_le_bytes());
    summed.push(1);
    summed.extend(2u32.to_le_bytes());
    summed.extend([0xab, 0xcd]);
    summed.extend(0u32.to_le_bytes());
    let info = EncodedDepInfo::parse(&summed).expect("checksummed");
    assert_eq!(info.files[0].checksum, Some((9, 1, vec![0xab, 0xcd])));
    assert_eq!(info.encode(), Some(summed));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(EncodedDepInfo::parse(&trailing), None);
    assert_eq!(EncodedDepInfo::parse(&bytes[..bytes.len() - 1]), None);
    assert_eq!(EncodedDepInfo::parse(&bytes[1..]), None, "no header");
    let mut kind = bytes.clone();
    kind[ENCODED_HEADER.len() + 4] = 2;
    assert_eq!(EncodedDepInfo::parse(&kind), None, "unknown root");
}

/// Two checkouts holding package `p` at `crates/p` with the same bytes, and a
/// donor unit of it whose dep-info lists `files`, dated after every source.
struct Fixture {
    _dir: tempfile::TempDir,
    donor: PathBuf,
    ours: PathBuf,
    fingerprint: PathBuf,
}

const BUILT: i64 = 1_800_000_000;

impl Fixture {
    fn new(files: &[(u8, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let donor = dir.path().join("donor");
        let ours = dir.path().join("ours");
        for root in [&donor, &ours] {
            write(
                &root.join("Cargo.toml"),
                b"[workspace]\nmembers = [\"crates/p\"]\n",
            );
            write(
                &root.join("crates/p/Cargo.toml"),
                b"[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
            );
            write(&root.join("crates/p/src/lib.rs"), b"pub fn f() {}\n");
            write(&root.join("crates/p/assets/a.txt"), b"asset\n");
            write(&root.join("shared/x.txt"), b"shared\n");
        }
        for file in [
            "Cargo.toml",
            "crates/p/Cargo.toml",
            "crates/p/src/lib.rs",
            "crates/p/assets/a.txt",
            "shared/x.txt",
        ] {
            at(&donor.join(file), BUILT - 100);
        }
        let fingerprint = donor.join("target/debug/.fingerprint/p-0123456789abcdef");
        write(&fingerprint.join("lib-p"), b"0011223344556677");
        write(&fingerprint.join("dep-lib-p"), &encoded(files, &[]));
        at(&fingerprint.join("dep-lib-p"), BUILT);
        Self {
            _dir: dir,
            donor,
            ours,
            fingerprint,
        }
    }

    fn check(&self) -> Option<CrossCheckout> {
        CrossCheckout::new(
            &self.donor,
            &self.donor.join("target"),
            &self.ours,
            &BTreeSet::from(["p".to_owned()]),
            true,
            later(),
        )
    }

    fn verify(&self) -> Option<Verified> {
        self.check()?
            .verify("p", &self.fingerprint, &record_of(&self.fingerprint))
    }
}

#[test]
fn a_unit_built_from_this_checkouts_bytes_is_verified_and_dated_to_the_check() {
    let fixture = Fixture::new(&[(0, "src/lib.rs"), (0, "../../shared/x.txt")]);
    let before = SystemTime::now();
    let verified = fixture.verify().expect("same bytes");
    assert!(
        verified.stamp >= crate::build_script::write_floor(before)
            && verified.stamp <= SystemTime::now()
    );
    // Only the dep-info takes the date.
    let copy = fixture.donor.join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    for name in ["dep-lib-p", "lib-p"] {
        std::fs::copy(fixture.fingerprint.join(name), copy.join(name)).unwrap();
        at(&copy.join(name), BUILT);
    }
    fixture
        .check()
        .unwrap()
        .date(&copy, verified.stamp)
        .unwrap();
    assert_eq!(modified(&copy.join("dep-lib-p")), Some(verified.stamp));
    assert_eq!(
        modified(&copy.join("lib-p")),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(BUILT as u64))
    );
}

/// A change made to a fixture.
type Change = fn(&Fixture);

#[test]
fn any_difference_in_the_package_keeps_the_unit_out() {
    let cases: [(&str, Change); 6] = [
        ("an edited source, same length", |f| {
            write(&f.ours.join("crates/p/src/lib.rs"), b"pub fn g() {}\n")
        }),
        ("an asset the dep-info does not list", |f| {
            write(&f.ours.join("crates/p/assets/a.txt"), b"other\n")
        }),
        ("a new file in the package", |f| {
            write(&f.ours.join("crates/p/assets/b.txt"), b"new\n")
        }),
        ("a file gone from the package", |f| {
            std::fs::remove_file(f.ours.join("crates/p/assets/a.txt")).unwrap()
        }),
        ("a donor file newer than its build", |f| {
            at(&f.donor.join("crates/p/assets/a.txt"), BUILT + 1)
        }),
        ("a second package of the same name", |f| {
            write(
                &f.ours.join("vendor/p/Cargo.toml"),
                b"[package]\nname = \"p\"\nversion = \"0.2.0\"\n",
            )
        }),
    ];
    for (case, change) in cases {
        let fixture = Fixture::new(&[(0, "src/lib.rs")]);
        assert!(fixture.verify().is_some(), "{case}: before");
        change(&fixture);
        assert_eq!(fixture.verify(), None, "{case}");
    }
}

#[test]
fn files_outside_the_package_are_checked_and_some_paths_refused() {
    // Outside the package, inside the checkouts: compared by bytes and age.
    let fixture = Fixture::new(&[(0, "src/lib.rs"), (0, "../../shared/x.txt")]);
    write(&fixture.ours.join("shared/x.txt"), b"SHARED\n");
    assert_eq!(fixture.verify(), None, "different bytes");
    let fixture = Fixture::new(&[(0, "src/lib.rs"), (0, "../../shared/x.txt")]);
    at(&fixture.donor.join("shared/x.txt"), BUILT + 1);
    assert_eq!(fixture.verify(), None, "donor copy newer than its build");
    // Above the checkout.
    let fixture = Fixture::new(&[(0, "../../../elsewhere.txt")]);
    assert_eq!(fixture.verify(), None);
    // Generated in the target, and a checkout written out in full.
    let fixture = Fixture::new(&[(0, "src/lib.rs"), (1, "debug/build/p-1/out/x.rs")]);
    assert_eq!(fixture.verify(), None);
    // This checkout written out in full: not the donor's build.
    let fixture = Fixture::new(&[]);
    let ours = fixture.ours.join("shared/x.txt");
    let dep_info = fixture.fingerprint.join("dep-lib-p");
    write(&dep_info, &encoded(&[(1, ours.to_str().unwrap())], &[]));
    at(&dep_info, BUILT);
    assert_eq!(fixture.verify(), None);
    // The donor's checkout in full, climbing out of the package as Cargo
    // writes an `include_bytes!("../../../shared/x.txt")` from `src/`:
    // compared as above; climbing above the checkout or into its target is
    // refused.
    for (case, climbing, ours, kept) in [
        (
            "same bytes",
            "crates/p/src/../../../shared/x.txt",
            None,
            true,
        ),
        (
            "different bytes",
            "crates/p/src/../../../shared/x.txt",
            Some(b"SHARED\n"),
            false,
        ),
        (
            "above the checkout",
            "crates/../../elsewhere.txt",
            None,
            false,
        ),
        (
            "into the target",
            "crates/p/../../target/debug/x.txt",
            None,
            false,
        ),
    ] {
        let fixture = Fixture::new(&[]);
        for root in [&fixture.donor, &fixture.ours] {
            write(&root.join("target/debug/x.txt"), b"generated\n");
        }
        at(&fixture.donor.join("target/debug/x.txt"), BUILT - 1);
        let path = fixture.donor.join(climbing);
        let dep_info = fixture.fingerprint.join("dep-lib-p");
        write(&dep_info, &encoded(&[(1, path.to_str().unwrap())], &[]));
        at(&dep_info, BUILT);
        if let Some(bytes) = ours {
            write(&fixture.ours.join("shared/x.txt"), bytes);
        }
        assert_eq!(fixture.verify().is_some(), kept, "{case}");
    }
    // The same file for both, outside both checkouts: its age decides.
    let outside = tempfile::tempdir().unwrap();
    let header = outside.path().join("h.txt");
    write(&header, b"h\n");
    at(&header, BUILT - 1);
    let fixture = Fixture::new(&[(0, "src/lib.rs"), (0, header.to_str().unwrap())]);
    assert!(fixture.verify().is_some());
    at(&header, BUILT + 1);
    assert_eq!(fixture.verify(), None);
    // No dep-info at all: a build script's run.
    let fixture = Fixture::new(&[(0, "src/lib.rs")]);
    std::fs::remove_file(fixture.fingerprint.join("dep-lib-p")).unwrap();
    assert_eq!(fixture.verify(), None);
}

/// Cargo leaves the variables it sets out of its own dep-info and does not
/// compare them, so a copy would keep the donor's
/// `env!("CARGO_MANIFEST_DIR")`: the compiler's dep-info decides.
#[test]
fn a_unit_that_read_the_donors_checkout_from_the_environment_stays_out() {
    let fixture = Fixture::new(&[]);
    let check = fixture.check().unwrap();
    let donor = &fixture.donor;
    // As rustc writes a value (`escape_dep_env`): each backslash doubled.
    let escaped = |path: PathBuf| path.to_str().unwrap().replace('\\', r"\\");
    let dep_info = fixture.donor.join("target/debug/deps/p-0123456789abcdef.d");
    for (case, env, kept_out) in [
        (
            "its package's folder",
            format!(
                "CARGO_MANIFEST_DIR={}",
                escaped(donor.join("crates").join("p"))
            ),
            true,
        ),
        (
            "the checkout in a longer value",
            format!("V=x:{}", escaped(donor.join("y"))),
            true,
        ),
        (
            "its OUT_DIR in the target",
            format!(
                "OUT_DIR={}",
                escaped(
                    donor
                        .join("target")
                        .join("debug")
                        .join("build")
                        .join("p-1")
                        .join("out")
                )
            ),
            false,
        ),
        ("another path", "V=/usr/share/x".to_owned(), false),
        ("no value", "V".to_owned(), false),
    ] {
        write(
            &dep_info,
            format!("x.d: src/lib.rs\n\nsrc/lib.rs:\n\n# env-dep:{env}\n").as_bytes(),
        );
        assert_eq!(
            check.reads_checkout_from_env(std::slice::from_ref(&dep_info)),
            kept_out,
            "{case}"
        );
    }
    assert!(check.reads_checkout_from_env(&[]), "no dep-info to read");
    assert!(check.reads_checkout_from_env(&[fixture.donor.join("gone.d")]));
}

#[test]
fn a_package_is_compared_once_per_seeding() {
    let fixture = Fixture::new(&[(0, "src/lib.rs")]);
    let check = fixture.check().unwrap();
    let first = check
        .verify("p", &fixture.fingerprint, &record_of(&fixture.fingerprint))
        .unwrap();
    // A later change is seen by Cargo through its newer time, not by
    // comparing again within this seeding.
    write(
        &fixture.ours.join("crates/p/src/lib.rs"),
        b"pub fn g() {}\n",
    );
    assert_eq!(
        check.verify("p", &fixture.fingerprint, &record_of(&fixture.fingerprint)),
        Some(first.clone())
    );
    assert!(modified(&fixture.ours.join("crates/p/src/lib.rs")).unwrap() >= first.stamp);
    assert_eq!(
        check.verify("q", &fixture.fingerprint, &record_of(&fixture.fingerprint)),
        None,
        "not a package here"
    );
}

#[test]
fn a_manifest_walk_skips_targets_and_gives_up_past_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("a/Cargo.toml"),
        b"[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    write(
        &dir.path().join("target/package/a-0.1.0/Cargo.toml"),
        b"[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    );
    let packages = BTreeSet::from(["a".to_owned()]);
    let roots = package_roots(dir.path(), &packages, later()).unwrap();
    assert_eq!(roots.get("a").map(PathBuf::as_path), Some(Path::new("a")));
    assert_eq!(package_roots(dir.path(), &packages, Instant::now()), None);
}

#[test]
fn a_donor_file_written_out_in_full_is_compared_and_renamed_in_the_copy() {
    // As Clippy lists the workspace's manifest for every unit.
    let fixture = Fixture::new(&[]);
    let theirs = fixture.donor.join("Cargo.toml");
    let dep_info = fixture.fingerprint.join("dep-lib-p");
    let listed = [(0, "src/lib.rs"), (1, theirs.to_str().unwrap())];
    write(&dep_info, &encoded(&listed, &[("A", None)]));
    at(&dep_info, BUILT);
    let verified = fixture.verify().expect("same manifest");
    let copy = fixture.donor.join("copy");
    write(&copy.join("dep-lib-p"), &std::fs::read(&dep_info).unwrap());
    fixture
        .check()
        .unwrap()
        .date(&copy, verified.stamp)
        .unwrap();
    let ours = fixture.ours.join("Cargo.toml");
    let rebased = [(0, "src/lib.rs"), (1, ours.to_str().unwrap())];
    assert_eq!(
        std::fs::read(copy.join("dep-lib-p")).unwrap(),
        encoded(&rebased, &[("A", None)])
    );
    assert_eq!(modified(&copy.join("dep-lib-p")), Some(verified.stamp));
    // Another manifest, or a donor copy newer than the build, keeps it out.
    write(
        &fixture.ours.join("Cargo.toml"),
        b"[workspace]\nmembers = []\n",
    );
    assert_eq!(fixture.verify(), None);
    let fixture = Fixture::new(&[]);
    let theirs = fixture.donor.join("Cargo.toml");
    let listed = [(1, theirs.to_str().unwrap())];
    write(
        &fixture.fingerprint.join("dep-lib-p"),
        &encoded(&listed, &[]),
    );
    at(&fixture.fingerprint.join("dep-lib-p"), BUILT);
    at(&fixture.donor.join("Cargo.toml"), BUILT + 1);
    assert_eq!(fixture.verify(), None);
    // An unreadable dep-info in the copy is removed, so the unit is built.
    let bad = fixture.donor.join("bad");
    write(&bad.join("dep-lib-p"), b"junk");
    assert!(
        fixture
            .check()
            .unwrap()
            .date(&bad, SystemTime::now())
            .is_err()
    );
    assert!(!bad.join("dep-lib-p").exists());
}

/// Clippy lists `clippy.toml` by its canonical path: `\\?\C:\w\clippy.toml`
/// on Windows, the real path through a symbolic link on Unix. Spelled so,
/// the donor's file is still the donor's: compared by bytes and renamed in
/// the copy, never taken for one file outside both checkouts.
#[test]
fn a_donor_file_listed_by_another_spelling_of_its_checkout_is_compared() {
    let fixture = Fixture::new(&[]);
    #[cfg(unix)]
    let (donor_root, theirs) = {
        let alias = fixture.donor.with_file_name("alias");
        std::os::unix::fs::symlink(&fixture.donor, &alias).unwrap();
        (alias, fixture.donor.join("Cargo.toml"))
    };
    #[cfg(windows)]
    let (donor_root, theirs) = (
        fixture.donor.clone(),
        std::fs::canonicalize(fixture.donor.join("Cargo.toml")).unwrap(),
    );
    assert!(
        theirs.strip_prefix(&donor_root).is_err(),
        "another spelling"
    );
    let dep_info = fixture.fingerprint.join("dep-lib-p");
    let listed = [(0, "src/lib.rs"), (1, theirs.to_str().unwrap())];
    write(&dep_info, &encoded(&listed, &[]));
    at(&dep_info, BUILT);
    let check = || {
        CrossCheckout::new(
            &donor_root,
            &donor_root.join("target"),
            &fixture.ours,
            &BTreeSet::from(["p".to_owned()]),
            true,
            later(),
        )
        .unwrap()
    };
    let record = record_of(&fixture.fingerprint);
    let verified = check()
        .verify("p", &fixture.fingerprint, &record)
        .expect("same manifest");
    let copy = fixture.donor.join("copy");
    write(&copy.join("dep-lib-p"), &std::fs::read(&dep_info).unwrap());
    check().date(&copy, verified.stamp).unwrap();
    let ours = fixture.ours.join("Cargo.toml");
    let rebased = [(0, "src/lib.rs"), (1, ours.to_str().unwrap())];
    assert_eq!(
        std::fs::read(copy.join("dep-lib-p")).unwrap(),
        encoded(&rebased, &[])
    );
    write(
        &fixture.ours.join("Cargo.toml"),
        b"[workspace]\nmembers = []\n",
    );
    assert_eq!(
        check().verify("p", &fixture.fingerprint, &record),
        None,
        "another manifest"
    );
}

/// Where the unit at `fingerprint` (`<target>/debug/.fingerprint/<unit>`) has
/// its run's output recorded, if it is a run.
fn record_of(fingerprint: &Path) -> PathBuf {
    let profile = fingerprint.parent().unwrap().parent().unwrap();
    profile
        .join("build")
        .join(fingerprint.file_name().unwrap())
        .join("output")
}

/// A run of `p`'s build script in the donor, built at [`BUILT`], whose record
/// holds `local` (Cargo's `local` list), next to the fixture's compiled unit.
fn run_record(fixture: &Fixture, local: &str) -> PathBuf {
    let run = fixture
        .donor
        .join("target/debug/.fingerprint/p-fedcba9876543210");
    write(
        &run.join("run-build-script-build-script-build"),
        b"8899aabbccddeeff",
    );
    write(
        &run.join("run-build-script-build-script-build.json"),
        format!(r#"{{"rustc":1,"local":{local},"deps":[]}}"#).as_bytes(),
    );
    let output = fixture
        .donor
        .join("target/debug/build/p-fedcba9876543210/output");
    write(&output, b"cargo:rerun-if-changed=build.rs\n");
    at(&output, BUILT);
    run
}

const DECLARED: &str = r#"[{"RerunIfChanged":{"output":"debug/build/p-fedcba9876543210/output","paths":["build.rs","assets/a.txt","../../shared/x.txt"]}},{"RerunIfEnvChanged":{"var":"V","val":null}}]"#;

fn run_fixture() -> (Fixture, PathBuf) {
    let fixture = Fixture::new(&[(0, "src/lib.rs")]);
    for root in [&fixture.donor, &fixture.ours] {
        write(&root.join("crates/p/build.rs"), b"fn main() {}\n");
    }
    at(&fixture.donor.join("crates/p/build.rs"), BUILT - 100);
    // This checkout came after the donor's build.
    for file in ["crates/p/build.rs", "crates/p/assets/a.txt", "shared/x.txt"] {
        at(&fixture.ours.join(file), BUILT + 1_000);
    }
    let run = run_record(&fixture, DECLARED);
    (fixture, run)
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A run's declared files take the donor's times here, so Cargo finds the
/// copied output (which keeps the donor's time) no older than them, and the
/// units above it keep theirs.
#[test]
fn a_run_on_the_same_declared_bytes_gives_its_files_the_donors_times() {
    let (fixture, run) = run_fixture();
    let check = fixture.check().unwrap();
    let verified = check
        .verify("p", &run, &record_of(&run))
        .expect("same declared bytes");
    // An untouched file already older than the run keeps its time.
    let shared = fixture.ours.join("shared/x.txt");
    at(&shared, BUILT - 5_000);
    check.finish(&run, &verified).unwrap();
    for file in ["crates/p/build.rs", "crates/p/assets/a.txt"] {
        assert_eq!(
            secs(modified(&fixture.ours.join(file)).unwrap()),
            (BUILT - 100) as u64,
            "{file}"
        );
    }
    assert_eq!(secs(modified(&shared).unwrap()), (BUILT - 5_000) as u64);
    assert_eq!(
        std::fs::read(fixture.ours.join("crates/p/build.rs")).unwrap(),
        b"fn main() {}\n"
    );
}

#[test]
fn a_run_is_kept_out_unless_every_declared_file_checks_out() {
    let cases: [(&str, Change); 4] = [
        ("declared file differs", |f| {
            write(&f.ours.join("crates/p/assets/a.txt"), b"other\n")
        }),
        ("declared file outside the package differs", |f| {
            write(&f.ours.join("shared/x.txt"), b"other\n")
        }),
        ("donor's file newer than its run", |f| {
            at(&f.donor.join("crates/p/build.rs"), BUILT + 1)
        }),
        ("declared file gone here", |f| {
            std::fs::remove_file(f.ours.join("crates/p/build.rs")).unwrap()
        }),
    ];
    for (case, change) in cases {
        let (fixture, run) = run_fixture();
        assert!(fixture.verify_at(&run).is_some(), "{case}: before");
        change(&fixture);
        assert_eq!(fixture.verify_at(&run), None, "{case}");
    }
    let records = [
        (
            "a directory with a donor file newer than the run",
            r#"[{"RerunIfChanged":{"output":"debug/build/p-fedcba9876543210/output","paths":["src"]}}]"#,
        ),
        (
            "a package's time beside declared files",
            r#"[{"Precalculated":"1.5s (src/lib.rs)"},{"RerunIfEnvChanged":{"var":"V","val":null}},{"RerunIfChanged":{"output":"debug/build/p-fedcba9876543210/output","paths":["build.rs"]}}]"#,
        ),
        (
            "a checkout file in full",
            r#"[{"RerunIfChanged":{"output":"debug/build/p-fedcba9876543210/output","paths":["DONOR/crates/p/build.rs"]}}]"#,
        ),
        (
            "an output outside the target",
            r#"[{"RerunIfChanged":{"output":"../output","paths":["build.rs"]}}]"#,
        ),
        ("a record of another shape", r#"{"local":7}"#),
    ];
    for (case, local) in records {
        let (fixture, _) = run_fixture();
        let local = local.replace("DONOR", fixture.donor.to_str().unwrap());
        at(&fixture.donor.join("crates/p/src/lib.rs"), BUILT + 1);
        let run = run_record(&fixture, &local);
        assert_eq!(fixture.verify_at(&run), None, "{case}");
    }
    // Environment alone: Cargo compares it itself.
    let (fixture, _) = run_fixture();
    let run = run_record(&fixture, r#"[{"RerunIfEnvChanged":{"var":"V","val":"1"}}]"#);
    assert!(fixture.verify_at(&run).is_some());
}

/// A file written between the check and the new time gets the present time
/// back, and the copied run loses its fingerprint: Cargo runs it.
#[test]
fn a_declared_file_changed_while_seeding_keeps_the_run_out() {
    let (fixture, run) = run_fixture();
    let check = fixture.check().unwrap();
    let verified = check.verify("p", &run, &record_of(&run)).unwrap();
    let copy = fixture.donor.join("copy");
    for name in [
        "run-build-script-build-script-build",
        "run-build-script-build-script-build.json",
    ] {
        write(&copy.join(name), &std::fs::read(run.join(name)).unwrap());
    }
    write(&fixture.ours.join("crates/p/assets/a.txt"), b"edited\n");
    at(&fixture.ours.join("crates/p/assets/a.txt"), BUILT + 2_000);
    let before = SystemTime::now() - Duration::from_secs(1);
    assert!(check.finish(&copy, &verified).is_err());
    assert!(modified(&fixture.ours.join("crates/p/assets/a.txt")).unwrap() >= before);
    assert!(super::super::entries(&copy).is_empty());
}

impl Fixture {
    fn verify_at(&self, fingerprint: &Path) -> Option<Verified> {
        self.check()?
            .verify("p", fingerprint, &record_of(fingerprint))
    }
}

/// A declared directory, and a run Cargo judges by its package's newest file,
/// take the donor's time for every entry under them: Cargo's walk of a
/// directory takes directories' times too, and a package's newest time is
/// hashed into the run's fingerprint, so that one must be the very same.
#[test]
fn a_declared_directory_and_a_package_judged_by_time_take_every_time() {
    for (case, local, exact) in [
        (
            "directory",
            r#"[{"RerunIfChanged":{"output":"debug/build/p-fedcba9876543210/output","paths":["assets"]}}]"#,
            false,
        ),
        (
            "package",
            r#"[{"Precalculated":"1799999900.0s (Cargo.toml)"}]"#,
            true,
        ),
    ] {
        let (fixture, _) = run_fixture();
        let run = run_record(&fixture, local);
        for dir in ["crates/p", "crates/p/src", "crates/p/assets"] {
            at(&fixture.donor.join(dir), BUILT - 50);
            at(&fixture.ours.join(dir), BUILT + 1_000);
        }
        let check = fixture.check().unwrap();
        let verified = check
            .verify("p", &run, &record_of(&run))
            .unwrap_or_else(|| panic!("{case}"));
        // Older than the run is not enough for a package's time.
        at(&fixture.ours.join("crates/p/src/lib.rs"), BUILT - 7_000);
        check.finish(&run, &verified).unwrap();
        for (path, want) in [
            ("crates/p/assets", BUILT - 50),
            ("crates/p/assets/a.txt", BUILT - 100),
        ] {
            assert_eq!(
                secs(modified(&fixture.ours.join(path)).unwrap()),
                want as u64,
                "{case}: {path}"
            );
        }
        let lib = secs(modified(&fixture.ours.join("crates/p/src/lib.rs")).unwrap());
        assert_eq!(
            lib,
            if exact { BUILT - 100 } else { BUILT - 7_000 } as u64,
            "{case}"
        );
    }
    // An entry more on one side keeps the directory out.
    let (fixture, _) = run_fixture();
    let run = run_record(
        &fixture,
        r#"[{"RerunIfChanged":{"output":"debug/build/p-fedcba9876543210/output","paths":["assets"]}}]"#,
    );
    write(&fixture.ours.join("crates/p/assets/new.txt"), b"x");
    assert_eq!(fixture.verify_at(&run), None);
}

/// A compiled unit may read files its own package's run wrote (Cargo then
/// rebuilds it if that run is not copied and runs), never another's.
#[test]
fn generated_sources_count_only_from_the_units_own_run() {
    for (path, ok) in [
        ("debug/build/p-fedcba9876543210/out/gen.rs", true),
        ("debug/build/q-fedcba9876543210/out/gen.rs", false),
        ("debug/build/p-fedcba9876543210/output", false),
        ("debug/build/p-fedcba9876543210/out/../../x.rs", false),
        ("debug/deps/p.rs", false),
    ] {
        let fixture = Fixture::new(&[(0, "src/lib.rs"), (1, path)]);
        assert_eq!(fixture.verify().is_some(), ok, "{path}");
    }
}

/// No run is copied into a checkout whose other targets could take its
/// files' older times for fresh.
#[test]
fn no_run_without_leave_to_adopt_times() {
    let (fixture, run) = run_fixture();
    let check = CrossCheckout::new(
        &fixture.donor,
        &fixture.donor.join("target"),
        &fixture.ours,
        &BTreeSet::from(["p".to_owned()]),
        false,
        later(),
    )
    .unwrap();
    assert_eq!(check.verify("p", &run, &record_of(&run)), None);
    assert!(
        check
            .verify("p", &fixture.fingerprint, &record_of(&fixture.fingerprint))
            .is_some(),
        "compiled units still"
    );
}

/// A run of another checkout whose record names that checkout is copied
/// only where the copy can name this one: at the start of a
/// `rerun-if-changed` path, or of a `rustc-link-search` directory whose
/// files are the same bytes in both checkouts.
#[test]
fn a_record_naming_the_donors_checkout_is_copied_only_where_it_rebases() {
    type Edit = fn(&Fixture, &Path);
    let cases: [(&str, &str, Edit, bool); 9] = [
        (
            "declared file and searched directory",
            "cargo:rerun-if-changed=DONOR/shared/x.txt\ncargo::rustc-link-search=native=DONOR/crates/p/lib\ncargo:rustc-link-lib=static=q\n",
            |_, _| {},
            true,
        ),
        (
            "a link search without a kind",
            "cargo:rustc-link-search=DONOR/crates/p/lib\n",
            |_, _| {},
            true,
        ),
        (
            "a library in the searched directory differs",
            "cargo:rustc-link-search=native=DONOR/crates/p/lib\n",
            |fixture, _| write(&fixture.ours.join("crates/p/lib/libq.a"), b"other"),
            false,
        ),
        (
            "the searched directory has one more file here",
            "cargo:rustc-link-search=native=DONOR/crates/p/lib\n",
            |fixture, _| write(&fixture.ours.join("crates/p/lib/libr.a"), b"r"),
            false,
        ),
        (
            "an environment variable a compiler reads",
            "cargo:rustc-env=DIR=DONOR/crates/p\n",
            |_, _| {},
            false,
        ),
        (
            "a link argument",
            "cargo:rustc-link-arg=DONOR/crates/p/lib/libq.a\n",
            |_, _| {},
            false,
        ),
        (
            "a line Cargo does not read",
            "running DONOR/crates/p/build.rs\n",
            |_, _| {},
            false,
        ),
        (
            "the checkout inside a path",
            "cargo:rerun-if-changed=/x/DONOR/shared/x.txt\n",
            |_, _| {},
            false,
        ),
        (
            "the donor's target beside its OUT_DIR",
            "cargo:rustc-link-search=native=DONOR/target/debug/deps\n",
            |_, _| {},
            false,
        ),
    ];
    for (case, record, change, copied) in cases {
        let (fixture, run) = run_fixture();
        for root in [&fixture.donor, &fixture.ours] {
            write(&root.join("crates/p/lib/libq.a"), b"!<arch>\nq");
        }
        let output = record_of(&run);
        write(
            &output,
            record
                .replace("DONOR", fixture.donor.to_str().unwrap())
                .as_bytes(),
        );
        at(&output, BUILT);
        change(&fixture, &output);
        assert_eq!(fixture.verify_at(&run).is_some(), copied, "{case}");
    }
}
