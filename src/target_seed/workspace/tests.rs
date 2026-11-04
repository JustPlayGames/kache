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
    assert_eq!(EncodedDepInfo::parse(&bytes).unwrap().encode(), Some(bytes.clone()));
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
            write(&root.join("Cargo.toml"), b"[workspace]\nmembers = [\"crates/p\"]\n");
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
            &self.ours,
            &BTreeSet::from(["p".to_owned()]),
            later(),
        )
    }

    fn verify(&self) -> Option<Verified> {
        self.check()?.verify("p", &self.fingerprint)
    }
}

#[test]
fn a_unit_built_from_this_checkouts_bytes_is_verified_and_dated_to_the_check() {
    let fixture = Fixture::new(&[(0, "src/lib.rs"), (0, "../../shared/x.txt")]);
    let before = SystemTime::now();
    let verified = fixture.verify().expect("same bytes");
    assert!(verified.stamp >= before && verified.stamp <= SystemTime::now());
    // Only the dep-info takes the date.
    let copy = fixture.donor.join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    for name in ["dep-lib-p", "lib-p"] {
        std::fs::copy(fixture.fingerprint.join(name), copy.join(name)).unwrap();
        at(&copy.join(name), BUILT);
    }
    fixture.check().unwrap().date(&copy, verified.stamp).unwrap();
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

#[test]
fn a_package_is_compared_once_per_seeding() {
    let fixture = Fixture::new(&[(0, "src/lib.rs")]);
    let check = fixture.check().unwrap();
    let first = check.verify("p", &fixture.fingerprint).unwrap();
    // A later change is seen by Cargo through its newer time, not by
    // comparing again within this seeding.
    write(&fixture.ours.join("crates/p/src/lib.rs"), b"pub fn g() {}\n");
    assert_eq!(check.verify("p", &fixture.fingerprint), Some(first));
    assert!(modified(&fixture.ours.join("crates/p/src/lib.rs")).unwrap() >= first.stamp);
    assert_eq!(check.verify("q", &fixture.fingerprint), None, "not a package here");
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
    fixture.check().unwrap().date(&copy, verified.stamp).unwrap();
    let ours = fixture.ours.join("Cargo.toml");
    let rebased = [(0, "src/lib.rs"), (1, ours.to_str().unwrap())];
    assert_eq!(
        std::fs::read(copy.join("dep-lib-p")).unwrap(),
        encoded(&rebased, &[("A", None)])
    );
    assert_eq!(modified(&copy.join("dep-lib-p")), Some(verified.stamp));
    // Another manifest, or a donor copy newer than the build, keeps it out.
    write(&fixture.ours.join("Cargo.toml"), b"[workspace]\nmembers = []\n");
    assert_eq!(fixture.verify(), None);
    let fixture = Fixture::new(&[]);
    let theirs = fixture.donor.join("Cargo.toml");
    let listed = [(1, theirs.to_str().unwrap())];
    write(&fixture.fingerprint.join("dep-lib-p"), &encoded(&listed, &[]));
    at(&fixture.fingerprint.join("dep-lib-p"), BUILT);
    at(&fixture.donor.join("Cargo.toml"), BUILT + 1);
    assert_eq!(fixture.verify(), None);
    // An unreadable dep-info in the copy is removed, so the unit is built.
    let bad = fixture.donor.join("bad");
    write(&bad.join("dep-lib-p"), b"junk");
    assert!(fixture.check().unwrap().date(&bad, SystemTime::now()).is_err());
    assert!(!bad.join("dep-lib-p").exists());
}
