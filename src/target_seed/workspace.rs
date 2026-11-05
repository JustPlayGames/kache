//! Workspace (path) units copied from a donor target of another checkout,
//! once the sources they were built from are shown to be the same bytes as
//! this checkout's.
//!
//! Cargo judges a path unit by modification times: it is fresh while no file
//! its dep-info lists is newer than the dep-info file itself. A new
//! checkout's files are newer than any other checkout's build, so a copied
//! unit is never fresh as it stands, and one whose donor sources differ
//! could pass as fresh if its stamps happened to fit. A unit is copied only
//! when:
//!
//! - the package's directory in the donor checkout holds the same entries
//!   with the same bytes as in this one (files a build reads without
//!   declaring them, such as a macro's assets, included), and nothing in it
//!   is newer than the donor's dep-info, so the donor built it from those
//!   bytes;
//! - every file its dep-info lists outside the package is the same file
//!   (outside both checkouts) or the same bytes (inside them), and no newer
//!   than the donor's dep-info either;
//! - its dep-info lists nothing in the target directory (a build script's
//!   generated sources, which the donor's run made).
//!
//! Cargo writes a file outside the package with its full path (Clippy lists
//! the workspace's `Cargo.toml` and `clippy.toml` for every unit). The
//! copy's dep-info names this checkout's file instead of the donor's
//! ([`date_and_rebase`]), so Cargo watches the right one, and takes the time
//! the check started ([`Verified::stamp`]) as its modification time: Cargo
//! finds the unit fresh and rebuilds it as soon as one of its files changes
//! after the check. Environment variables a unit read keep their recorded values, and
//! Cargo compares them itself, as it does flags, features and dependencies
//! through the unit's fingerprint hash.
//!
//! A build script's run has no dep-info. Cargo keeps it fresh while no file
//! its output declared with `rerun-if-changed` is newer than that output,
//! and the units above it while the output is no newer than theirs, so
//! dating the copied output would rebuild everything above it. Instead each
//! declared file, once found the same bytes as the donor's and no newer
//! than the donor's run, takes the donor's time in this checkout
//! ([`CrossCheckout::finish`]); it is compared again after, and a file that
//! changed meanwhile gets the present time back and keeps the run out. A
//! run whose package Cargo judges by its newest file (no `rerun-if-changed`),
//! that declares a directory, or names a file of either checkout in full is
//! not copied.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

/// Entries a walk for the workspace's manifests looks at before it gives up:
/// a checkout this large seeds registry units only.
const MAX_MANIFEST_WALK: usize = 200_000;

/// Directories a manifest walk does not enter.
const SKIPPED_DIRS: [&str; 3] = ["target", ".git", "node_modules"];

/// Bytes read at a time when two files are compared.
const COMPARE_CHUNK: usize = 1 << 16;

/// The check of one donor checkout's path units against this checkout.
pub(super) struct CrossCheckout {
    donor_root: PathBuf,
    our_root: PathBuf,
    /// The donor's target directory, which a run's record names its output
    /// from.
    donor_target: PathBuf,
    /// Whether this checkout's files may take the donor's times: only while
    /// no other target of this checkout could take them for fresh.
    adopt: bool,
    /// Each path package's directory, relative to the workspace root, as
    /// this checkout's manifests place it.
    roots: HashMap<String, PathBuf>,
    /// What each package's directory comparison found, once made.
    /// A copy thread that needs a package another is comparing waits for
    /// that comparison rather than making its own.
    packages: Mutex<HashMap<String, Arc<OnceLock<Option<PackageCheck>>>>>,
    /// Both checkouts' canonical paths, resolved the first time a dep-info
    /// lists a path spelled otherwise than either root ([`Self::place`]).
    canonical_roots: OnceLock<[Option<PathBuf>; 2]>,
    deadline: Instant,
}

/// Where an absolute path a donor's dep-info lists lies
/// ([`CrossCheckout::place`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    /// In the donor's checkout, at this path below its root.
    Donor(PathBuf),
    /// In this checkout.
    Ours,
    /// Outside both: the same file for either.
    Outside,
}

/// A package directory found the same in both checkouts.
#[derive(Debug, Clone, Copy)]
struct PackageCheck {
    /// When the comparison started, less a file system clock tick: no later
    /// change to this checkout's copy went unseen by it.
    checked_from: SystemTime,
    /// The newest modification time of the donor's copy.
    donor_newest: SystemTime,
}

/// A unit that may be copied, and the time its dep-info takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verified {
    pub(super) stamp: SystemTime,
    /// For a build script's run: the files whose times this checkout takes.
    run: Option<RunInputs>,
}

impl Verified {
    /// Whether this is a build script's run.
    pub(super) fn is_run(&self) -> bool {
        self.run.is_some()
    }
}

/// What a copied run needs before Cargo looks at it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunInputs {
    /// The time of the donor's run output, which the copy keeps.
    output_time: SystemTime,
    /// Every input takes the donor's very time (a run judged by its
    /// package's newest file), not just one no later than the run.
    exact: bool,
    inputs: Vec<Adopted>,
}

/// A declared input of a run, a file with the same bytes or a directory with
/// the same entries in both checkouts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Adopted {
    theirs: PathBuf,
    ours: PathBuf,
    /// The donor's modification time, no later than its run.
    time: SystemTime,
    directory: bool,
}

impl Adopted {
    /// Whether this checkout's copy still matches the donor's.
    fn still_same(&self, deadline: Instant) -> bool {
        if self.directory {
            sorted_entries(&self.theirs)
                .is_some_and(|theirs| Some(theirs) == sorted_entries(&self.ours))
        } else {
            same_bytes(&self.theirs, &self.ours, deadline)
        }
    }
}

/// `theirs` and `ours` and everything under them, when they hold the same
/// entries and bytes (no symbolic link) and nothing of the donor's is newer
/// than `limit`. Directories count: Cargo's walk of a declared directory
/// takes theirs too. `Bytes::Compared` skips reading files a package
/// comparison already found the same; [`CrossCheckout::finish`] reads again
/// each file whose time it changes.
fn tree_inputs(
    theirs: &Path,
    ours: &Path,
    limit: Option<SystemTime>,
    bytes: Bytes,
    deadline: Instant,
) -> Option<Vec<Adopted>> {
    let mut found = Vec::new();
    let mut pending = vec![(theirs.to_path_buf(), ours.to_path_buf())];
    while let Some((their_path, our_path)) = pending.pop() {
        if Instant::now() >= deadline {
            return None;
        }
        let their_meta = std::fs::symlink_metadata(&their_path).ok()?;
        let our_meta = std::fs::symlink_metadata(&our_path).ok()?;
        let time = their_meta.modified().ok()?;
        if limit.is_some_and(|limit| time > limit) {
            return None;
        }
        let directory = their_meta.is_dir();
        if directory && our_meta.is_dir() {
            let names = sorted_entries(&their_path)?;
            if sorted_entries(&our_path)? != names {
                return None;
            }
            for name in names {
                pending.push((their_path.join(&name), our_path.join(&name)));
            }
        } else if !(their_meta.is_file()
            && our_meta.is_file()
            && their_meta.len() == our_meta.len()
            && (bytes == Bytes::Compared || same_bytes(&their_path, &our_path, deadline)))
        {
            return None;
        }
        found.push(Adopted {
            theirs: their_path,
            ours: our_path,
            time,
            directory,
        });
    }
    Some(found)
}

/// Whether [`tree_inputs`] reads the files it walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bytes {
    /// Read and compared.
    Read,
    /// Inside a package found the same in both checkouts.
    Compared,
}

/// Whether `path` (from the target directory, as a dep-info writes it) is
/// under `package`'s own build-script `OUT_DIR`:
/// `<profile>/build/<package>-<hash>/out/...`.
fn in_own_out_dir(path: &Path, package: &str) -> bool {
    let parts: Vec<_> = path.components().collect();
    match parts.as_slice() {
        [
            Component::Normal(_),
            Component::Normal(build),
            Component::Normal(unit),
            Component::Normal(out),
            rest @ ..,
        ] if *build == "build" && *out == "out" && !rest.is_empty() => {
            rest.iter().all(|c| matches!(c, Component::Normal(_)))
                && unit
                    .to_str()
                    .and_then(|unit| unit.rsplit_once('-'))
                    .is_some_and(|(name, hash)| {
                        name == package && crate::cargo_layout::is_unit_hash(hash)
                    })
        }
        _ => false,
    }
}

/// How a run's fingerprint record (`run-build-script-*.json`) says Cargo
/// judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunCheck {
    /// `rerun-if-changed`: these files (from the package root, or in full)
    /// against the output at `output` (from the target directory).
    Files {
        output: PathBuf,
        paths: Vec<PathBuf>,
    },
    /// `rerun-if-env-changed`: compared by Cargo itself.
    Env,
    /// No `rerun-if-changed`: the package's newest file and its time.
    Package,
}

impl CrossCheckout {
    /// The check for `donor_root`'s units of `packages`, or `None` when this
    /// checkout's manifests cannot be read by `deadline`.
    pub(super) fn new(
        donor_root: &Path,
        donor_target: &Path,
        our_root: &Path,
        packages: &BTreeSet<String>,
        adopt: bool,
        deadline: Instant,
    ) -> Option<Self> {
        // A checkout inside the other would make one path name both.
        if donor_root.starts_with(our_root) || our_root.starts_with(donor_root) {
            return None;
        }
        let roots = package_roots(our_root, packages, deadline)?;
        Some(Self {
            donor_root: donor_root.to_path_buf(),
            our_root: our_root.to_path_buf(),
            donor_target: donor_target.to_path_buf(),
            adopt,
            roots,
            packages: Mutex::new(HashMap::new()),
            canonical_roots: OnceLock::new(),
            deadline,
        })
    }

    /// The donor's checkout and this one.
    pub(super) fn checkouts(&self) -> (&Path, &Path) {
        (&self.donor_root, &self.our_root)
    }

    /// Whether the donor's unit of `package`, whose fingerprint directory is
    /// `fingerprint`, was built from this checkout's bytes. `record` is
    /// where a run's output would be recorded (`stdout`), which only runs
    /// have.
    pub(super) fn verify(
        &self,
        package: &str,
        fingerprint: &Path,
        record: &Path,
    ) -> Option<Verified> {
        let relative = self.roots.get(package)?;
        let check = self.package(package, relative)?;
        let dep_infos: Vec<PathBuf> = super::entries(fingerprint)
            .into_iter()
            .filter(|file| super::file_name(file).starts_with("dep-"))
            .collect();
        if dep_infos.is_empty() {
            return self.verify_run(relative, fingerprint, record, check);
        }
        let theirs = self.donor_root.join(relative);
        let ours = self.our_root.join(relative);
        for dep_info in &dep_infos {
            let reference = modified(dep_info)?;
            if check.donor_newest > reference {
                return None;
            }
            let info = EncodedDepInfo::parse(&std::fs::read(dep_info).ok()?)?;
            for file in &info.files {
                let path = Path::new(&file.path);
                // Relative to the target: generated by a run. Its own
                // package's run is a dependency of the unit, so Cargo
                // rebuilds the unit when that run is not copied and runs.
                if file.kind == PathKind::TargetRoot && !path.is_absolute() {
                    if Instant::now() >= self.deadline || !in_own_out_dir(path, package) {
                        return None;
                    }
                    continue;
                }
                if Instant::now() >= self.deadline
                    || !self.outside_file_holds(&theirs, &ours, path, reference)
                {
                    return None;
                }
            }
        }
        Some(Verified {
            stamp: check.checked_from,
            run: None,
        })
    }

    /// Whether the donor's build-script run at `fingerprint`, of the package
    /// at `relative`, ran on the bytes its declared files hold here.
    fn verify_run(
        &self,
        relative: &Path,
        fingerprint: &Path,
        record: &Path,
        check: PackageCheck,
    ) -> Option<Verified> {
        if !self.adopt || !self.record_rebases(record) {
            return None;
        }
        let checks = run_checks(fingerprint)?;
        let theirs = self.donor_root.join(relative);
        let ours = self.our_root.join(relative);
        if checks == [RunCheck::Package] {
            // Cargo hashes the package's newest file and its time into the
            // run's fingerprint: the same bytes need the same times.
            return Some(Verified {
                stamp: check.checked_from,
                run: Some(RunInputs {
                    output_time: SystemTime::UNIX_EPOCH,
                    exact: true,
                    inputs: tree_inputs(&theirs, &ours, None, Bytes::Compared, self.deadline)?,
                }),
            });
        }
        let mut output_time = None;
        let mut inputs = Vec::new();
        for run_check in checks {
            let RunCheck::Files { output, paths } = run_check else {
                if run_check == RunCheck::Package {
                    return None;
                }
                continue;
            };
            if !output
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                return None;
            }
            let time = modified(&self.donor_target.join(output))?;
            if output_time.is_some_and(|known| known != time) {
                return None;
            }
            output_time = Some(time);
            for path in paths {
                if Instant::now() >= self.deadline {
                    return None;
                }
                inputs.extend(self.run_input(&theirs, &ours, &path, time)?);
            }
        }
        Some(Verified {
            stamp: check.checked_from,
            run: Some(RunInputs {
                output_time: output_time.unwrap_or(SystemTime::UNIX_EPOCH),
                exact: false,
                inputs,
            }),
        })
    }

    /// Whether the compiler read the donor's checkout from the environment,
    /// as the unit's compiler dep-info files `dep_infos` (`deps/*.d`, with
    /// `# env-dep:NAME=VALUE` lines) record it, or one cannot be read. Cargo
    /// leaves the variables it sets (`CARGO_MANIFEST_DIR`, a build script's
    /// `rustc-env`) out of its own dep-info and does not compare them, so a
    /// copy would keep the donor's `env!("CARGO_MANIFEST_DIR")`; kache's key
    /// keeps that path, and such a crate builds in each checkout.
    pub(super) fn reads_checkout_from_env(&self, dep_infos: &[PathBuf]) -> bool {
        dep_infos.is_empty()
            || dep_infos.iter().any(|file| match std::fs::read(file) {
                Ok(bytes) => String::from_utf8_lossy(&bytes)
                    .lines()
                    .filter_map(|line| line.strip_prefix("# env-dep:"))
                    .any(|dep| {
                        dep.split_once('=')
                            .is_some_and(|(_, value)| self.names_checkout(value))
                    }),
                Err(_) => true,
            })
    }

    /// Whether `value` names the donor's checkout outside its target. An
    /// `OUT_DIR` in the target does not count: a seed into a target of the
    /// same checkout keeps it too, and kache's key does not hold it.
    fn names_checkout(&self, value: &str) -> bool {
        let value = match self.donor_target.to_str() {
            Some(target) => value.replace(target, ""),
            None => value.to_owned(),
        };
        super::donor_needles(&super::Donor {
            target_dir: PathBuf::new(),
            workspace_root: self.donor_root.clone(),
            rustc: None,
        })
        .iter()
        .any(|needle| super::contains_path(value.as_bytes(), needle))
    }

    /// Whether the run's recorded output `record` names the donor's checkout
    /// only where its copy can name this one instead
    /// ([`super::relocation::rebase_checkout`]): at the start of the path of
    /// a `rerun-if-changed` (whose file [`Self::run_input`] then compares,
    /// as Cargo's fingerprint names it from the package), or of a
    /// `rustc-link-search` directory that holds the same files with the same
    /// bytes here. The recorded `OUT_DIR`, which Cargo rewrites itself, does
    /// not count; another spelling of the checkout (a Windows path with `/`)
    /// keeps the run out. A run without a record names nothing.
    fn record_rebases(&self, record: &Path) -> bool {
        let text = match std::fs::read(record) {
            Ok(bytes) => bytes,
            Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
        };
        let Ok(text) = String::from_utf8(text) else {
            return false;
        };
        let out_dir =
            std::fs::read_to_string(record.with_file_name("root-output")).unwrap_or_default();
        let text = match out_dir.trim() {
            "" => text,
            out_dir => text.replace(out_dir, ""),
        };
        let Some(root) = self.donor_root.to_str() else {
            return false;
        };
        let needles = super::donor_needles(&super::Donor {
            target_dir: PathBuf::new(),
            workspace_root: self.donor_root.clone(),
            rustc: None,
        });
        let names_root = |text: &str| {
            needles
                .iter()
                .any(|needle| super::contains_path(text.as_bytes(), needle))
        };
        for line in text.lines() {
            if Instant::now() >= self.deadline {
                return false;
            }
            if !names_root(line) {
                continue;
            }
            let Some(span) = super::relocation::checkout_directive(line) else {
                return false;
            };
            let (before, path) = (&line[..span.start], &line[span.clone()]);
            let Some(rest) = super::relocation::below_root(path, root) else {
                return false;
            };
            if names_root(before) || names_root(rest) {
                return false;
            }
            if before.contains("rustc-link-search")
                && !self.same_link_dir(Path::new(rest.trim_start_matches(['/', '\\'])))
            {
                return false;
            }
        }
        true
    }

    /// Whether the directory `rest` below both checkouts (as a
    /// `rustc-link-search` names it) holds the same files with the same
    /// bytes in each: the files a linker may take from it. Its
    /// subdirectories are not searched.
    fn same_link_dir(&self, rest: &Path) -> bool {
        let (Some(theirs), Some(ours)) = (
            normal(&self.donor_root.join(rest)),
            normal(&self.our_root.join(rest)),
        ) else {
            return false;
        };
        if !theirs.starts_with(&self.donor_root)
            || !ours.starts_with(&self.our_root)
            || theirs.starts_with(&self.donor_target)
        {
            return false;
        }
        let (Some(names), Some(our_names)) = (sorted_entries(&theirs), sorted_entries(&ours))
        else {
            return false;
        };
        if names != our_names {
            return false;
        }
        names.iter().all(|name| {
            let (their_file, our_file) = (theirs.join(name), ours.join(name));
            match (
                std::fs::symlink_metadata(&their_file),
                std::fs::symlink_metadata(&our_file),
            ) {
                (Ok(their_meta), Ok(our_meta)) if their_meta.is_dir() && our_meta.is_dir() => true,
                (Ok(their_meta), Ok(our_meta)) => {
                    their_meta.is_file()
                        && our_meta.is_file()
                        && their_meta.len() == our_meta.len()
                        && same_bytes(&their_file, &our_file, self.deadline)
                }
                _ => false,
            }
        })
    }

    /// `None` when the declared file `path` keeps the run out; else the file
    /// whose time this checkout takes, if any. A file outside both checkouts
    /// is the same file for both, and holds while no newer than the run.
    fn run_input(
        &self,
        theirs: &Path,
        ours: &Path,
        path: &Path,
        output_time: SystemTime,
    ) -> Option<Vec<Adopted>> {
        if path.is_absolute() {
            // Cargo keeps a checkout's file in full only when it is outside
            // the package, and the copied output names the donor's.
            if path.starts_with(&self.donor_root) || path.starts_with(&self.our_root) {
                return None;
            }
            return (modified(path)? <= output_time).then(Vec::new);
        }
        let (their_file, our_file) = (normal(&theirs.join(path))?, normal(&ours.join(path))?);
        if !their_file.starts_with(&self.donor_root)
            || !our_file.starts_with(&self.our_root)
            || their_file.starts_with(&self.donor_target)
        {
            return None;
        }
        // A directory: Cargo takes the newest time of everything under it.
        let bytes = if self.in_compared_package(&their_file) {
            Bytes::Compared
        } else {
            Bytes::Read
        };
        tree_inputs(
            &their_file,
            &our_file,
            Some(output_time),
            bytes,
            self.deadline,
        )
    }

    /// Whether the donor's `file` is inside a path package whose directory
    /// [`Self::package`] found the same bytes in both checkouts (a run that
    /// declares the sources of the crates it scans: Prism's app).
    fn in_compared_package(&self, file: &Path) -> bool {
        self.roots
            .iter()
            .filter(|(_, relative)| file.starts_with(self.donor_root.join(relative)))
            .max_by_key(|(_, relative)| relative.components().count())
            .is_some_and(|(package, relative)| self.package(package, relative).is_some())
    }

    /// Ready the copied unit at `fingerprint` for Cargo: a compiled unit's
    /// dep-info dated and rebased ([`date_and_rebase`]); a run's declared
    /// files given the donor's times where theirs are newer than the run.
    /// On an error the unit's fingerprint is gone and Cargo builds it.
    pub(super) fn finish(&self, fingerprint: &Path, verified: &Verified) -> std::io::Result<()> {
        let Some(run) = &verified.run else {
            return self.date(fingerprint, verified.stamp);
        };
        for input in &run.inputs {
            let current = modified(&input.ours).ok_or_else(|| std::io::Error::other("gone"))?;
            if current == input.time || (!run.exact && current <= run.output_time) {
                continue;
            }
            let adopted = filetime::set_file_mtime(
                &input.ours,
                filetime::FileTime::from_system_time(input.time),
            );
            // A write that ended before the new time is caught here; one
            // after it leaves a newer time, which Cargo sees.
            if adopted.is_err() || !input.still_same(self.deadline) {
                let _ = filetime::set_file_mtime(&input.ours, filetime::FileTime::now());
                for file in super::entries(fingerprint) {
                    let _ = std::fs::remove_file(file);
                }
                return Err(std::io::Error::other(format!(
                    "{} changed while seeding",
                    input.ours.display()
                )));
            }
        }
        Ok(())
    }

    /// Date and rebase the copied unit's dep-infos at `fingerprint`
    /// ([`date_and_rebase`]).
    pub(super) fn date(&self, fingerprint: &Path, stamp: SystemTime) -> std::io::Result<()> {
        let below_donor = |path: &Path| match self.place(path)? {
            Place::Donor(within) => Some(within),
            Place::Ours | Place::Outside => None,
        };
        date_and_rebase(fingerprint, below_donor, &self.our_root, stamp)
    }

    /// Where the absolute `path` lies. A path spelled otherwise than either
    /// checkout's root is placed by its canonical path, as are the roots:
    /// Clippy lists `clippy.toml` canonicalized, `\\?\C:\w\clippy.toml` on
    /// Windows and the real path through a symbolic link on Unix, and a
    /// letter's case or an 8.3 name spells a Windows directory too. Taken
    /// for a file outside both checkouts, the donor's would be judged by its
    /// age alone, and a copy kept another checkout's Clippy settings. `None`
    /// when such a path cannot be resolved (it is gone).
    fn place(&self, path: &Path) -> Option<Place> {
        if let Ok(within) = path.strip_prefix(&self.donor_root) {
            return Some(Place::Donor(within.to_path_buf()));
        }
        if path.starts_with(&self.our_root) {
            return Some(Place::Ours);
        }
        let canonical = std::fs::canonicalize(path).ok()?;
        let [donor, ours] = self.canonical_roots.get_or_init(|| {
            [&self.donor_root, &self.our_root].map(|root| std::fs::canonicalize(root).ok())
        });
        if let Some(within) = donor
            .as_deref()
            .and_then(|donor| canonical.strip_prefix(donor).ok())
        {
            return Some(Place::Donor(within.to_path_buf()));
        }
        if ours
            .as_deref()
            .is_some_and(|ours| canonical.starts_with(ours))
        {
            return Some(Place::Ours);
        }
        Some(Place::Outside)
    }

    /// Whether a file a dep-info lists, `path` from the package root, is
    /// inside the package (which [`Self::package`] compared whole), the same
    /// file in both checkouts, or the same bytes, no newer than `reference`.
    fn outside_file_holds(
        &self,
        theirs: &Path,
        ours: &Path,
        path: &Path,
        reference: SystemTime,
    ) -> bool {
        if path.is_absolute() {
            let within = match self.place(path) {
                Some(Place::Donor(within)) => within,
                Some(Place::Ours) | None => return false,
                // The same file for both checkouts.
                Some(Place::Outside) => {
                    return modified(path).is_some_and(|time| time <= reference);
                }
            };
            // `src/../../assets/x` (an `include_bytes!` that climbs out of
            // its package) stays below the checkout; each side's path is
            // compared as the compiler opened it there.
            let our_file = self.our_root.join(&within);
            return normal(&within)
                .is_some_and(|below| !self.donor_root.join(below).starts_with(&self.donor_target))
                && modified(path).is_some_and(|time| time <= reference)
                && same_bytes(path, &our_file, self.deadline);
        }
        let (Some(their_file), Some(our_file)) =
            (normal(&theirs.join(path)), normal(&ours.join(path)))
        else {
            return false;
        };
        if their_file.starts_with(theirs) && our_file.starts_with(ours) {
            return true;
        }
        their_file.starts_with(&self.donor_root)
            && our_file.starts_with(&self.our_root)
            && modified(&their_file).is_some_and(|time| time <= reference)
            && same_bytes(&their_file, &our_file, self.deadline)
    }

    /// The comparison of `package`'s directory, made once.
    fn package(&self, package: &str, relative: &Path) -> Option<PackageCheck> {
        let comparison = Arc::clone(
            self.packages
                .lock()
                .ok()?
                .entry(package.to_owned())
                .or_default(),
        );
        *comparison.get_or_init(|| {
            // A write just after this may carry an mtime a clock tick before
            // `now`; dated from the floor, the copy is older than it.
            let checked_from = crate::build_script::write_floor(SystemTime::now());
            compare_trees(
                &self.donor_root.join(relative),
                &self.our_root.join(relative),
                self.deadline,
            )
            .map(|donor_newest| PackageCheck {
                checked_from,
                donor_newest,
            })
        })
    }
}

/// `path` with `.` and `..` resolved by name alone, or `None` when it climbs
/// above its root.
fn normal(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

/// What a run's fingerprint record checks, or `None` for a run Cargo judges
/// otherwise (by the package's newest file) or a record this cannot read.
fn run_checks(fingerprint: &Path) -> Option<Vec<RunCheck>> {
    let records: Vec<PathBuf> = super::entries(fingerprint)
        .into_iter()
        .filter(|file| {
            let name = super::file_name(file);
            name.starts_with("run-build-script-") && name.ends_with(".json")
        })
        .collect();
    let [record] = records.as_slice() else {
        return None;
    };
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(record).ok()?).ok()?;
    let mut checks = Vec::new();
    for local in json.get("local")?.as_array()? {
        let local = local.as_object()?;
        if local.len() != 1 {
            return None;
        }
        if let Some(files) = local.get("RerunIfChanged") {
            let paths = files
                .get("paths")?
                .as_array()?
                .iter()
                .map(|path| path.as_str().map(PathBuf::from))
                .collect::<Option<Vec<_>>>()?;
            checks.push(RunCheck::Files {
                output: PathBuf::from(files.get("output")?.as_str()?),
                paths,
            });
        } else if local.contains_key("RerunIfEnvChanged") {
            checks.push(RunCheck::Env);
        } else if local
            .get("Precalculated")
            .is_some_and(serde_json::Value::is_string)
        {
            checks.push(RunCheck::Package);
        } else {
            return None;
        }
    }
    Some(checks)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The directory of each of `packages` under `root`, relative to it, from
/// the `[package]` names of the manifests there. A name two manifests use
/// is left out. `None` past [`MAX_MANIFEST_WALK`] entries or `deadline`.
fn package_roots(
    root: &Path,
    packages: &BTreeSet<String>,
    deadline: Instant,
) -> Option<HashMap<String, PathBuf>> {
    let mut found: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = dirs.pop() {
        if Instant::now() >= deadline {
            return None;
        }
        let manifest = dir.join("Cargo.toml");
        // Whether the directory holds a manifest, from its listing: asked of
        // every directory of the checkout, it was 1,670 failed calls for a
        // 600-crate workspace. A link is followed, as before.
        let mut holds_manifest = None;
        if let Ok(listed) = std::fs::read_dir(&dir) {
            holds_manifest = Some(false);
            for entry in listed.flatten() {
                seen += 1;
                if seen > MAX_MANIFEST_WALK {
                    return None;
                }
                let name = entry.file_name();
                let kind = entry.file_type().ok();
                if name == OsStr::new("Cargo.toml") {
                    holds_manifest = Some(kind.is_some_and(|kind| {
                        kind.is_file() || (kind.is_symlink() && manifest.is_file())
                    }));
                }
                if SKIPPED_DIRS.iter().any(|skip| name == OsStr::new(skip)) {
                    continue;
                }
                if kind.is_some_and(|kind| kind.is_dir()) {
                    dirs.push(entry.path());
                }
            }
        }
        // A directory that cannot be listed may still be searched.
        if holds_manifest.unwrap_or_else(|| manifest.is_file())
            && let Some(name) = package_name(&manifest)
            && packages.contains(&name)
        {
            let relative = dir.strip_prefix(root).ok()?.to_path_buf();
            found
                .entry(name)
                .and_modify(|known| *known = None)
                .or_insert(Some(relative));
        }
    }
    Some(
        found
            .into_iter()
            .filter_map(|(name, path)| Some((name, path?)))
            .collect(),
    )
}

/// The `[package] name` of a manifest.
fn package_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    Some(table.get("package")?.get("name")?.as_str()?.to_owned())
}

/// Whether the trees at `theirs` and `ours` hold the same entries, links
/// with the same targets and files with the same bytes; then the newest
/// modification time under `theirs`. `None` on any difference, an
/// unreadable entry or `deadline`.
fn compare_trees(theirs: &Path, ours: &Path, deadline: Instant) -> Option<SystemTime> {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut pending = vec![(theirs.to_path_buf(), ours.to_path_buf())];
    while let Some((their_dir, our_dir)) = pending.pop() {
        if Instant::now() >= deadline {
            return None;
        }
        let their_entries = entries_with_status(&their_dir)?;
        let our_entries = entries_with_status(&our_dir)?;
        if their_entries.len() != our_entries.len()
            || their_entries
                .iter()
                .zip(&our_entries)
                .any(|(theirs, ours)| theirs.0 != ours.0)
        {
            return None;
        }
        for ((name, their_meta), (_, our_meta)) in their_entries.into_iter().zip(our_entries) {
            let (their_path, our_path) = (their_dir.join(&name), our_dir.join(&name));
            newest = newest.max(their_meta.modified().ok()?);
            let (their_kind, our_kind) = (their_meta.file_type(), our_meta.file_type());
            if their_kind.is_symlink() || our_kind.is_symlink() {
                if !(their_kind.is_symlink() && our_kind.is_symlink())
                    || std::fs::read_link(&their_path).ok()?
                        != std::fs::read_link(&our_path).ok()?
                {
                    return None;
                }
            } else if their_kind.is_dir() && our_kind.is_dir() {
                pending.push((their_path, our_path));
            } else if their_kind.is_file() && our_kind.is_file() {
                if their_meta.len() != our_meta.len()
                    || !same_bytes_of(&their_path, &our_path, their_meta.len(), deadline)
                {
                    return None;
                }
            } else {
                return None;
            }
        }
    }
    Some(newest)
}

/// The names in `dir`, sorted, each with its status, not following a link,
/// asked of `dir`'s open descriptor by the name: a stat of each entry's full
/// path walked the checkout's directories above it again, two calls for
/// each of the 17,000 files a 600-crate workspace's packages hold.
fn entries_with_status(dir: &Path) -> Option<Vec<(std::ffi::OsString, std::fs::Metadata)>> {
    let mut entries = std::fs::read_dir(dir)
        .ok()?
        .map(|entry| entry.and_then(|entry| Ok((entry.file_name(), entry.metadata()?))))
        .collect::<std::io::Result<Vec<_>>>()
        .ok()?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Some(entries)
}

fn sorted_entries(dir: &Path) -> Option<Vec<std::ffi::OsString>> {
    let mut names = std::fs::read_dir(dir)
        .ok()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .ok()?;
    names.sort();
    Some(names)
}

/// [`same_bytes`] of two files whose stats both say `len` bytes. One under
/// [`COMPARE_CHUNK`], as nearly every source file a checkout holds, is read
/// in one call a side: a read of one byte more than `len` finds the end in
/// the same call, where filling a whole chunk took a second read to find it,
/// and only `len` bytes are set aside for it. Either side read short or long
/// is not the file its stat said, and the two are not taken for the same.
fn same_bytes_of(a: &Path, b: &Path, len: u64, deadline: Instant) -> bool {
    let Some(len) = usize::try_from(len).ok().filter(|len| *len < COMPARE_CHUNK) else {
        return same_bytes(a, b, deadline);
    };
    if Instant::now() >= deadline {
        return false;
    }
    let (Ok(mut a), Ok(mut b)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let mut left = vec![0u8; len + 1];
    let mut right = vec![0u8; len + 1];
    matches!(
        (a.read(&mut left), b.read(&mut right)),
        (Ok(read), Ok(other)) if read == len && other == len
    ) && left == right
}

/// Whether two files hold the same bytes, read side by side.
fn same_bytes(a: &Path, b: &Path, deadline: Instant) -> bool {
    let (Ok(mut a), Ok(mut b)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let mut left = vec![0u8; COMPARE_CHUNK];
    let mut right = vec![0u8; COMPARE_CHUNK];
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        let Ok(read) = fill(&mut a, &mut left) else {
            return false;
        };
        let Ok(other) = fill(&mut b, &mut right) else {
            return false;
        };
        if read != other || left[..read] != right[..other] {
            return false;
        }
        if read < COMPARE_CHUNK {
            return true;
        }
    }
}

/// Read until `buffer` is full or the file ends.
fn fill(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Where a dep-info path is rooted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// The package's directory (or an absolute path).
    PackageRoot,
    /// The target directory: a build script's generated files.
    TargetRoot,
}

/// Cargo's own encoding of a unit's dep-info (`.fingerprint/*/dep-*`), as
/// Cargo 1.83 and later write it: a version header, the files with their
/// root and an optional checksum, then the environment variables read. Any
/// other shape is refused, which only leaves the unit to be built.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EncodedDepInfo {
    files: Vec<DepFile>,
    env: Vec<(String, Option<String>)>,
}

/// A file of an [`EncodedDepInfo`]. Cargo writes a path outside both the
/// package and the target in full, as [`PathKind::TargetRoot`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct DepFile {
    kind: PathKind,
    path: String,
    /// The file's length, checksum algorithm and checksum, when recorded.
    checksum: Option<(u64, u8, Vec<u8>)>,
}

/// The header Cargo writes before an encoded dep-info: a `u32` 1, then
/// 0xff and the format version 1.
const ENCODED_HEADER: [u8; 6] = [1, 0, 0, 0, 0xff, 1];

impl EncodedDepInfo {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader(bytes.strip_prefix(&ENCODED_HEADER)?);
        let mut info = Self::default();
        for _ in 0..reader.u32()? {
            let kind = match reader.u8()? {
                0 => PathKind::PackageRoot,
                1 => PathKind::TargetRoot,
                _ => return None,
            };
            let path = reader.string()?;
            let checksum = match reader.u8()? {
                0 => None,
                1 => Some((reader.u64()?, reader.u8()?, reader.bytes()?.to_vec())),
                _ => return None,
            };
            info.files.push(DepFile {
                kind,
                path,
                checksum,
            });
        }
        for _ in 0..reader.u32()? {
            let key = reader.string()?;
            let value = match reader.u8()? {
                0 => None,
                1 => Some(reader.string()?),
                _ => return None,
            };
            info.env.push((key, value));
        }
        reader.0.is_empty().then_some(info)
    }

    /// The bytes [`Self::parse`] reads back.
    fn encode(&self) -> Option<Vec<u8>> {
        let mut out = ENCODED_HEADER.to_vec();
        let len = |n: usize| u32::try_from(n).ok().map(u32::to_le_bytes);
        out.extend(len(self.files.len())?);
        for file in &self.files {
            out.push(match file.kind {
                PathKind::PackageRoot => 0,
                PathKind::TargetRoot => 1,
            });
            out.extend(len(file.path.len())?);
            out.extend(file.path.as_bytes());
            match &file.checksum {
                None => out.push(0),
                Some((length, algorithm, checksum)) => {
                    out.push(1);
                    out.extend(length.to_le_bytes());
                    out.push(*algorithm);
                    out.extend(len(checksum.len())?);
                    out.extend(checksum);
                }
            }
        }
        out.extend(len(self.env.len())?);
        for (key, value) in &self.env {
            out.extend(len(key.len())?);
            out.extend(key.as_bytes());
            match value {
                None => out.push(0),
                Some(value) => {
                    out.push(1);
                    out.extend(len(value.len())?);
                    out.extend(value.as_bytes());
                }
            }
        }
        Some(out)
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn string(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
}

/// Make each dep-info of the copied unit at `fingerprint` name this
/// checkout's files where it named the donor's in full (`below_donor` gives
/// the part of such a path below the donor's checkout, in any spelling
/// [`CrossCheckout::place`] knows), and give it the
/// time `stamp`. A dep-info that cannot be read or written is removed: the
/// unit is then built, as without seeding.
pub(super) fn date_and_rebase(
    fingerprint: &Path,
    below_donor: impl Fn(&Path) -> Option<PathBuf>,
    our_root: &Path,
    stamp: SystemTime,
) -> std::io::Result<()> {
    let time = filetime::FileTime::from_system_time(stamp);
    for file in super::entries(fingerprint) {
        if !super::file_name(&file).starts_with("dep-") {
            continue;
        }
        let rebased = std::fs::read(&file).ok().and_then(|bytes| {
            let mut info = EncodedDepInfo::parse(&bytes)?;
            for entry in &mut info.files {
                let path = Path::new(&entry.path);
                if !path.is_absolute() {
                    continue;
                }
                if let Some(within) = below_donor(path) {
                    entry.path = our_root.join(within).to_str()?.to_owned();
                }
            }
            info.encode()
        });
        let written = match rebased {
            Some(bytes) => {
                std::fs::write(&file, bytes).and_then(|()| filetime::set_file_mtime(&file, time))
            }
            None => Err(std::io::Error::other("unreadable dep-info")),
        };
        if let Err(error) = written {
            let _ = std::fs::remove_file(&file);
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
