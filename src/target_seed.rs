//! Registry build units copied into a new checkout's target directory before
//! Cargo decides what to build.
//!
//! The first thing Cargo sends a new target directory's `RUSTC_WRAPPER` is
//! its target-info probe (`rustc - --crate-name ___ --print=file-names ...`),
//! before it takes the build lock and before it checks any unit's freshness.
//! Cargo caches the answer in `<target>/.rustc_info.json`, so a target
//! directory without that file is one Cargo has not built yet. The wrapper
//! seeds it before it answers the probe, for up to [`SEED_DEADLINE`], and
//! longer while the copy keeps going ([`SeedDeadline`]): it copies the units
//! another checkout already built for the same registry and git packages,
//! and Cargo then finds them fresh and skips them. The wrapper
//! does this itself rather than through the daemon, so a build that starts
//! while no daemon runs, or while the daemon is starting or busy, is seeded
//! all the same.
//!
//! A unit's directory name carries Cargo's hash of everything that decides it
//! (package, version, features, profile, dependencies, compiler). A registry
//! or git package's hash does not depend on where the checkout is, so a unit
//! with the same hash in another target directory is the same unit. The copy
//! keeps what Cargo's freshness check relies on:
//!
//! - Packages with a `source` in `Cargo.lock` are copied from any checkout.
//!   A path package's fingerprint trusts its sources' modification times,
//!   so a copy from a checkout with different sources could pass as fresh:
//!   its units are copied only from a donor target of the same checkout
//!   ([`same_checkout`]), where Cargo compares the very files the donor's
//!   units were built from, as it would in the donor target itself.
//!   Integrate builds a new target per check in the checkout the work was
//!   done in. From another checkout's target, a path unit is copied only
//!   when its package's files are the same bytes in both checkouts, and
//!   its copied dep-info is dated to when that was checked
//!   ([`workspace`]). `KACHE_SEED_WORKSPACE_UNITS=same` keeps them to the
//!   same checkout, `0` leaves them out.
//! - Modification times are kept: Cargo rebuilds a unit whose dependencies'
//!   outputs are newer than its own.
//! - Files are cloned where the filesystem can. Otherwise a file nobody may
//!   write (a store blob the donor's restore linked) is hard-linked, and any
//!   other file is copied (see [`shares_inode`]). Each unit's
//!   fingerprint is put in place last, so an interrupted copy leaves a unit
//!   Cargo rebuilds rather than one it trusts.
//! - Compiler dep-info paths and paired build-script stdout/root-output are
//!   rebased before publication. Recorded environment dependencies keep
//!   their observed values, so Cargo can still detect an environment change.
//! - A build script's `OUT_DIR` that a hermetic run left as a link to its
//!   sealed, shared run in this kache's cache is linked to the same run, and
//!   recorded as linking to it so the run is kept while the new target uses
//!   it. Any other symbolic link keeps the unit out.
//! - A build script whose recorded output names the other checkout, other
//!   than its own `OUT_DIR` (which Cargo rewrites), is not copied. Files
//!   under that script's `out/` are copied unchanged, so one that names
//!   the other checkout keeps the unit out too, and so does an `out/` that
//!   is not read in full within [`OUT_SCAN_BYTES`] and the deadline. A
//!   checkout path with `\` separators also counts when written with `/`,
//!   as CMake writes it on Windows, or with each `\` doubled, and its drive
//!   letter matches in either case.
//! - The other checkout must have been built by the same `rustc`: the one
//!   kache recorded while building into it, else the one Cargo's rustc info
//!   cache names (Cargo does not write that cache when it cannot fingerprint
//!   `rustc`). No build may hold either profile's lock while units are
//!   copied.
//!
//! Both of Cargo's layouts are handled (see [`crate::cargo_layout`]). Only
//! the host `debug` profile is seeded: the probe does not say which profile
//! the build uses, and `debug` is the one `build`, `check`, `test` and
//! `clippy` share.

mod placement;
mod relocation;
mod workspace;

use crate::invocation::process::CommandExt as _;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::build_script::HeldRun;
/// No hermetic run is linked to off Unix.
#[cfg(not(unix))]
type HeldRun = std::convert::Infallible;

/// How long seeding copies for in any case. Past it, the copy goes on while
/// units keep coming ([`SeedDeadline`]), up to [`SEED_LIMIT`]. Copying stops
/// between files, and Cargo builds whatever was not copied.
///
/// A unit left out costs far more later than copying it now: the wrapper
/// restores it from the store in a compiler call of its own (about 50 ms of
/// CPU each in a 1,300-unit workspace, against about 1 ms to copy). On a
/// loaded machine 3 s cut seedings of 1,400-1,500 units to 78-166.
pub(crate) const SEED_DEADLINE: Duration = Duration::from_secs(10);

/// How long a seeding that keeps copying may go on past [`SEED_DEADLINE`].
///
/// Under load a copy slows down far less than what it saves. A workspace
/// clippy in a new checkout of a 600-crate workspace (2,188 units) seeded
/// for 10 s at most was cut at load 62-198 with 817-1,809 units copied, and
/// restoring the rest one compiler call at a time took it to 75-250 s.
/// Going on past 10 s, eleven seedings at load 118-272 copied every unit
/// in 10.7-21.3 s, and the clippy took 18-37 s.
pub(crate) const SEED_LIMIT: Duration = Duration::from_secs(60);

/// Past [`SEED_DEADLINE`], the copy stops once no unit has been copied for
/// this long: it no longer gives Cargo anything for its wait.
const SEED_STALL: Duration = Duration::from_secs(2);

/// Units copied at once: half the machine's threads, at least four and at
/// most eight ([`copy_threads`]). A copy is mostly metadata calls, which
/// overlap: on btrfs, 1159 units took 2.9 s on one thread, 0.6 s on four
/// and 0.5 s on eight.
const COPY_THREADS: std::ops::RangeInclusive<usize> = 4..=8;

/// How many units a seeding copies at once ([`COPY_THREADS`]).
///
/// While every thread staged its units beside the others' in the target's
/// shared directories, more threads only waited longer for those
/// directories' locks. Staged in directories of their own ([`Staging`]),
/// eight copied 2,188 units in 5.7-11.5 s at load 156-193 where four took
/// 7.6-9.0 s (medians 5.9 and 7.6 s); twelve and sixteen were no faster
/// and burned more of the kernel's time. A machine of fewer than sixteen
/// threads keeps four.
fn copy_threads() -> usize {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    (threads / 2).clamp(*COPY_THREADS.start(), *COPY_THREADS.end())
}

/// The profile directory seeded.
const PROFILE: &str = "debug";

/// Where a unit is copied before it is renamed into place.
const STAGING: &str = ".kache-seeding-";

/// Units a copy thread copies before it places them ([`place_staged`]).
///
/// Few enough that a seeding cut short by its deadline still places what it
/// copied in a moment, and that a thread's directory of staged names stays
/// small; enough that a thread waits for each shared directory once for
/// many names. Research bench (one production seeding of command-center's
/// workspace clippy into a new checkout), at load 9-11, nine interleaved
/// rounds: placing every 8 units, 32 or once a thread's units were all
/// copied took 1,020, 1,156 and 1,252 ms (medians, within noise).
const PLACE_BATCH: usize = 16;

/// Bytes of a build script's `out/` read per unit while looking for a donor
/// path. A unit whose files add up to more is not seeded: the unread bytes
/// could name the donor. aws-lc-sys 0.45's `out/`, objects with debug info
/// and their archive, is 30.5 MiB in an x86_64 Linux debug build with clang.
const OUT_SCAN_BYTES: u64 = 256 << 20;

/// Bytes of an `out/` file read at a time. The deadline is checked before
/// each read, and only this much of a file is held.
const OUT_SCAN_CHUNK: usize = 1 << 20;

/// Is `args` (the compiler and its arguments) Cargo's target-info probe?
pub(crate) fn is_target_info_probe(args: &[String]) -> bool {
    let crate_name = args
        .windows(2)
        .any(|pair| pair[0] == "--crate-name" && pair[1] == "___");
    let file_names = args.iter().any(|arg| arg == "--print=file-names")
        || args
            .windows(2)
            .any(|pair| pair[0] == "--print" && pair[1] == "file-names");
    crate_name && file_names
}

/// A target directory Cargo is about to build for the first time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewTarget {
    pub(crate) target_dir: PathBuf,
    pub(crate) workspace_root: PathBuf,
}

/// The target directory a probe in `cwd` is for, when it is one this can
/// seed: the workspace is the nearest directory up from `cwd` holding
/// `Cargo.toml` and `Cargo.lock`, and the target directory is
/// `target_env` (the resolved intermediate directory) or `<workspace>/target`. `None` when
/// `elsewhere` says configuration moves the target or build directory, and
/// when Cargo has already built there.
pub(crate) fn new_target(
    cwd: &Path,
    target_env: Option<&OsStr>,
    elsewhere: bool,
) -> Option<NewTarget> {
    if elsewhere {
        return None;
    }
    let workspace_root = workspace_root(cwd)?.to_path_buf();
    let target_dir = match target_env.filter(|value| !value.is_empty()) {
        Some(value) => cwd.join(value),
        None => workspace_root.join("target"),
    };
    (!built(&target_dir)).then_some(NewTarget {
        target_dir,
        workspace_root,
    })
}

/// Resolve Cargo's intermediate directory for diagnostics as well as seeding.
/// The shared resolver keeps target and build overrides independent.
pub(crate) fn configured_output_dir(
    cwd: &Path,
    workspace: &Path,
    cargo_home: &Path,
    target_env: Option<&std::ffi::OsStr>,
    build_env: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    placement::resolve(cwd, workspace, cargo_home, target_env, build_env)
}

fn workspace_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file() && dir.join("Cargo.toml").is_file())
}

/// Whether Cargo has built in `target_dir`: it cached its compiler probe
/// there or wrote any unit to the seeded profile.
fn built(target_dir: &Path) -> bool {
    [
        target_dir.join(".rustc_info.json"),
        target_dir.join(PROFILE).join(".fingerprint"),
        target_dir.join(PROFILE).join("build"),
    ]
    .iter()
    .any(|path| path.exists())
}

/// The registry and git packages in a lockfile: those with a `source`, less
/// any name a path package also uses.
pub(crate) fn registry_packages(lockfile: &str) -> BTreeSet<String> {
    let Ok(lock) = lockfile.parse::<toml::Table>() else {
        return BTreeSet::new();
    };
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut registry = BTreeSet::new();
    let mut path = BTreeSet::new();
    for package in packages {
        let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        if package.get("source").is_some() {
            registry.insert(name.to_owned());
        } else {
            path.insert(name.to_owned());
        }
    }
    &registry - &path
}

/// The path packages in a lockfile: those without a `source`, less any name
/// a registry or git package also uses (a unit's directory names only the
/// package, so the two could not be told apart).
pub(crate) fn path_packages(lockfile: &str) -> BTreeSet<String> {
    let Ok(lock) = lockfile.parse::<toml::Table>() else {
        return BTreeSet::new();
    };
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut registry = BTreeSet::new();
    let mut path = BTreeSet::new();
    for package in packages {
        let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        if package.get("source").is_some() {
            registry.insert(name.to_owned());
        } else {
            path.insert(name.to_owned());
        }
    }
    &path - &registry
}

/// Which donors give workspace (path) units, from
/// `KACHE_SEED_WORKSPACE_UNITS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceUnits {
    /// `0`, `false`, `no` or `off`: none.
    Off,
    /// `same`: targets of the same checkout only.
    SameCheckout,
    /// Anything else, or unset: also another checkout's, checked by content.
    Checked,
}

fn workspace_units_wanted(value: Option<&OsStr>) -> WorkspaceUnits {
    match value.and_then(OsStr::to_str).map(str::trim) {
        Some(value)
            if ["0", "false", "no", "off"]
                .iter()
                .any(|off| value.eq_ignore_ascii_case(off)) =>
        {
            WorkspaceUnits::Off
        }
        Some(value) if value.eq_ignore_ascii_case("same") => WorkspaceUnits::SameCheckout,
        _ => WorkspaceUnits::Checked,
    }
}

/// The directory holding a unit's fingerprint and dep-info.
fn fingerprint_dir(profile: &Path, layout: Layout, unit: &Unit) -> PathBuf {
    match layout {
        Layout::PerUnit => marker(profile, layout, unit).join("fingerprint"),
        Layout::Shared => marker(profile, layout, unit),
    }
}

/// Whether `donor` was built from the checkout `target` is for. Cargo then
/// judges a copied path unit against the files the donor built it from, by
/// the same modification times it would compare in the donor target, so a
/// copy is fresh exactly when it would be fresh there.
fn same_checkout(donor: &Donor, target: &NewTarget) -> bool {
    let canonical = |path: &Path| std::fs::canonicalize(path).ok();
    donor.workspace_root == target.workspace_root
        || canonical(&donor.workspace_root)
            .is_some_and(|root| Some(root) == canonical(&target.workspace_root))
}

/// The paths that keep a donor's build-script unit out when its recorded
/// output or `out/` names one ([`names_the_donor`]): the donor's target and
/// its checkout. A donor of the same checkout names that checkout's files
/// as this build does, so only its target counts.
fn needles_for(donor: &Donor, same_checkout: bool) -> Vec<String> {
    if same_checkout {
        donor_needles(&Donor {
            workspace_root: PathBuf::new(),
            ..donor.clone()
        })
    } else {
        donor_needles(donor)
    }
}

/// Whether the checkout `target` is for has a target other than it, which a
/// run's files taking older times could make look fresh ([`workspace`]):
/// one kache built into, or a Cargo target directory under the checkout's
/// `target/` (two levels down, as integrate's are).
fn checkout_has_other_targets(target: &NewTarget, donors: &[Donor]) -> bool {
    let ours = |dir: &Path| dir == target.target_dir || dir.starts_with(&target.target_dir);
    if donors.iter().any(|donor| {
        same_checkout(donor, target) && !ours(&donor.target_dir) && donor.target_dir.exists()
    }) {
        return true;
    }
    let root = target.workspace_root.join("target");
    let mut pending = vec![(root, 0)];
    while let Some((dir, depth)) = pending.pop() {
        if ours(&dir) {
            continue;
        }
        if dir.join("CACHEDIR.TAG").exists() || dir.join(PROFILE).join(".fingerprint").exists() {
            return true;
        }
        if depth < 2 {
            pending.extend(subdirectories(&dir).into_iter().map(|sub| (sub, depth + 1)));
        }
    }
    false
}

/// A target directory units may be copied from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Donor {
    pub(crate) target_dir: PathBuf,
    pub(crate) workspace_root: PathBuf,
    /// The `rustc -vV` kache recorded building into it, if any.
    pub(crate) rustc: Option<String>,
}

/// Which of Cargo's layouts a profile directory uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `build/<pkg>/<hash>/`, one directory per unit (Cargo 1.100 and later).
    PerUnit,
    /// `deps/`, `.fingerprint/<pkg>-<hash>/` and `build/<pkg>-<hash>/`.
    Shared,
}

fn layout(profile: &Path) -> Option<Layout> {
    if profile.join(".fingerprint").is_dir() {
        return Some(Layout::Shared);
    }
    subdirectories(&profile.join("build"))
        .iter()
        .flat_map(|package| subdirectories(package))
        .any(|unit| unit.join("fingerprint").is_dir())
        .then_some(Layout::PerUnit)
}

/// One unit of a donor's profile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unit {
    package: String,
    hash: String,
}

/// The donor's units of `packages`, in the order the donor built them
/// ([`built_at`]), so a seeding the deadline cuts short has copied units
/// whose dependencies it copied too: Cargo rebuilds a unit whose dependency
/// it rebuilds, so a unit copied without its dependencies is copied in vain.
fn units(profile: &Path, layout: Layout, packages: &BTreeSet<String>) -> Vec<Unit> {
    in_build_order(profile, layout, units_by_name(profile, layout, packages))
}

/// `units` of `profile` in the order the donor built them ([`units`]).
fn in_build_order(profile: &Path, layout: Layout, mut units: Vec<Unit>) -> Vec<Unit> {
    // Stable: units built at the same time keep their names' order, and a
    // unit without a time goes last.
    units.sort_by_cached_key(|unit| {
        let built = built_at(profile, layout, unit);
        (built.is_none(), built)
    });
    units
}

/// When the donor built `unit`: when Cargo last added a file to the unit's
/// fingerprint directory, which it fills once the unit is built. A
/// dependency is built, and its fingerprint written, before the units that
/// use it start. Not the unit's own directory in the per-unit layout,
/// which also changes when Cargo makes the unit's other directories.
fn built_at(profile: &Path, layout: Layout, unit: &Unit) -> Option<std::time::SystemTime> {
    let fingerprint = match layout {
        Layout::PerUnit => marker(profile, layout, unit).join("fingerprint"),
        Layout::Shared => marker(profile, layout, unit),
    };
    std::fs::symlink_metadata(fingerprint)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// The donor's units of `packages`, by package and hash.
fn units_by_name(profile: &Path, layout: Layout, packages: &BTreeSet<String>) -> Vec<Unit> {
    let mut units = Vec::new();
    match layout {
        Layout::PerUnit => {
            for package in packages {
                for unit in subdirectories(&profile.join("build").join(package)) {
                    let hash = file_name(&unit);
                    if crate::cargo_layout::is_unit_hash(&hash) && unit.join("fingerprint").is_dir()
                    {
                        units.push(Unit {
                            package: package.clone(),
                            hash,
                        });
                    }
                }
            }
        }
        Layout::Shared => {
            for fingerprint in subdirectories(&profile.join(".fingerprint")) {
                let name = file_name(&fingerprint);
                let Some((package, hash)) = name.rsplit_once('-') else {
                    continue;
                };
                if packages.contains(package) && crate::cargo_layout::is_unit_hash(hash) {
                    units.push(Unit {
                        package: package.to_owned(),
                        hash: hash.to_owned(),
                    });
                }
            }
        }
    }
    units
}

/// The unit's directory or fingerprint in a profile: what, once present,
/// makes Cargo consider the unit built.
fn marker(profile: &Path, layout: Layout, unit: &Unit) -> PathBuf {
    match layout {
        Layout::PerUnit => profile.join("build").join(&unit.package).join(&unit.hash),
        Layout::Shared => profile
            .join(".fingerprint")
            .join(format!("{}-{}", unit.package, unit.hash)),
    }
}

/// The compiler's dep-info files of a compiled unit: `deps/<crate>-<hash>.d`,
/// and a build script's own beside it in `build/<pkg>-<hash>/` (or, since
/// Cargo 1.100, in the unit's `out/`). `build_dir`: whether the donor holds
/// the unit's directory in `build/` ([`BuildDirs`]).
fn compiler_dep_infos(
    from: &Path,
    layout: Layout,
    unit: &Unit,
    deps: &HashMap<String, Vec<PathBuf>>,
    build_dir: bool,
) -> Vec<PathBuf> {
    let dep_info = |file: &PathBuf| file.extension() == Some(OsStr::new("d")) && file.is_file();
    match layout {
        Layout::Shared => deps
            .get(&unit.hash)
            .into_iter()
            .flatten()
            .cloned()
            .chain(if build_dir {
                entries(
                    &from
                        .join("build")
                        .join(format!("{}-{}", unit.package, unit.hash)),
                )
            } else {
                Vec::new()
            })
            .filter(dep_info)
            .collect(),
        Layout::PerUnit => entries(&build_script_out_dir(from, layout, unit))
            .into_iter()
            .filter(dep_info)
            .collect(),
    }
}

/// Where a run's output is recorded (`stdout`, beside `root-output`); only a
/// build script's run has one.
fn run_record(profile: &Path, layout: Layout, unit: &Unit) -> PathBuf {
    match layout {
        Layout::PerUnit => profile
            .join("build")
            .join(&unit.package)
            .join(&unit.hash)
            .join("run/stdout"),
        Layout::Shared => profile
            .join("build")
            .join(format!("{}-{}", unit.package, unit.hash))
            .join("output"),
    }
}

/// A path into the donor in the unit's recorded build-script output, other
/// than its own `OUT_DIR`, which Cargo rewrites when it replays the output.
fn names_the_donor(profile: &Path, layout: Layout, unit: &Unit, needles: &[String]) -> bool {
    let stdout = run_record(profile, layout, unit);
    let Ok(output) = std::fs::read(&stdout) else {
        return false;
    };
    let output = String::from_utf8_lossy(&output);
    let out_dir = std::fs::read_to_string(stdout.with_file_name("root-output")).unwrap_or_default();
    let rest = match out_dir.trim() {
        "" => output.into_owned(),
        out_dir => output.replace(out_dir, ""),
    };
    needles
        .iter()
        .any(|needle| contains_path(rest.as_bytes(), needle))
}

/// Whether a file under the build script's `out/` names the donor, or the
/// scan could not read all of them.
///
/// Cargo rewrites `OUT_DIR` only when it replays the script's stdout. A
/// cache written into `out/` (cmake's `CMakeCache.txt` records
/// `OUT_DIR/build`) still contains the other checkout's path, and copying
/// it makes the script fail in the new target. The scan runs only when the
/// script's stdout record exists, so a compiler unit's `out/` is the
/// artifact itself and is still copied. Files adding up to more than
/// [`OUT_SCAN_BYTES`], the deadline passing during the scan, an unreadable
/// file and a directory that cannot be listed each keep the unit out.
fn out_dir_names_the_donor(
    profile: &Path,
    layout: Layout,
    unit: &Unit,
    needles: &[String],
    deadline: Instant,
) -> bool {
    if !run_record(profile, layout, unit).is_file() {
        return false;
    }
    OutScan::new(needles, deadline).keeps_out(&build_script_out_dir(profile, layout, unit))
}

/// The units of a donor's shared-layout profile that have a directory in its
/// `build/`, listed once a seeding, under the donor's lock. Only those hold a
/// build script's binary or a run's records (`output`, `root-output`, a
/// launcher's record, `out/`). Most units are a library's compilation and
/// have none, and looking for their records one by one cost each of them two
/// failed opens and four failed status calls (a 600-crate workspace: 2,188
/// units, 262 of them in `build/`). In the per-unit layout every unit's
/// directory is in `build/`.
struct BuildDirs(Option<std::collections::HashSet<std::ffi::OsString>>);

impl BuildDirs {
    fn of(profile: &Path, layout: Layout) -> Self {
        Self(match layout {
            Layout::PerUnit => None,
            Layout::Shared => Some(
                std::fs::read_dir(profile.join("build"))
                    .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
                    .unwrap_or_default(),
            ),
        })
    }

    /// Whether `unit` has its directory in `build/`.
    fn holds(&self, layout: Layout, unit: &Unit) -> bool {
        match (&self.0, layout) {
            (Some(names), Layout::Shared) => {
                names.contains(OsStr::new(&format!("{}-{}", unit.package, unit.hash)))
            }
            _ => true,
        }
    }
}

/// What a shared-layout profile seeding copies into held when the seeding
/// began: the entries of its `.fingerprint/`, `deps/` and `examples/`. The
/// seeding holds the target's lock and copies each unit once, so a unit's
/// marker or file is there only if it was then; asking for each (a new
/// target holds none) was 6,150 failed status calls for a 600-crate
/// workspace. A file two units share may be placed twice, the second time
/// by rename over the first. In the per-unit layout each path is asked.
struct Present(Option<std::collections::HashSet<PathBuf>>);

impl Present {
    fn of(profile: &Path, layout: Layout) -> Self {
        Self(match layout {
            Layout::PerUnit => None,
            Layout::Shared => Some(
                std::iter::once(".fingerprint")
                    .chain(OUTPUT_DIRS)
                    .flat_map(|dir| std::fs::read_dir(profile.join(dir)).into_iter().flatten())
                    .flatten()
                    .map(|entry| entry.path())
                    .collect(),
            ),
        })
    }

    /// Whether `path` was there when the seeding began.
    fn holds(&self, path: &Path) -> bool {
        match &self.0 {
            Some(present) => present.contains(path),
            None => path.exists(),
        }
    }
}

/// `out/` of a build script: `<unit>/out` since Cargo 1.100, and
/// `build/<pkg>-<hash>/out` before that.
fn build_script_out_dir(profile: &Path, layout: Layout, unit: &Unit) -> PathBuf {
    match layout {
        Layout::PerUnit => profile
            .join("build")
            .join(&unit.package)
            .join(&unit.hash)
            .join("out"),
        Layout::Shared => profile
            .join("build")
            .join(format!("{}-{}", unit.package, unit.hash))
            .join("out"),
    }
}

/// The donor's directories as a file may spell them: as the platform does
/// and, for a path with `\` separators, with `/` instead, as CMake writes
/// `C:/Users/me/ws/target` on Windows, and with each `\` doubled, as JSON,
/// `{:?}` and string literals write it.
fn donor_needles(donor: &Donor) -> Vec<String> {
    let mut needles = Vec::new();
    for path in [&donor.target_dir, &donor.workspace_root] {
        let native = path.to_string_lossy().into_owned();
        if native.is_empty() {
            continue;
        }
        if native.contains('\\') {
            needles.push(native.replace('\\', "/"));
            needles.push(native.replace('\\', r"\\"));
        }
        needles.push(native);
    }
    needles
}

/// Whether `text` holds `path`, with a leading drive letter in either case:
/// Windows tools disagree on it (`c:\ws` in an editor's terminal, `C:/ws`
/// in a CMake cache).
fn contains_path(text: &[u8], path: &str) -> bool {
    match path.as_bytes() {
        [drive, tail @ ..] if drive.is_ascii_alphabetic() && tail.starts_with(b":") => {
            // A Windows path has no other `:`, so matches cannot overlap.
            memchr::memmem::find_iter(text, tail)
                .any(|at| at > 0 && text[at - 1].eq_ignore_ascii_case(drive))
        }
        path => crate::build_script::find_bytes(text, path).is_some(),
    }
}

/// A walk of a build script's `out/` for the donor's paths.
struct OutScan<'a> {
    needles: &'a [String],
    /// What the unit's files may still add up to.
    bytes_left: u64,
    deadline: Instant,
    /// The end of the last read, followed by the next one.
    buffer: Vec<u8>,
    /// Bytes of the last read kept before the next: one fewer than the
    /// longest needle, so a needle split between two reads is found whole.
    overlap: usize,
}

impl<'a> OutScan<'a> {
    fn new(needles: &'a [String], deadline: Instant) -> Self {
        let overlap = needles
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        OutScan {
            needles,
            bytes_left: OUT_SCAN_BYTES,
            deadline,
            buffer: vec![0; overlap + OUT_SCAN_CHUNK],
            overlap,
        }
    }

    /// Whether what is under `dir` keeps the unit out: a file names the
    /// donor, or something was left unread. Symbolic links are not followed.
    fn keeps_out(&mut self, dir: &Path) -> bool {
        let listed = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return dir.exists(),
        };
        for entry in listed {
            let Ok(entry) = entry else {
                return true;
            };
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                return true;
            };
            let keeps_out = if metadata.is_dir() {
                self.keeps_out(&path)
            } else {
                metadata.is_file() && self.file_keeps_out(&path, metadata.len())
            };
            if keeps_out {
                return true;
            }
        }
        false
    }

    /// Whether the file at `path`, `len` bytes long, names the donor or does
    /// not fit in what is left of the budget. At most `len` bytes are read.
    fn file_keeps_out(&mut self, path: &Path, len: u64) -> bool {
        use std::io::Read as _;
        if len > self.bytes_left {
            return true;
        }
        self.bytes_left -= len;
        match std::fs::File::open(path) {
            Ok(file) => self.read_keeps_out(file.take(len)),
            Err(_) => true,
        }
    }

    /// Whether `reader` names the donor, fails, or is still being read at
    /// the deadline. Each read, of up to [`OUT_SCAN_CHUNK`] bytes, is
    /// searched together with the end of the one before it.
    fn read_keeps_out(&mut self, mut reader: impl std::io::Read) -> bool {
        let mut kept = 0;
        loop {
            if Instant::now() >= self.deadline {
                return true;
            }
            let read = match reader.read(&mut self.buffer[kept..kept + OUT_SCAN_CHUNK]) {
                Ok(0) => return false,
                Ok(read) => read,
                Err(_) => return true,
            };
            let filled = kept + read;
            let window = &self.buffer[..filled];
            if self
                .needles
                .iter()
                .any(|needle| contains_path(window, needle))
            {
                return true;
            }
            kept = filled.min(self.overlap);
            self.buffer.copy_within(filled - kept..filled, 0);
        }
    }
}

/// The files in a shared-layout donor's `deps/`, by the unit hash their name
/// carries (`<name>-<hash>` or `<name>-<hash>.<ext>`). `deps/` holds a file
/// or more per unit, so it is listed once per donor, not once per unit.
fn deps_by_hash(deps: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for file in entries(deps) {
        let name = file_name(&file);
        for hash in unit_hashes(&name) {
            index.entry(hash.to_owned()).or_default().push(file.clone());
        }
    }
    index
}

/// The directories of a shared-layout profile where the compiler writes a
/// unit's outputs: `deps/` for libraries, binaries, tests and benches, and
/// `examples/` for examples (`lib<name>-<hash>.rmeta` and `<name>-<hash>.d`
/// for a check, `<name>-<hash>` for a build). Cargo's own copies without the
/// hash (an uplifted `examples/<name>`) are not a unit's.
const OUTPUT_DIRS: [&str; 2] = ["deps", "examples"];

/// [`deps_by_hash`] of every [`OUTPUT_DIRS`] directory of a shared-layout
/// `profile`. A unit's file keeps its directory: it is copied to the same
/// one in the new target ([`copy_unit`]).
///
/// Indexed from `deps/` alone, an example's check was no root of a check's
/// demand ([`check_demand`]), whose outputs it could not find, and a copy
/// of its fingerprint without them would have been rebuilt anyway: in a new
/// checkout of a 600-crate workspace, the 43 examples were the only units a
/// workspace clippy still compiled.
fn outputs_by_hash(profile: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut index = deps_by_hash(&profile.join(OUTPUT_DIRS[0]));
    for dir in &OUTPUT_DIRS[1..] {
        for (hash, files) in deps_by_hash(&profile.join(dir)) {
            index.entry(hash).or_default().extend(files);
        }
    }
    index
}

/// Every unit hash in a `deps/` file name: 16 hex digits after a `-`,
/// followed by a `.` or the end of the name.
fn unit_hashes(name: &str) -> impl Iterator<Item = &str> {
    name.match_indices('-').filter_map(move |(at, _)| {
        let rest = &name[at + 1..];
        let hash = rest.get(..16)?;
        (crate::cargo_layout::is_unit_hash(hash)
            && matches!(rest.as_bytes().get(16), None | Some(b'.')))
        .then_some(hash)
    })
}

/// The store's memo rows for a donor's artifacts, by path, each with the
/// stamp it was recorded under
/// ([`kache_store::file_hash::FileHashCache::recorded_at`]).
pub(crate) type RecordedHashes = HashMap<String, (kache_store::file_hash::FileFingerprint, String)>;

/// Reads the memo rows for the paths it is given; [`no_recorded_hashes`]
/// when there is no store to ask.
pub(crate) type RecordedLookup<'a> = &'a dyn Fn(&[&str]) -> RecordedHashes;

/// A [`RecordedLookup`] that knows no hashes: every artifact is read.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn no_recorded_hashes(_: &[&str]) -> RecordedHashes {
    RecordedHashes::new()
}

/// Whether `file` is an artifact a dependent's `--extern` reads.
fn is_artifact(file: &Path) -> bool {
    file.extension().is_some_and(|ext| {
        ["rlib", "rmeta", "dylib", "so", "dll"].contains(&&*ext.to_string_lossy())
    })
}

/// The unit's artifacts ([`is_artifact`]), or `None` once `deadline` passes.
///
/// A shared-layout unit's are its files in `deps/` and `examples/` named as
/// artifacts, told by name alone: Cargo keeps no unit's directory there, and
/// a status call for each of a unit's files, made once for the store's rows
/// and again for the check, was 2 of the 4 or 5 a seeding asked of each.
fn unit_artifacts(
    from: &Path,
    layout: Layout,
    unit: &Unit,
    deps: &HashMap<String, Vec<PathBuf>>,
    deadline: Instant,
) -> Option<Vec<PathBuf>> {
    let mut files = match layout {
        Layout::PerUnit => vec![from.join("build").join(&unit.package).join(&unit.hash)],
        Layout::Shared => {
            return (Instant::now() < deadline).then(|| {
                deps.get(&unit.hash)
                    .into_iter()
                    .flatten()
                    .filter(|file| is_artifact(file))
                    .cloned()
                    .collect()
            });
        }
    };
    let mut artifacts = Vec::new();
    while let Some(file) = files.pop() {
        if Instant::now() >= deadline {
            return None;
        }
        if std::fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_dir()) {
            files.extend(entries(&file));
        } else if is_artifact(&file) {
            artifacts.push(file);
        }
    }
    Some(artifacts)
}

/// The memo rows for every artifact of `units`, read in one lookup before
/// the copy threads start.
fn recorded_for_units(
    from: &Path,
    layout: Layout,
    units: &[Unit],
    deps: &HashMap<String, Vec<PathBuf>>,
    recorded: RecordedLookup,
    deadline: Instant,
) -> RecordedHashes {
    let paths: Vec<String> = units
        .iter()
        .filter_map(|unit| unit_artifacts(from, layout, unit, deps, deadline))
        .flatten()
        .map(|file| {
            kache_store::file_hash::absolute_path(&file)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if paths.is_empty() || Instant::now() >= deadline {
        return RecordedHashes::new();
    }
    recorded(&paths.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Whether the donor file `file` is byte for byte a blob in `store`, read
/// only when the store cannot vouch for it. The memo's row for its path
/// vouches when the file's whole stamp (size, modification and change times,
/// inode) is the one the row was recorded under, as a dependent's key takes
/// it for the same file. Or the file is the blob the row names: the same
/// inode, which nobody may write ([`is_blob_inode`]). Links come and go on
/// such an inode with every target that restores or seeds it, and each one
/// moves its change time, so only this second check lasts on a busy store.
fn is_stored_blob(file: &Path, recorded: &RecordedHashes, store: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(file) else {
        return false;
    };
    let now = kache_store::file_hash::FileFingerprint::from_metadata(file, &metadata);
    if let Some((stamp, hash)) = recorded.get(&now.path) {
        let blob = kache_store::blob_path_in_store_dir(store, hash);
        if *stamp == now {
            return blob.is_file();
        }
        if is_blob_inode(&metadata, &blob) {
            return true;
        }
    }
    kache_store::file_hash::hash_file(file)
        .is_ok_and(|hash| kache_store::blob_path_in_store_dir(store, &hash).is_file())
}

/// Whether `metadata` is of the inode at `blob`, and nobody may write it.
/// The store names a blob by the hash of its bytes and keeps it without
/// write permission, so a link to it holds those bytes, as every restore
/// and every seeded link ([`shares_inode`]) already relies on.
#[cfg(unix)]
fn is_blob_inode(metadata: &std::fs::Metadata, blob: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    metadata.is_file()
        && metadata.permissions().mode() & 0o222 == 0
        && std::fs::metadata(blob).is_ok_and(|blob| {
            blob.is_file() && blob.dev() == metadata.dev() && blob.ino() == metadata.ino()
        })
}

/// Without inode numbers, a blob's link is known only by its stamp.
#[cfg(not(unix))]
fn is_blob_inode(_: &std::fs::Metadata, _: &Path) -> bool {
    false
}

/// Whether every artifact of the unit, what a dependent's `--extern` reads,
/// is byte for byte a blob in this cache. A dependent's key hashes those
/// bytes, and another kache or a build without kache writes different ones
/// for the same Cargo unit; such a unit is left for Cargo to build through
/// the cache. A file the store vouches for ([`is_stored_blob`]) is not read
/// again: under load, reading every donor rlib back (0.7-1.2 GB a seeding)
/// left hundreds of units past the deadline.
fn artifacts_stored(
    from: &Path,
    layout: Layout,
    unit: &Unit,
    deps: &HashMap<String, Vec<PathBuf>>,
    cache_dir: &Path,
    recorded: &RecordedHashes,
    deadline: Instant,
) -> bool {
    let Some(files) = unit_artifacts(from, layout, unit, deps, deadline) else {
        return false;
    };
    let store = cache_dir.join("store");
    files
        .iter()
        .all(|file| Instant::now() < deadline && is_stored_blob(file, recorded, &store))
}

/// Copy one unit from the donor profile `from` into `staging`, to be placed
/// in `to` ([`StagedUnit`]); its marker goes last. `deps` is
/// [`outputs_by_hash`] of the donor's profile
/// for the shared layout; `rebase` moves dep-info from `from` to `to`.
/// `build_dir`: whether the donor holds the unit's directory in `build/`
/// ([`BuildDirs`]), where a build script's binary or a run's records are.
/// A file of the unit `to` already held when the seeding began ([`Present`])
/// is left as it is.
fn copy_unit(
    from: &Path,
    to: &Path,
    layout: Layout,
    unit: &Unit,
    deps: &HashMap<String, Vec<PathBuf>>,
    cache_dir: &Path,
    counts: &FileCounts,
    staging: &Staging,
    rebase: &kache_store::link::DepinfoRebase,
    checkout: Option<(&Path, &Path)>,
    build_dir: bool,
    present: &Present,
    deadline: Instant,
) -> std::io::Result<StagedUnit> {
    let relocation = if build_dir {
        relocation::Relocation::for_unit(from, to, rebase, layout, unit, checkout, deadline)?
    } else {
        relocation::Relocation::without_build_dir(from, rebase, unit)
    };
    #[cfg(unix)]
    if build_dir {
        let out = build_script_out_dir(from, layout, unit);
        let record = match layout {
            Layout::PerUnit => out.join(crate::build_script::LAUNCH_RECORD),
            Layout::Shared => out.with_file_name(crate::build_script::LAUNCH_RECORD),
        };
        pin_launcher(from, to, &record, deadline)?;
    }
    match layout {
        Layout::PerUnit => {
            let package = to.join("build").join(&unit.package);
            counts.make_dir(&package)?;
            let tree = stage_tree(
                &from.join("build").join(&unit.package).join(&unit.hash),
                &package.join(&unit.hash),
                cache_dir,
                counts,
                staging,
                deadline,
                Some(&relocation),
            )?;
            Ok(StagedUnit {
                outputs: Vec::new(),
                marker: tree,
            })
        }
        Layout::Shared => {
            let named = format!("{}-{}", unit.package, unit.hash);
            let mut outputs = Vec::new();
            for file in deps.get(&unit.hash).into_iter().flatten() {
                // In `deps/` or `examples/`, as in the donor.
                let Some(dir) = file.parent().and_then(Path::file_name) else {
                    continue;
                };
                let to_dir = to.join(dir);
                counts.make_dir(&to_dir)?;
                let at = to_dir.join(file_name(file));
                if !present.holds(&at) {
                    outputs.push(stage_tree(
                        file,
                        &at,
                        cache_dir,
                        counts,
                        staging,
                        deadline,
                        Some(&relocation),
                    )?);
                }
            }
            let build = from.join("build").join(&named);
            if build_dir {
                counts.make_dir(&to.join("build"))?;
                outputs.push(stage_tree(
                    &build,
                    &to.join("build").join(&named),
                    cache_dir,
                    counts,
                    staging,
                    deadline,
                    Some(&relocation),
                )?);
            }
            counts.make_dir(&to.join(".fingerprint"))?;
            let marker = stage_tree(
                &from.join(".fingerprint").join(&named),
                &to.join(".fingerprint").join(&named),
                cache_dir,
                counts,
                staging,
                deadline,
                Some(&relocation),
            )?;
            Ok(StagedUnit { outputs, marker })
        }
    }
}

/// A unit a copy thread has copied and not yet given its final names: each
/// output, then the marker that makes the unit whole for Cargo (its
/// fingerprint directory, or the whole unit in the per-unit layout).
///
/// A copy thread places its units [`PLACE_BATCH`] at a time
/// ([`place_staged`]), a directory at a time. Every unit's final names go
/// into the same few directories (`deps/`, `build/`, `.fingerprint/`), which
/// the kernel changes one name at a time; placed as each unit was copied,
/// they were one wait for the directory a name, and on a loaded machine the
/// thread holding it was often preempted while the seven others slept on
/// it: at load 125-205 a seeding's renames waited 17.9 s, summed over its
/// threads, for 1.4 s of their processor time.
struct StagedUnit {
    outputs: Vec<Place>,
    marker: Place,
}

impl StagedUnit {
    /// Where the unit's fingerprint directory is staged.
    fn fingerprint(&self, layout: Layout) -> PathBuf {
        let staged = self.marker.staged();
        match layout {
            Layout::PerUnit => staged.join("fingerprint"),
            Layout::Shared => staged.to_path_buf(),
        }
    }
}

/// A name a seeding gives the new profile: a staged copy renamed to it, or a
/// donor file linked to it ([`link_in_place`]).
enum Place {
    Rename {
        staging: PathBuf,
        to: PathBuf,
        /// Files placed under `staging`, given their final paths once the
        /// rename is made.
        placed: Vec<PlacedFile>,
        /// Held past the rename, so a sweep keeps each run the tree links
        /// to until its recorded `OUT_DIR` does.
        #[cfg_attr(not(unix), allow(dead_code))]
        runs: Vec<HeldRun>,
    },
    /// Only Unix links a donor file in place ([`link_in_place`]).
    #[cfg_attr(not(unix), allow(dead_code))]
    Link {
        from: PathBuf,
        metadata: std::fs::Metadata,
        to: PathBuf,
        /// Where a copy is staged when the link cannot be made.
        staging: PathBuf,
    },
}

impl Place {
    fn to(&self) -> &Path {
        match self {
            Self::Rename { to, .. } | Self::Link { to, .. } => to,
        }
    }

    /// Where the name's contents are until it is placed. For a staged tree,
    /// its fingerprint lives there too ([`StagedUnit::fingerprint`]).
    fn staged(&self) -> &Path {
        match self {
            Self::Rename { staging, .. } => staging,
            Self::Link { from, .. } => from,
        }
    }

    /// Give the new profile this name.
    fn apply(self, counts: &FileCounts) -> std::io::Result<()> {
        match self {
            Self::Rename {
                staging,
                to,
                placed,
                runs,
            } => {
                let renamed = std::fs::rename(&staging, &to);
                drop(runs);
                if let Err(error) = renamed {
                    let _ = std::fs::remove_dir_all(&staging);
                    let _ = std::fs::remove_file(&staging);
                    return Err(error);
                }
                let mut all = counts.placed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                all.extend(placed.into_iter().map(|mut file: PlacedFile| {
                    file.at = match file.at.strip_prefix(&staging) {
                        Ok(below) if below.as_os_str().is_empty() => to.clone(),
                        Ok(below) => to.join(below),
                        Err(_) => file.at,
                    };
                    file
                }));
                Ok(())
            }
            Self::Link {
                from,
                metadata,
                to,
                staging,
            } => {
                #[cfg(unix)]
                if link_read_only(&from, &to) {
                    counts.linked.fetch_add(1, Ordering::Relaxed);
                    if let Some(placed) = placed_file(&from, &metadata, &to) {
                        counts
                            .placed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(placed);
                    }
                    return Ok(());
                }
                copy_file(&from, &staging, &metadata, counts.umask)
                    .and_then(|()| std::fs::rename(&staging, &to))
                    .inspect_err(|_| {
                        let _ = std::fs::remove_file(&staging);
                    })?;
                counts.copied.fetch_add(1, Ordering::Relaxed);
                if let Some(placed) = placed_file(&from, &metadata, &to) {
                    counts
                        .placed
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(placed);
                }
                Ok(())
            }
        }
    }

    /// Leave the name out: what was staged for it goes.
    fn discard(self) {
        if let Self::Rename { staging, .. } = self {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = std::fs::remove_file(&staging);
        }
    }
}

/// Place a copy thread's staged `units` in the new profile and say how many
/// it placed whole: every unit's outputs first, then their markers, each
/// directory's names together while the thread holds the directory
/// ([`FileCounts::directory`]). A unit one of whose outputs could not be
/// placed keeps out its marker: what is left of it is complete names, which
/// Cargo builds the unit over. Holding a directory for a run of names, the
/// thread sleeps for it once rather than once a name.
///
/// The seeding's deadline does not cut this short: each unit here is copied
/// whole, and its names are a rename or a link each, so a seeding past its
/// deadline keeps what it copied, as when each unit was placed as it was
/// copied.
fn place_staged(units: Vec<StagedUnit>, counts: &FileCounts) -> usize {
    let mut failed = vec![false; units.len()];
    let mut outputs = Vec::new();
    let mut markers = Vec::new();
    for (index, unit) in units.into_iter().enumerate() {
        outputs.extend(unit.outputs.into_iter().map(|place| (index, place)));
        markers.push((index, unit.marker));
    }
    let place = |names: Vec<(usize, Place)>, failed: &mut Vec<bool>| {
        let mut names = names;
        // Stable: a unit's names in one directory keep their order.
        names.sort_by(|(_, a), (_, b)| a.to().parent().cmp(&b.to().parent()));
        let mut names = names.into_iter().peekable();
        while let Some((_, first)) = names.peek() {
            let dir = first.to().parent().unwrap_or(first.to()).to_path_buf();
            let lock = counts.directory(&dir);
            let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while let Some((index, name)) =
                names.next_if(|(_, name)| name.to().parent().unwrap_or(name.to()) == dir)
            {
                if failed[index] {
                    name.discard();
                } else if name.apply(counts).is_err() {
                    failed[index] = true;
                }
            }
        }
    };
    place(outputs, &mut failed);
    place(markers, &mut failed);
    failed.iter().filter(|failed| !**failed).count()
}

/// Copy `from` (a file or a directory) into `staging` and rename it to `to`.
/// A copy thread stages a unit's names ([`stage_tree`]) and places them later.
#[cfg(test)]
fn place_tree(
    from: &Path,
    to: &Path,
    cache_dir: &Path,
    counts: &FileCounts,
    staging: &Staging,
    deadline: Instant,
    relocation: Option<&relocation::Relocation<'_>>,
) -> std::io::Result<()> {
    stage_tree(from, to, cache_dir, counts, staging, deadline, relocation)?.apply(counts)
}

/// Copy `from` (a file or a directory) into `staging`, to be renamed to `to`
/// ([`Place::apply`]); a file [`link_in_place`] allows is left to be linked
/// there instead.
fn stage_tree(
    from: &Path,
    to: &Path,
    cache_dir: &Path,
    counts: &FileCounts,
    staging: &Staging,
    deadline: Instant,
    relocation: Option<&relocation::Relocation<'_>>,
) -> std::io::Result<Place> {
    let metadata = std::fs::symlink_metadata(from)?;
    let staging = staging.path_for(to);
    #[cfg(unix)]
    if link_in_place(from, &metadata, counts, relocation, deadline)? {
        return Ok(Place::Link {
            from: from.to_path_buf(),
            metadata,
            to: to.to_path_buf(),
            staging,
        });
    }
    let placed = std::sync::Mutex::new(Vec::new());
    let tree = Placing {
        from,
        to,
        cache_dir,
        counts,
        placed: &placed,
        relocation,
    };
    let mut runs = Vec::new();
    if let Err(error) = copy_tree(from, &metadata, &staging, tree, &mut runs, deadline) {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(Place::Rename {
        staging,
        to: to.to_path_buf(),
        placed: placed.into_inner().unwrap_or_default(),
        runs,
    })
}

/// Where a copy thread builds what it places by rename: in a directory of
/// the thread's own in the new profile, else beside each final name.
///
/// Making a directory is the costliest change a seeding asks of the
/// filesystem (ext4: an inode and a block, 0.4-0.5 ms against 26-38 µs to
/// create a file or rename one), and the parent stays locked meanwhile. Four
/// threads making each unit's staging directory beside the others in
/// `.fingerprint/` took turns, no faster than one, and under load each turn
/// grew: a seeding's copy was 85-95 % of it. In a directory of its own each
/// thread makes them side by side, and the parents shared by every unit
/// (`.fingerprint/`, `build/`, `deps/`) see one rename a placement, which
/// still makes each name whole at once.
struct Staging(Option<tempfile::TempDir>);

impl Staging {
    /// A directory of this thread's own in `profile`, removed with whatever
    /// a failed copy left in it when the thread is done; beside each final
    /// name when it cannot be made.
    fn in_profile(profile: &Path) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(STAGING)
                .tempdir_in(profile)
                .ok(),
        )
    }

    /// Beside each final name.
    #[cfg(test)]
    fn beside() -> Self {
        Self(None)
    }

    /// Where `to` is built before its rename. A thread's directory holds
    /// every unit it copied until they are placed ([`StagedUnit`]), so the
    /// name of `to`'s directory is part of it: a unit's `build/` and
    /// `.fingerprint/` directories have one name.
    fn path_for(&self, to: &Path) -> PathBuf {
        match &self.0 {
            Some(dir) => dir.path().join(format!(
                "{}~{}",
                to.parent().map(file_name).unwrap_or_default(),
                file_name(to)
            )),
            None => to.with_file_name(format!("{STAGING}{}-{}", file_name(to), std::process::id())),
        }
    }
}

/// Whether the donor file `from`, of `metadata`, is to be linked straight to
/// its final name ([`Place::Link`]): once this seeding knows clones fail
/// ([`FileCounts::no_clones`]) and when [`shares_inode`] allows a link. A
/// link makes the name whole at once, as the rename of a staged copy does,
/// and spares the journal that rename. False when `from` is not such a
/// file: the caller stages it.
#[cfg(unix)]
fn link_in_place(
    from: &Path,
    metadata: &std::fs::Metadata,
    counts: &FileCounts,
    relocation: Option<&relocation::Relocation<'_>>,
    deadline: Instant,
) -> std::io::Result<bool> {
    if !counts.no_clones.load(Ordering::Relaxed) {
        return Ok(false);
    }
    if !metadata.is_file() || !shares_inode(Path::new(""), metadata) {
        return Ok(false);
    }
    if Instant::now() >= deadline {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    // Dep-info and a build script's records may be written anew, and are
    // staged: their bytes are read once, where the staged copy is made.
    Ok(!relocation.is_some_and(|relocation| relocation.rewrites(from)))
}

/// The [`PlacedFile`] for the donor file `from`, with `metadata` read before
/// it was placed at `at`, when it is large enough for the hash memo.
fn placed_file(from: &Path, metadata: &std::fs::Metadata, at: &Path) -> Option<PlacedFile> {
    (metadata.len() >= crate::target_dedup::MIN_BYTES).then(|| PlacedFile {
        from: from.to_path_buf(),
        before: kache_store::file_hash::FileFingerprint {
            path: from.to_string_lossy().into_owned(),
            size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            mtime_ns: kache_store::file_hash::metadata_mtime_ns(metadata),
            ctime_ns: kache_store::file_hash::metadata_ctime_ns(metadata),
            inode: kache_store::file_hash::metadata_inode(metadata),
        },
        at: at.to_path_buf(),
    })
}

/// A tree [`place_tree`] copies: where it comes from, where it is placed,
/// and the cache whose hermetic runs a link in it may name.
#[derive(Clone, Copy)]
#[cfg_attr(not(unix), allow(dead_code))]
struct Placing<'a> {
    from: &'a Path,
    to: &'a Path,
    cache_dir: &'a Path,
    counts: &'a FileCounts,
    /// Files placed under the staging path, before the tree's rename.
    placed: &'a std::sync::Mutex<Vec<PlacedFile>>,
    /// Target-local metadata rewritten for the new target.
    relocation: Option<&'a relocation::Relocation<'a>>,
}

/// A donor file a seeding placed, large enough for the hash memo: the donor
/// path and its stamp before it was linked or copied, and where it went. The
/// donor's memo row for that stamp is the placed file's hash too, so the
/// seeding can memoise it ([`PlacedFile::memo_rows`]) and the first build in
/// the new target does not read it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacedFile {
    pub(crate) from: PathBuf,
    pub(crate) before: kache_store::file_hash::FileFingerprint,
    pub(crate) at: PathBuf,
}

impl PlacedFile {
    /// The memo rows this placement earns, given `hash`, the donor's memo
    /// row for its stamp before the seeding, and `since_ns`, read from
    /// [`kache_store::file_hash::stamp_clock_ns`] before the seeding began.
    ///
    /// A link shares the donor's inode, which nobody may write
    /// ([`shares_inode`]): the link itself changes the ctime, and nothing
    /// else did when the size, mtime and inode are the donor's and the ctime
    /// moved only during the seeding. That stamp then holds for the donor
    /// path too. A copy is a new inode with the donor's mtime; its bytes are
    /// the donor's when the donor's whole stamp did not move while it was
    /// copied.
    pub(crate) fn memo_rows(
        &self,
        since_ns: i64,
    ) -> Vec<kache_store::file_hash::FileFingerprint> {
        use kache_store::file_hash::FileFingerprint;
        let (Ok(from), Ok(at)) = (
            FileFingerprint::from_path(&self.from),
            FileFingerprint::from_path(&self.at),
        ) else {
            return Vec::new();
        };
        let before = &self.before;
        let content_kept = |now: &FileFingerprint| {
            now.size == before.size && now.mtime_ns == before.mtime_ns
        };
        if before.inode != 0 && at.inode == before.inode {
            let linked_now = |now: &FileFingerprint| {
                now.inode == before.inode
                    && content_kept(now)
                    && now.ctime_ns >= before.ctime_ns
                    && (now.ctime_ns == before.ctime_ns
                        || kache_store::file_hash::stamp_written_since(now, since_ns))
            };
            if linked_now(&at) && linked_now(&from) {
                return vec![at, from];
            }
            return Vec::new();
        }
        if from == *before && content_kept(&at) {
            return vec![at];
        }
        Vec::new()
    }
}

/// Copy `from`, a file or directory in `tree` whose status (not following
/// a link) is `metadata`, to `to`, keeping every file's modification time.
/// A symbolic link that stays inside the tree is copied as a link, and a
/// build script's `OUT_DIR` linked to its sealed hermetic run is linked to
/// the same run; any other symbolic link fails the copy, and so does
/// reaching `deadline` before a file. The runs linked to are added to
/// `runs`, held.
#[cfg_attr(not(unix), allow(clippy::only_used_in_recursion))]
fn copy_tree(
    from: &Path,
    metadata: &std::fs::Metadata,
    to: &Path,
    tree: Placing,
    runs: &mut Vec<HeldRun>,
    deadline: Instant,
) -> std::io::Result<()> {
    if metadata.is_dir() {
        std::fs::create_dir(to)?;
        for (entry, metadata) in entries_with_status(from)? {
            copy_tree(&entry, &metadata, &to.join(file_name(&entry)), tree, runs, deadline)?;
        }
        return Ok(());
    }
    if Instant::now() >= deadline {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    #[cfg(unix)]
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        let below = from
            .strip_prefix(tree.from)
            .map_err(std::io::Error::other)?;
        if crate::build_script::link_stays_inside(below, &target) {
            return std::os::unix::fs::symlink(target, to);
        }
        let cargo_out_dir = tree.to.join(below);
        if let Some(run) =
            crate::build_script::link_sealed_out_dir(tree.cache_dir, &target, &cargo_out_dir, to)
                .map_err(std::io::Error::other)?
        {
            runs.push(run);
            return Ok(());
        }
        if let Some(rebased) = tree
            .relocation
            .and_then(|relocation| relocation.out_dir_link(&target))
        {
            return std::os::unix::fs::symlink(rebased, to);
        }
    }
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a file or a directory",
            from.display()
        )));
    }
    let below = from.strip_prefix(tree.from).unwrap_or(Path::new(""));
    if is_invocation_stamp(below) {
        return Ok(());
    }
    // Dep-info and a build script's records name the donor's paths: they
    // are written anew for this target, never linked, and never memoised
    // as the donor's bytes.
    let contents = tree
        .relocation
        .map(|relocation| relocation.contents(from, metadata, deadline))
        .transpose()?
        .flatten();
    if let Some(contents) = contents {
        write_stamped(to, &mut &contents[..], metadata, tree.counts.umask)?;
        tree.counts.copied.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    place_file(from, below, to, metadata, tree.counts)?;
    if let Some(placed) = placed_file(from, metadata, to) {
        tree.placed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(placed);
    }
    Ok(())
}

/// The name of Cargo's stamp of when it last started a unit.
const INVOCATION_STAMP: &str = "invoked.timestamp";

/// Whether `below`, a path inside a unit's tree, is Cargo's stamp of when it
/// last started the unit: `invoked.timestamp` in a unit's fingerprint
/// directory or a build script run's directory (`fingerprint/` and `run/`
/// in the per-unit layout), never a file a build script wrote to `out/`.
///
/// Cargo writes the stamp anew just before it runs the unit and reads only
/// the time of the one it just wrote, to date the unit's dep-info and
/// outputs (cargo-util's `set_invocation_time`). Its freshness check never
/// reads one, and kache reads only the stamp of the unit it is compiling
/// (`cargo_check_redate_ns`), which Cargo has written by then, and leaves
/// stamps out when it reads a donor's fingerprints ([`check_demand`]). A
/// seeded stamp is never read, so a seeding leaves it out: one file a unit
/// fewer to create, write, date and give a block of the disk (ext4 spends
/// about as long allocating it as a `mkdir`): 2,188 of the 13,032 files a
/// seeding of command-center's workspace clippy wrote.
fn is_invocation_stamp(below: &Path) -> bool {
    let mut parts = below.components().map(|part| part.as_os_str());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), None, None) => name == INVOCATION_STAMP,
        (Some(dir), Some(name), None) => {
            (dir == "fingerprint" || dir == "run") && name == INVOCATION_STAMP
        }
        _ => false,
    }
}

/// Give a cloned or copied file `metadata`'s modification time and
/// permissions. A Linux reflink creates `to` with the default mode, so
/// without the permissions a seeded build script is one Cargo cannot execute.
fn stamp(to: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    filetime::set_file_mtime(
        to,
        filetime::FileTime::from_last_modification_time(metadata),
    )?;
    std::fs::set_permissions(to, metadata.permissions())
}

/// Write `contents` to a new file `to` with `metadata`'s modification time
/// and permissions, set through the open file. On a busy journal each
/// metadata call waits its turn: `std::fs::copy` and [`stamp`] made five of
/// them (create, mode, write, time by path, mode again by path), this makes
/// three, and a fourth only when the umask took bits from the mode.
///
/// `umask`, the process's file creation mask when known, tells when the
/// mode is the one asked for without asking the file: no set-id bit, which
/// a write clears, and no bit the mask takes away.
#[cfg(unix)]
fn write_stamped(
    to: &Path,
    contents: &mut impl std::io::Read,
    metadata: &std::fs::Metadata,
    umask: Option<u32>,
) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = metadata.permissions().mode() & 0o7777;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(to)?;
    std::io::copy(contents, &mut file)?;
    let kept = mode & 0o6000 == 0 && umask.is_some_and(|umask| mode & umask == 0);
    // After the write, which clears set-id bits.
    if !kept && file.metadata()?.permissions().mode() & 0o7777 != mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    filetime::set_file_handle_times(
        &file,
        None,
        Some(filetime::FileTime::from_last_modification_time(metadata)),
    )
}

/// Without a mode to create a file with, written and then stamped.
#[cfg(not(unix))]
fn write_stamped(
    to: &Path,
    contents: &mut impl std::io::Read,
    metadata: &std::fs::Metadata,
    _: Option<u32>,
) -> std::io::Result<()> {
    std::io::copy(contents, &mut std::fs::File::create(to)?)?;
    stamp(to, metadata)
}

/// The process's file creation mask, where it can be read without changing
/// it ([`write_stamped`]).
fn creation_mask() -> Option<u32> {
    #[cfg(unix)]
    {
        kache_client::creation_mask()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Files up to this size are copied through memory ([`copy_file`]).
const SMALL_COPY: u64 = 64 << 10;

/// Copy the donor file `from`, of `metadata`, to a new file `to`
/// ([`write_stamped`]). A file of at most [`SMALL_COPY`] bytes, as nearly
/// every one a seeding copies (a unit's fingerprint files, a run's records),
/// is read in one call and written in one: copying file to file asked the
/// kernel for both files' kinds and for its in-kernel copy twice, the second
/// time to find the end, which is as costly for a few bytes. A read of one
/// byte more than the stat says finds the end in the same call.
fn copy_file(
    from: &Path,
    to: &Path,
    metadata: &std::fs::Metadata,
    umask: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Read;
    let mut source = std::fs::File::open(from)?;
    if metadata.len() > SMALL_COPY {
        return write_stamped(to, &mut source, metadata, umask);
    }
    let len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let mut bytes = vec![0; len + 1];
    let read = source.read(&mut bytes)?;
    bytes.truncate(read);
    if read != len {
        // Not the size the stat said: copy what it holds now.
        source.read_to_end(&mut bytes)?;
    }
    write_stamped(to, &mut &bytes[..], metadata, umask)
}

/// Whether a clone failed because this filesystem, or this pair of
/// directories, cannot clone at all, as ext4 and tmpfs refuse every one.
fn cannot_clone(error: &anyhow::Error) -> bool {
    let Some(error) = error.downcast_ref::<std::io::Error>() else {
        return false;
    };
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return [
            libc::EOPNOTSUPP,
            libc::ENOTSUP,
            libc::EXDEV,
            libc::ENOTTY,
            libc::ENOSYS,
        ]
        .contains(&code);
    }
    error.kind() == std::io::ErrorKind::Unsupported
}

/// Files a seeding gave by hard link and by copy or clone, and the
/// directories it made for them.
#[derive(Debug, Default)]
struct FileCounts {
    linked: AtomicUsize,
    copied: AtomicUsize,
    /// Set once a clone fails because the filesystem cannot make one
    /// ([`cannot_clone`]). Each failed clone created and removed a file in
    /// the target, two waits on a busy journal, so the seeding stops
    /// asking.
    no_clones: std::sync::atomic::AtomicBool,
    /// Files placed for good, at their final paths.
    placed: std::sync::Mutex<Vec<PlacedFile>>,
    /// The new profile's directories this seeding made or found.
    made: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// A lock for each directory the copy threads place names into
    /// ([`FileCounts::directory`]).
    placing: std::sync::Mutex<HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>,
    /// The process's file creation mask, read once a seeding
    /// ([`write_stamped`]).
    umask: Option<u32>,
}

impl FileCounts {
    /// Make the directory `dir` once a seeding: each unit placed into
    /// `deps/`, `build/` or `.fingerprint/` asked for its directory again,
    /// a lookup and a refused `mkdir`, which waits for the directory's lock
    /// like a change of it.
    fn make_dir(&self, dir: &Path) -> std::io::Result<()> {
        let lock = || {
            self.made
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        };
        if lock().contains(dir) {
            return Ok(());
        }
        std::fs::create_dir_all(dir)?;
        lock().insert(dir.to_path_buf());
        Ok(())
    }

    /// The lock a copy thread holds while it adds names to the directory
    /// `dir` ([`place_staged`]), so that no other one adds names there
    /// meanwhile.
    ///
    /// The kernel changes a directory one name at a time, under the
    /// directory's lock, and every unit's final renames and links go into
    /// the same few (`.fingerprint/`, `deps/`, `build/`). Eight threads
    /// waiting on that lock spun on the processor while its holder ran;
    /// waiting here, they sleep. Under a bounded load (load 119-195) a
    /// seeding's 5,037 renames took 0.7-1.2 s of the processor instead of
    /// 1.4-4.2 s, at the same wall time.
    fn directory(&self, dir: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
        std::sync::Arc::clone(
            self.placing
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(dir.to_path_buf())
                .or_default(),
        )
    }
}

/// Give `to` the donor file `from` (`below` is its path inside the unit's
/// tree): a clone where the filesystem can make one, else a hard link when
/// [`shares_inode`] allows it, else a copy. A link is the donor's own inode,
/// so its modification time and mode are already the donor's.
fn place_file(
    from: &Path,
    below: &Path,
    to: &Path,
    metadata: &std::fs::Metadata,
    counts: &FileCounts,
) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = below;
    if !counts.no_clones.load(Ordering::Relaxed) {
        match kache_store::link::try_reflink(from, to) {
            Ok(()) => {
                counts.copied.fetch_add(1, Ordering::Relaxed);
                return stamp(to, metadata);
            }
            Err(error) if cannot_clone(&error) => {
                counts.no_clones.store(true, Ordering::Relaxed);
            }
            Err(_) => {}
        }
    }
    #[cfg(unix)]
    if shares_inode(below, metadata) && link_read_only(from, to) {
        counts.linked.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    copy_file(from, to, metadata, counts.umask)?;
    counts.copied.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Whether a donor file may be given to the new target as a hard link rather
/// than a copy: a regular file that nobody may write, outside a build script's
/// `out/`, while kache does not run as root (root writes any file).
///
/// kache's store keeps its blobs without write permission (`0444`, `0440`
/// when a group shares the store), and a donor's restored outputs are links
/// to them, so these are most of a donor's bytes. Nothing in a build rewrites
/// such a file in place: rustc and the linkers write a new file and rename it
/// over the old one, which leaves every other link alone, and a tool that
/// opens one for writing fails in the new target exactly as it would in the
/// donor, where the file has the same mode. So the shared inode is never
/// written through, neither into the donor nor into the store. A build
/// script's `out/` holds whatever the script wrote; a rerun could make a
/// file writable and rewrite it, so those files stay copies.
#[cfg(unix)]
fn shares_inode(below: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file()
        && metadata.permissions().mode() & 0o222 == 0
        && !below
            .components()
            .any(|component| component.as_os_str() == "out")
        // SAFETY: geteuid has no preconditions and cannot fail.
        && unsafe { libc::geteuid() } != 0
}

/// Hard-link `from` to `to`. False when the link cannot be made (another
/// filesystem, the link limit, the kernel's protected hard links for a file
/// another user owns), or when the inode it shares turns out writable after
/// all, in which case the link is removed again and the caller copies.
#[cfg(unix)]
fn link_read_only(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::hard_link(from, to).is_err() {
        return false;
    }
    match std::fs::symlink_metadata(to) {
        Ok(linked) if linked.is_file() && linked.permissions().mode() & 0o222 == 0 => true,
        _ => {
            let _ = std::fs::remove_file(to);
            false
        }
    }
}

/// When a seeding stops taking units: at `soft`, or past it while units keep
/// coming, at least one every [`SEED_STALL`], until `limit`. Everything a
/// unit's copy waits on (reading and comparing files, the store's rows)
/// gives up at `limit`.
///
/// A deadline that does not move with the copy cut it short exactly when it
/// was worth the most: on a loaded machine, where each unit left to the
/// wrapper costs more again ([`SEED_LIMIT`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SeedDeadline {
    pub(crate) soft: Instant,
    pub(crate) limit: Instant,
}

impl SeedDeadline {
    /// [`SEED_DEADLINE`] and [`SEED_LIMIT`] from `started`.
    pub(crate) fn from_start(started: Instant) -> Self {
        Self {
            soft: started + SEED_DEADLINE,
            limit: started + SEED_LIMIT,
        }
    }

    /// A deadline that never moves.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn fixed(at: Instant) -> Self {
        Self {
            soft: at,
            limit: at,
        }
    }
}

/// The copy's progress against a [`SeedDeadline`], shared by its threads.
struct Progress {
    deadline: SeedDeadline,
    started: Instant,
    /// When the last unit was copied, in nanoseconds from `started`.
    copied_at: std::sync::atomic::AtomicU64,
}

impl Progress {
    fn new(deadline: SeedDeadline) -> Self {
        Self {
            deadline,
            started: Instant::now(),
            copied_at: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn copied(&self, now: Instant) {
        let at = now.saturating_duration_since(self.started).as_nanos();
        self.copied_at
            .fetch_max(u64::try_from(at).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Whether another unit may be taken at `now`.
    fn takes_more(&self, now: Instant) -> bool {
        if now < self.deadline.soft {
            return true;
        }
        let last = self.started
            + Duration::from_nanos(self.copied_at.load(Ordering::Relaxed));
        now < self.deadline.limit && now.saturating_duration_since(last) < SEED_STALL
    }
}

/// What a seeding did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Seeded {
    pub(crate) units: usize,
    pub(crate) donor: Option<PathBuf>,
    /// The donor's units the deadline left untried ([`SeedDeadline`]).
    pub(crate) left: usize,
    /// Files given as hard links rather than copies ([`shares_inode`]).
    pub(crate) linked: usize,
    /// Files large enough for the hash memo, for [`PlacedFile::memo_rows`].
    pub(crate) placed: Vec<PlacedFile>,
}

/// Editor and check targets reuse only the same lane in another checkout.
fn seed_lane(directory: &Path) -> &str {
    match directory.file_name().and_then(OsStr::to_str) {
        Some("check") => "check",
        Some("rust-analyzer") => "rust-analyzer",
        _ => "build",
    }
}

/// Copy the registry units `target` needs from the first donor, most recent
/// first, built by the `rustc -vV` in `rustc_version` that has any. A link
/// to a hermetic run is linked again only into `cache_dir`. Stops at
/// `deadline`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn seed(
    target: &NewTarget,
    rustc_version: &str,
    donors: &[Donor],
    cache_dir: &Path,
    deadline: Instant,
) -> Seeded {
    seed_for_lane(
        target,
        rustc_version,
        donors,
        cache_dir,
        &no_recorded_hashes,
        SeedDeadline::fixed(deadline),
        false,
    )
}

/// [`seed`], and for a build that only checks (`check_lane`), only the units
/// a check of the donor's workspace used ([`check_demand`]). Without such a
/// record in the donor, every unit, as [`seed`]. `recorded` reads the
/// store's memo rows for the donor's artifacts ([`artifacts_stored`]).
pub(crate) fn seed_for_lane(
    target: &NewTarget,
    rustc_version: &str,
    donors: &[Donor],
    cache_dir: &Path,
    recorded: RecordedLookup,
    deadline: SeedDeadline,
    check_lane: bool,
) -> Seeded {
    let Ok(lockfile) = std::fs::read_to_string(target.workspace_root.join("Cargo.lock")) else {
        return Seeded::default();
    };
    let registry = registry_packages(&lockfile);
    let mode = workspace_units_wanted(std::env::var_os("KACHE_SEED_WORKSPACE_UNITS").as_deref());
    let workspace = if mode == WorkspaceUnits::Off {
        BTreeSet::new()
    } else {
        path_packages(&lockfile)
    };
    if (registry.is_empty() && workspace.is_empty()) || rustc_version.trim().is_empty() {
        return Seeded::default();
    }
    let both: BTreeSet<String> = &registry | &workspace;
    let to = target.target_dir.join(PROFILE);
    let creates_target = !target.target_dir.exists();
    if std::fs::create_dir_all(&to).is_err() {
        return Seeded::default();
    }
    if creates_target {
        tag_as_cargo_target(&target.target_dir);
    }
    let Some(_ours) = try_lock(&to.join(".cargo-lock")) else {
        return Seeded::default();
    };
    // Another build may have started here since the probe looked.
    if built(&target.target_dir) {
        return Seeded::default();
    }
    let checked = mode == WorkspaceUnits::Checked;
    for (tried, (donor, layout, read)) in
        ranked_donors(target, rustc_version, donors, &registry, &both, checked, check_lane)
            .enumerate()
    {
        // The first donor goes on past `soft` while it copies; another, tried
        // when the one before copied nothing, only before it.
        let now = Instant::now();
        if now >= deadline.limit || (tried > 0 && now >= deadline.soft) {
            break;
        }
        let same_checkout = same_checkout(donor, target);
        // Another checkout's workspace units, each checked by content.
        let cross = (!same_checkout && checked && !workspace.is_empty())
            .then(|| {
                workspace::CrossCheckout::new(
                    &donor.workspace_root,
                    &donor.target_dir,
                    &target.workspace_root,
                    &workspace,
                    !checkout_has_other_targets(target, donors),
                    deadline.limit,
                )
            })
            .flatten();
        let packages = if same_checkout || cross.is_some() {
            &both
        } else {
            &registry
        };
        let needles = needles_for(donor, same_checkout);
        // A workspace run of another checkout may name the checkout where
        // its copy can name this one ([`workspace::CrossCheckout::verify`]);
        // the donor's target still keeps it out.
        let target_needles = needles_for(donor, true);
        let from = donor.target_dir.join(PROFILE);
        let Some(_theirs) = try_lock_shared(&from.join(".cargo-lock")) else {
            continue;
        };
        // A check's donor ranked first was read whole by the ranking: its
        // outputs, units and demand are not read a second time.
        let (deps, mut units, demand) = match (layout, read) {
            (Layout::Shared, Some(read)) => {
                let mut held = read.held;
                held.retain(|unit| packages.contains(&unit.package));
                (read.deps, in_build_order(&from, layout, held), read.demand)
            }
            (Layout::Shared, None) => {
                let deps = outputs_by_hash(&from);
                let demand = check_lane
                    .then(|| check_demand(&from, &registry, &deps))
                    .flatten();
                (deps, units(&from, layout, packages), demand)
            }
            (Layout::PerUnit, _) => (HashMap::new(), units(&from, layout, packages), None),
        };
        if let Some(demand) = demand {
            let all = units.len();
            units.retain(|unit| demand.contains(&format!("{}-{}", unit.package, unit.hash)));
            tracing::info!(
                "check build: seeding {} of {all} units the donor's check used",
                units.len()
            );
        }
        let build_dirs = BuildDirs::of(&from, layout);
        let present = Present::of(&to, layout);
        let limit = deadline.limit;
        let recorded = recorded_for_units(&from, layout, &units, &deps, recorded, limit);
        let progress = Progress::new(deadline);
        let next = AtomicUsize::new(0);
        let copied = AtomicUsize::new(0);
        let files = FileCounts {
            umask: creation_mask(),
            ..FileCounts::default()
        };
        let rebase = kache_store::link::DepinfoRebase::new(&from, &to);
        let cancellation = crate::invocation::process::current();
        std::thread::scope(|scope| {
            for _ in 0..copy_threads() {
                let cancellation = cancellation.clone();
                let (units, next, copied, from, to, deps, files, recorded, needles, cross) = (
                    &units, &next, &copied, &from, &to, &deps, &files, &recorded, &needles, &cross,
                );
                let rebase = &rebase;
                let progress = &progress;
                let target_needles = &target_needles;
                let workspace = &workspace;
                let build_dirs = &build_dirs;
                let present = &present;
                scope.spawn(move || {
                    let _cancellation =
                        crate::invocation::process::CancellationScope::borrow(cancellation);
                    let staging = Staging::in_profile(to);
                    let mut staged = Vec::new();
                    loop {
                        // Taken only while the deadline allows: a unit
                        // left at the end counts as left.
                        if !progress.takes_more(Instant::now())
                            || crate::invocation::process::check().is_err()
                        {
                            break;
                        }
                        let Some(unit) = units.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            break;
                        };
                        let checked_here = cross.is_some() && workspace.contains(&unit.package);
                        let record_needles = if checked_here {
                            target_needles
                        } else {
                            needles
                        };
                        let build_dir = build_dirs.holds(layout, unit);
                        if present.holds(&marker(to, layout, unit))
                            || (build_dir
                                && (names_the_donor(from, layout, unit, record_needles)
                                    || out_dir_names_the_donor(from, layout, unit, needles, limit)))
                            || !artifacts_stored(
                                from, layout, unit, deps, cache_dir, recorded, limit,
                            )
                        {
                            continue;
                        }
                        // A workspace unit from another checkout goes only
                        // once its sources are this checkout's bytes.
                        let verified = match cross {
                            Some(cross) if checked_here => {
                                match cross.verify(
                                    &unit.package,
                                    &fingerprint_dir(from, layout, unit),
                                    &run_record(from, layout, unit),
                                ) {
                                    Some(verified)
                                        if verified.is_run()
                                            || !cross.reads_checkout_from_env(
                                                &compiler_dep_infos(
                                                    from, layout, unit, deps, build_dir,
                                                ),
                                            ) =>
                                    {
                                        Some(verified)
                                    }
                                    _ => continue,
                                }
                            }
                            _ => None,
                        };
                        // A verified run's directives name this checkout.
                        let checkout = match (&verified, cross) {
                            (Some(verified), Some(cross)) if verified.is_run() => {
                                Some(cross.checkouts())
                            }
                            _ => None,
                        };
                        match copy_unit(
                            from, to, layout, unit, deps, cache_dir, files, &staging, rebase,
                            checkout, build_dir, present, limit,
                        ) {
                            Ok(unit_staged) => {
                                // Dated before it is placed, its dep-info
                                // naming this checkout's files. One that
                                // fails is removed: Cargo builds the unit.
                                if let (Some(verified), Some(cross)) = (verified, cross.as_ref())
                                    && let Err(error) =
                                        cross.finish(&unit_staged.fingerprint(layout), &verified)
                                {
                                    tracing::debug!(
                                        "could not date {}-{}: {error}",
                                        unit.package,
                                        unit.hash
                                    );
                                }
                                staged.push(unit_staged);
                                progress.copied(Instant::now());
                                if staged.len() >= PLACE_BATCH {
                                    copied.fetch_add(
                                        place_staged(std::mem::take(&mut staged), files),
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                            Err(error) => {
                                tracing::debug!(
                                    "did not seed {}-{}: {error}",
                                    unit.package,
                                    unit.hash
                                )
                            }
                        }
                    }
                    copied.fetch_add(place_staged(staged, files), Ordering::Relaxed);
                });
            }
        });
        let copied = copied.into_inner();
        if copied > 0 {
            return Seeded {
                units: copied,
                donor: Some(donor.workspace_root.clone()),
                left: units.len().saturating_sub(next.into_inner()),
                linked: files.linked.into_inner(),
                placed: files.placed.into_inner().unwrap_or_default(),
            };
        }
    }
    Seeded::default()
}

/// Copy the executable this unit's launcher needs before publishing the unit.
/// Each profile owns its inode, including pins inherited from older donors.
#[cfg(unix)]
fn pin_launcher(from: &Path, to: &Path, record: &Path, deadline: Instant) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    let record = match std::fs::read(record) {
        Ok(record) => record,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if Instant::now() >= deadline {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    let invalid =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid launcher pin path");
    let relative = record.split(|&byte| byte == 0).nth(1).ok_or_else(invalid)?;
    let relative = Path::new(OsStr::from_bytes(relative));
    let mut parts = relative.components();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (
            Some(Component::Normal(shims)),
            Some(Component::Normal(_)),
            Some(Component::Normal(name)),
            None,
        ) if shims == crate::build_script::SHIM_DIR && name == "kache" => {}
        _ => return Err(invalid()),
    }
    let pinned = to.join(relative);
    if std::fs::symlink_metadata(&pinned)
        .is_ok_and(|metadata| metadata.is_file() && metadata.nlink() == 1)
    {
        return Ok(());
    }
    let source = from.join(relative).canonicalize()?;
    let directory = pinned.parent().ok_or_else(invalid)?;
    std::fs::create_dir_all(directory)?;
    let temporary = tempfile::TempDir::new_in(directory)?;
    let staged = temporary.path().join("kache");
    if kache_fs::try_reflink(&source, &staged).is_err() {
        kache_fs::copy_writable(&source, &staged, true)?;
    }
    kache_fs::set_writable_permissions(&staged, true)?;
    std::fs::rename(staged, pinned)
}

/// How many of the most recently used donors are ranked by what they hold.
const RANKED_DONORS: usize = 8;

/// The donors that can give `target` units, in the order seeding tries them:
/// of the [`RANKED_DONORS`] most recently used, the one that can give the
/// most first (the more recent of two that give as many), then every other
/// in order of use. A build gets every unit of `packages` a donor holds; a
/// check (`check_lane`) only those [`check_units`] counts.
///
/// The checkout used last may have built a small part of the workspace: a
/// `cargo xtask` command builds the dependencies of one crate, 361 units of
/// a 1,900-unit workspace. Seeding stops at the first donor it copies from,
/// so taking the most recent one alone left a new checkout with those 361.
///
/// A check's ranked donor of the shared layout comes with what ranking read
/// of it ([`CheckRead`]), which seeding it uses again.
fn ranked_donors<'a>(
    target: &'a NewTarget,
    rustc_version: &'a str,
    donors: &'a [Donor],
    registry: &BTreeSet<String>,
    both: &BTreeSet<String>,
    checked: bool,
    check_lane: bool,
) -> impl Iterator<Item = (&'a Donor, Layout, Option<CheckRead>)> + 'a {
    let mut usable = donors.iter().filter_map(move |donor| {
        if donor.target_dir == target.target_dir
            || seed_lane(&donor.target_dir) != seed_lane(&target.target_dir)
            || !donor_built_by(donor, rustc_version)
        {
            return None;
        }
        layout(&donor.target_dir.join(PROFILE)).map(|layout| (donor, layout))
    });
    let candidates: Vec<_> = usable.by_ref().take(RANKED_DONORS).collect();
    // Each donor's fingerprints are read in a thread of its own: a check
    // reads every unit's, tens of milliseconds a donor.
    let mut ranked: Vec<_> = std::thread::scope(|scope| {
        let counts: Vec<_> = candidates
            .iter()
            .map(|&(donor, layout)| {
                scope.spawn(move || {
                    let profile = donor.target_dir.join(PROFILE);
                    let packages = if checked || same_checkout(donor, target) {
                        both
                    } else {
                        registry
                    };
                    if check_lane && layout == Layout::Shared {
                        let read = CheckRead::of(&profile, registry, packages);
                        (read.units(), Some(read))
                    } else {
                        (units_by_name(&profile, layout, packages).len(), None)
                    }
                })
            })
            .collect();
        candidates
            .iter()
            .zip(counts)
            .map(|(&(donor, layout), count)| {
                let (count, read) = count.join().unwrap_or((0, None));
                (donor, layout, count, read)
            })
            .collect()
    });
    // A target of the same checkout first: it gives every unit without
    // touching the checkout. Stable: equals keep their order of use.
    ranked.sort_by_key(|entry| (!same_checkout(entry.0, target), std::cmp::Reverse(entry.2)));
    ranked
        .into_iter()
        .map(|(donor, layout, _, read)| (donor, layout, read))
        .chain(usable.map(|(donor, layout)| (donor, layout, None)))
}

/// The units of `packages` in a donor's shared-layout `profile` a check
/// build is given ([`seed_for_lane`]): those a check of the donor's
/// workspace used ([`check_demand`]). A donor without a checked workspace
/// unit gives every unit, but a check uses only those it compiled itself,
/// which are metadata alone; only they count.
///
/// Ranked by every unit they hold, a workspace clippy in a new target was
/// seeded from a checkout's test-build target, whose checks had used 808 to
/// 1,000 registry units, while another checkout's workspace check, which
/// used 1,285, held fewer units in all.
#[cfg(test)]
fn check_units(
    profile: &Path,
    registry: &BTreeSet<String>,
    packages: &BTreeSet<String>,
) -> usize {
    CheckRead::of(profile, registry, packages).units()
}

/// What ranking reads of a shared-layout donor for a check: its outputs by
/// unit hash ([`outputs_by_hash`]), its units of the packages asked for, and
/// what its workspace check used ([`check_demand`]). The donor ranked first
/// is seeded from the same read, taken moments before: every file a unit's
/// copy takes is still read under the donor's lock, and a unit Cargo builds
/// meanwhile is left to the wrapper. Read a second time, the 2,188
/// fingerprints of a 600-crate workspace's donor were listed and parsed
/// twice before the first unit was copied.
struct CheckRead {
    deps: HashMap<String, Vec<PathBuf>>,
    held: Vec<Unit>,
    demand: Option<std::collections::HashSet<String>>,
}

impl CheckRead {
    fn of(profile: &Path, registry: &BTreeSet<String>, packages: &BTreeSet<String>) -> Self {
        let deps = outputs_by_hash(profile);
        let held = units_by_name(profile, Layout::Shared, packages);
        let demand = check_demand(profile, registry, &deps);
        Self { deps, held, demand }
    }

    /// How many units a check would be given ([`check_units`]).
    fn units(&self) -> usize {
        match &self.demand {
            Some(demand) => self
                .held
                .iter()
                .filter(|unit| demand.contains(&format!("{}-{}", unit.package, unit.hash)))
                .count(),
            None => self
                .held
                .iter()
                .filter(|unit| metadata_only(&self.deps, &unit.hash))
                .count(),
        }
    }
}

/// Are the unit's `deps/` outputs metadata alone (`.rmeta`, `.d`), as a
/// check writes them?
fn metadata_only(deps: &HashMap<String, Vec<PathBuf>>, hash: &str) -> bool {
    deps.get(hash).is_some_and(|files| {
        !files.is_empty()
            && files.iter().all(|file| {
                matches!(
                    file.extension().and_then(OsStr::to_str),
                    Some("rmeta" | "d")
                )
            })
    })
}

/// What Cargo writes to `CACHEDIR.TAG` in a target directory it creates.
const CARGO_CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55
# This file is a cache directory tag created by cargo.
# For information about cache directory tags see https://bford.info/cachedir/
";

/// Tag a target directory seeding created, as Cargo would have. Cargo tags
/// only a target directory it creates itself, so it never tags this one, and
/// kache neither tracks nor roots events in an untagged target directory.
fn tag_as_cargo_target(target_dir: &Path) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target_dir.join("CACHEDIR.TAG"))
        .and_then(|mut file| std::io::Write::write_all(&mut file, CARGO_CACHEDIR_TAG.as_bytes()));
}

/// Whether Cargo's cached `rustc -vV` answer in `target_dir` is `version`.
/// Whether `version` built `donor`. What kache recorded while building into
/// it decides; without a record, Cargo's rustc info cache does, which Cargo
/// does not always write (it cannot fingerprint every rustc).
fn donor_built_by(donor: &Donor, version: &str) -> bool {
    match &donor.rustc {
        Some(recorded) if is_rustc_version(recorded) => recorded.trim() == version.trim(),
        // Not a `rustc -vV`: before 1.2.1-tasty.4 a unit Clippy checked
        // recorded `clippy-driver`'s version (`clippy 0.1.99 (...)`), which
        // no probe ever matches. Cargo's own cache decides instead.
        _ => built_by(&donor.target_dir, version),
    }
}

/// Is `recorded` the output of `rustc -vV`, as a probe reports it?
fn is_rustc_version(recorded: &str) -> bool {
    recorded.trim_start().starts_with("rustc ")
}

fn built_by(target_dir: &Path, version: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(target_dir.join(".rustc_info.json")) else {
        return false;
    };
    let Ok(info) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    info.get("outputs")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|outputs| {
            outputs.values().any(|output| {
                output.get("stdout").and_then(serde_json::Value::as_str) == Some(version)
            })
        })
}

/// A Cargo lock seeding holds, released when dropped. Closing alone is not
/// enough: a child forked meanwhile keeps a duplicate descriptor until it
/// execs, and Cargo would wait for that.
struct Held(std::fs::File);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Lock `path` exclusively without waiting, creating it if needed. `None`
/// when another process holds it or it cannot be opened.
fn try_lock(path: &Path) -> Option<Held> {
    let file = open_lock(path)?;
    file.try_lock().ok()?;
    Some(Held(file))
}

/// Lock a donor's `path` for reading without waiting: Cargo building there
/// holds it exclusively and keeps seeding out, while other seedings, which
/// only read the donor, share it. An exclusive lock let one new checkout
/// at a time read a donor: of three started together from one donor, two
/// went unseeded and restored every unit one compiler call at a time.
fn try_lock_shared(path: &Path) -> Option<Held> {
    let file = open_lock(path)?;
    file.try_lock_shared().ok()?;
    Some(Held(file))
}

fn open_lock(path: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    paths.sort();
    paths
}

/// The directories in `dir`, sorted; a symbolic link is not one. Told apart
/// by the kind the listing gives each entry, where the filesystem records it
/// (ext4, XFS, btrfs): a stat of each entry was 2,188 calls a listing of a
/// large donor's `.fingerprint/`.
/// The entries of `dir`, sorted, each with its status, not following a link.
/// The status is asked of `dir`'s open descriptor by the entry's name, where
/// a stat of each entry's full path walked every directory above it again
/// (a unit's fingerprint directory in a target 9 levels deep). A directory
/// that cannot be listed has no entries, as for [`entries`]; an entry gone
/// before its status is read fails the listing.
fn entries_with_status(dir: &Path) -> std::io::Result<Vec<(PathBuf, std::fs::Metadata)>> {
    let Ok(listed) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut entries = listed
        .flatten()
        .map(|entry| Ok((entry.path(), entry.metadata()?)))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The unit directories (`<package>-<hash>`) a check build of the donor's
/// workspace reads, from Cargo's own fingerprints in the `profile` directory
/// of the shared layout: every unit reachable from a workspace unit Cargo
/// checked (one whose outputs are metadata only) through the `deps` of each
/// unit's fingerprint. A dependency is named there by its fingerprint hash,
/// the one the dependency's own hash file holds.
///
/// `None` when the donor holds no checked workspace unit or a fingerprint
/// cannot be read: then nothing says what a check needs. Only ever a filter
/// on what is copied: a unit left out is restored by the wrapper when Cargo
/// asks for it, as without seeding.
fn check_demand(
    profile: &Path,
    registry: &BTreeSet<String>,
    deps: &HashMap<String, Vec<PathBuf>>,
) -> Option<std::collections::HashSet<String>> {
    let mut by_fingerprint: HashMap<String, String> = HashMap::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots = Vec::new();
    for unit in subdirectories(&profile.join(".fingerprint")) {
        let name = file_name(&unit);
        let Some((package, hash)) = name.rsplit_once('-') else {
            continue;
        };
        let mut wants = Vec::new();
        for file in entries(&unit) {
            let file_name = file_name(&file);
            if file_name.ends_with(".json") {
                let text = std::fs::read_to_string(&file).ok()?;
                let json: serde_json::Value = serde_json::from_str(&text).ok()?;
                for dep in json.get("deps")?.as_array()? {
                    let fingerprint = dep.as_array()?.get(3)?.as_u64()?;
                    wants.push(hex_le(fingerprint));
                }
            } else if file_name != INVOCATION_STAMP
                && !file_name.starts_with("dep-")
                && !file_name.contains('.')
            {
                let fingerprint = std::fs::read_to_string(&file).ok()?;
                by_fingerprint.insert(fingerprint.trim().to_owned(), name.clone());
            }
        }
        if !registry.contains(package) && metadata_only(deps, hash) {
            roots.push(name.clone());
        }
        edges.insert(name, wants);
    }
    if roots.is_empty() {
        return None;
    }
    let mut reached = std::collections::HashSet::new();
    while let Some(unit) = roots.pop() {
        if !reached.insert(unit.clone()) {
            continue;
        }
        for fingerprint in edges.get(&unit).into_iter().flatten() {
            if let Some(dep) = by_fingerprint.get(fingerprint) {
                roots.push(dep.clone());
            }
        }
    }
    Some(reached)
}

/// A fingerprint as Cargo's hash files spell it: the little-endian bytes of
/// the `u64` in hex.
fn hex_le(fingerprint: u64) -> String {
    fingerprint
        .to_le_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Does the Cargo that sent this probe only check? `cargo clippy` runs
/// `cargo check` with `CLIPPY_ARGS` set; a `cargo check` is the subcommand of
/// the parent process. Only narrows what is seeded, so a wrong answer costs
/// time, never a wrong build.
fn probe_is_check_lane(env: &crate::invocation::InvocationEnv) -> bool {
    if env.var_os("CLIPPY_ARGS").is_some() {
        return true;
    }
    #[cfg(target_os = "linux")]
    if let Some(ppid) = crate::invocation::cargo_pid()
        && let Ok(cmdline) = std::fs::read(format!("/proc/{ppid}/cmdline"))
    {
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        return matches!(cargo_subcommand(&args), Some("check" | "clippy"));
    }
    false
}

/// The subcommand of a Cargo command line: the first argument after the
/// program that is neither an option nor a `+toolchain`.
#[cfg(any(target_os = "linux", test))]
fn cargo_subcommand(args: &[String]) -> Option<&str> {
    args.iter()
        .skip(1)
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-') && !arg.starts_with('+'))
}

/// Pass Cargo's explicit target selection to its compiler wrappers.
pub(crate) fn configure_cargo_seed(
    command: &mut std::process::Command,
    args: &[std::ffi::OsString],
    isolated_build_dir: bool,
) {
    placement::configure(command, args, isolated_build_dir);
}

/// Whether this rustc invocation should ask for a seed: seeding is on and
/// it is Cargo's target-info probe, which runs before any unit compiles.
fn asks_for_seed(enabled: bool, args: &[String]) -> bool {
    enabled && is_target_info_probe(args)
}

/// Does `args` name a target platform (`--target <triple>` or
/// `--target=<triple>`)?
fn names_a_target(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--target" || arg.starts_with("--target="))
}

/// Does the build this probe is for name a target platform? Cargo then
/// builds that platform's units under `<target>/<triple>/` and only the
/// units for the host (build scripts, proc macros) under `debug/`, which is
/// all seeding fills. A cross check of a web or Windows build used none of
/// the thousands of host units seeding copied from a donor (687 of 687
/// units restored with and without them, 2026-10-18), and the copy took the
/// whole [`SEED_DEADLINE`] before Cargo started. Read from the probe's own
/// arguments (Cargo probes each platform), `CARGO_BUILD_TARGET` and, on
/// Linux, the Cargo command line that sent the probe. Only skips a seeding,
/// so a wrong answer costs time, never a wrong build.
fn build_names_a_target(args: &[String], env: &crate::invocation::InvocationEnv) -> bool {
    if names_a_target(args)
        || env
            .var_os("CARGO_BUILD_TARGET")
            .is_some_and(|target| !target.is_empty())
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    if let Some(ppid) = crate::invocation::cargo_pid()
        && let Ok(cmdline) = std::fs::read(format!("/proc/{ppid}/cmdline"))
    {
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        return names_a_target(&args);
    }
    false
}

/// The wrapper's side: when `args` is the probe for a target directory Cargo
/// has not built, ask the daemon to seed it and wait for the answer.
///
/// See [`asks_for_seed`] for which invocations qualify.
pub(crate) fn before_probe_for(
    config: &crate::config::Config,
    args: &[String],
    env: &crate::invocation::InvocationEnv,
) {
    if !asks_for_seed(config.seed_new_targets, args) || build_names_a_target(args, env) {
        return;
    }
    let Some(cwd) = env.cwd() else {
        return;
    };
    let Some(workspace) = workspace_root(cwd) else {
        return;
    };
    let target_env = placement::override_target(env)
        .or_else(|| env.var_os("CARGO_TARGET_DIR"))
        .or_else(|| env.var_os("CARGO_BUILD_TARGET_DIR"));
    let build_env = placement::override_build(env).or_else(|| env.var_os("CARGO_BUILD_BUILD_DIR"));
    let Some(cargo_home) = env
        .var_os("CARGO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env.home_dir().map(|home| home.join(".cargo")))
        .and_then(|path| env.resolve(&path))
    else {
        return;
    };
    let Some(directory) = placement::resolve(
        cwd,
        workspace,
        &cargo_home,
        target_env.as_deref(),
        build_env.as_deref(),
    ) else {
        return;
    };
    let Some(target) = new_target(cwd, Some(directory.as_os_str()), false) else {
        return;
    };
    let Some(version) = rustc_version_for(args.first().map(String::as_str), env) else {
        return;
    };
    let note = env.var_os("KACHE_LOG_FILE").is_none();
    if let Err(error) =
        crate::daemon::seed_new_target(config, &target, &version, probe_is_check_lane(env), note)
    {
        tracing::debug!("not seeding {}: {error:#}", target.target_dir.display());
    }
}

/// Append `line` to kache's diagnostic log as its file layer writes an info
/// event. A wrapper has no file layer unless `KACHE_LOG_FILE` asks for one
/// (Cargo runs hundreds of wrappers a build), but a seeding happens once per
/// new target directory and leaves no other trace of what it copied.
pub(crate) fn note_in_log(line: &str) {
    note_in_log_for(&crate::invocation::InvocationEnv::capture(), line);
}

/// [`note_in_log`] into the file the log's file layer would write for `env`:
/// `KACHE_LOG_FILE_PATH` when set, not the default log beside the default
/// cache folder.
fn note_in_log_for(env: &crate::invocation::InvocationEnv, line: &str) {
    use std::io::Write;
    let Some(path) = crate::log_file_path_for(env) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let _ = writeln!(file, "{now}  INFO kache::target_seed: {line}");
    }
}

/// `rustc -vV` from the compiler Cargo is probing.
#[cfg(test)]
fn rustc_version(rustc: Option<&str>) -> Option<String> {
    rustc_version_for(rustc, &crate::invocation::InvocationEnv::capture())
}

fn rustc_version_for(
    rustc: Option<&str>,
    env: &crate::invocation::InvocationEnv,
) -> Option<String> {
    let output = env
        .command(rustc?)
        .ok()?
        .arg("-vV")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .invocation_output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {

    #[cfg(unix)]
    #[test]
    fn a_placed_file_earns_memo_rows_only_while_it_holds_the_donors_bytes() {
        use kache_store::file_hash::{FileFingerprint, stamp_clock_ns};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let donor = dir.path().join("donor.rlib");
        std::fs::write(&donor, vec![3u8; 70_000]).unwrap();
        std::fs::set_permissions(&donor, std::fs::Permissions::from_mode(0o444)).unwrap();
        let placed = |at: &std::path::Path| PlacedFile {
            from: donor.clone(),
            before: FileFingerprint::from_path(&donor).unwrap(),
            at: at.to_path_buf(),
        };
        let since = stamp_clock_ns();

        let linked = dir.path().join("linked.rlib");
        let file = placed(&linked);
        std::fs::hard_link(&donor, &linked).unwrap();
        let rows = file.memo_rows(since);
        assert_eq!(rows.len(), 2, "the link and the donor: {rows:?}");
        assert!(rows.iter().all(|row| row.ctime_ns >= file.before.ctime_ns));

        let copy = |to: &std::path::Path, mtime_ns: i64| {
            std::fs::copy(&donor, to).unwrap();
            filetime::set_file_mtime(
                to,
                filetime::FileTime::from_unix_time(
                    mtime_ns.div_euclid(1_000_000_000),
                    mtime_ns.rem_euclid(1_000_000_000) as u32,
                ),
            )
            .unwrap();
        };
        let copied = dir.path().join("copied.rlib");
        let file = placed(&copied);
        copy(&copied, file.before.mtime_ns);
        assert_eq!(file.memo_rows(since).len(), 1, "the copy only");

        let late = dir.path().join("late.rlib");
        let file = placed(&late);
        copy(&late, file.before.mtime_ns);
        std::fs::set_permissions(&donor, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&donor, vec![4u8; 70_000]).unwrap();
        assert!(
            file.memo_rows(since).is_empty(),
            "the donor changed while it was copied"
        );
    }
    use super::*;

    const HASH: &str = "0123456789abcdef";
    const OTHER: &str = "fedcba9876543210";
    const VERSION: &str = "rustc 1.0.0 (test)\nhost: test\n";
    /// A cache directory seeding tests without hermetic runs pass. Its store
    /// holds the fixtures' `rlib`, as kache stores what it compiles.
    fn no_runs() -> tempfile::TempDir {
        let cache = tempfile::tempdir().unwrap();
        cache_storing(cache.path(), &["rlib"]);
        cache
    }

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const LOCK: &str = r#"
[[package]]
name = "app"
version = "0.1.0"

[[package]]
name = "dep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "gitdep"
version = "0.1.0"
source = "git+https://example.com/gitdep#abc"
"#;

    /// A checkout at `root/name` with `LOCK`, and a donor target built by
    /// `VERSION` in `layout` holding units of `dep` and of `app`.
    fn donor(root: &Path, name: &str, layout: Layout) -> Donor {
        let workspace_root = root.join(name);
        write(
            &workspace_root.join("Cargo.toml"),
            "[package]\nname = \"app\"\n",
        );
        write(&workspace_root.join("Cargo.lock"), LOCK);
        let target_dir = workspace_root.join("target");
        let info = serde_json::json!({"outputs": {"1": {"stdout": VERSION}}});
        write(&target_dir.join(".rustc_info.json"), &info.to_string());
        let profile = target_dir.join(PROFILE);
        for (package, hash) in [("dep", HASH), ("app", OTHER)] {
            match layout {
                Layout::PerUnit => {
                    let unit = profile.join("build").join(package).join(hash);
                    write(&unit.join("fingerprint/lib"), "fingerprint");
                    write(&unit.join("out/lib.rlib"), "rlib");
                }
                Layout::Shared => {
                    let named = format!("{package}-{hash}");
                    write(&profile.join(".fingerprint").join(&named).join("lib"), "fp");
                    write(
                        &profile
                            .join("deps")
                            .join(format!("lib{package}-{hash}.rlib")),
                        "rlib",
                    );
                    write(
                        &profile.join("deps").join(format!("{package}-{hash}.d")),
                        &format!(
                            "{}: registry.rs\n",
                            profile
                                .join("deps")
                                .join(format!("lib{package}-{hash}.rlib"))
                                .to_str()
                                .unwrap()
                                .replace(' ', "\\ ")
                        ),
                    );
                    write(
                        &profile.join("build").join(&named).join("output"),
                        "cargo:rustc-cfg=x\n",
                    );
                    write(
                        &profile.join("build").join(&named).join("root-output"),
                        profile
                            .join("build")
                            .join(&named)
                            .join("out")
                            .to_str()
                            .unwrap(),
                    );
                }
            }
        }
        Donor {
            target_dir,
            workspace_root,
            rustc: None,
        }
    }

    fn checkout(root: &Path, name: &str) -> NewTarget {
        let workspace_root = root.join(name);
        write(
            &workspace_root.join("Cargo.toml"),
            "[package]\nname = \"app\"\n",
        );
        write(&workspace_root.join("Cargo.lock"), LOCK);
        NewTarget {
            target_dir: workspace_root.join("target"),
            workspace_root,
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// kache's own record of a donor's compiler decides, trimmed; Cargo's
    /// rustc info cache is the fallback when there is no record.
    #[test]
    fn a_donors_recorded_compiler_decides_before_cargos_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorded = donor(dir.path(), "recorded", Layout::Shared);
        std::fs::remove_file(recorded.target_dir.join(".rustc_info.json")).unwrap();
        assert!(!donor_built_by(&recorded, VERSION), "no record, no cache");
        recorded.rustc = Some(VERSION.trim().to_string());
        assert!(donor_built_by(&recorded, VERSION));
        recorded.rustc = Some("rustc 2.0.0".into());
        assert!(!donor_built_by(&recorded, VERSION));

        let cached = donor(dir.path(), "cached", Layout::Shared);
        assert!(
            donor_built_by(&cached, VERSION),
            "Cargo's cache, without a record"
        );
        let mut contradicted = cached.clone();
        contradicted.rustc = Some("rustc 2.0.0".into());
        assert!(
            !donor_built_by(&contradicted, VERSION),
            "a record overrides Cargo's cache"
        );
    }

    /// A target whose last unit Clippy checked recorded the driver's version
    /// before 1.2.1-tasty.4. That is no `rustc -vV`, so Cargo's cache decides:
    /// the target still seeds a checkout built by the same rustc.
    #[test]
    fn a_donor_recorded_by_clippy_falls_back_to_cargos_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut clippy = donor(dir.path(), "clippy", Layout::Shared);
        clippy.rustc = Some("clippy 0.1.99 (58cf8f9c35 2026-10-13)".into());
        assert!(donor_built_by(&clippy, VERSION), "Cargo's cache decides");
        assert!(!donor_built_by(&clippy, "rustc 2.0.0\n"));
        std::fs::remove_file(clippy.target_dir.join(".rustc_info.json")).unwrap();
        assert!(!donor_built_by(&clippy, VERSION), "no cache, no seed");

        let target = checkout(dir.path(), "new");
        let seeded = seed(
            &target,
            VERSION,
            &[donor(dir.path(), "built", Layout::Shared)],
            no_runs().path(),
            later(),
        );
        assert!(seeded.donor.is_some(), "the fixture seeds");
        let mut by_clippy = donor(dir.path(), "checked", Layout::Shared);
        by_clippy.rustc = Some("clippy 0.1.99 (58cf8f9c35 2026-10-13)".into());
        let target = checkout(dir.path(), "after-clippy");
        let seeded = seed(&target, VERSION, &[by_clippy], no_runs().path(), later());
        assert!(seeded.donor.is_some(), "a donor Clippy checked last seeds");
    }

    #[test]
    fn only_the_target_info_probe_asks_for_a_seed_and_only_when_enabled() {
        let probe = args("rustc - --crate-name ___ --print=file-names --crate-type bin");
        assert!(asks_for_seed(true, &probe));
        assert!(!asks_for_seed(false, &probe));
        assert!(!asks_for_seed(true, &args("rustc -vV")));
    }

    /// A build for a named platform is not seeded: its units are not the
    /// host units seeding copies.
    #[test]
    fn a_probe_naming_a_target_is_a_cross_build() {
        assert!(names_a_target(&args(
            "rustc - --crate-name ___ --print=file-names --target wasm32-unknown-unknown"
        )));
        assert!(names_a_target(&args(
            "cargo +nightly check -p web --target=wasm32-unknown-unknown"
        )));
        assert!(!names_a_target(&args(
            "rustc - --crate-name ___ --print=file-names --crate-type bin"
        )));
        assert!(!names_a_target(&args(
            "cargo clippy --workspace --all-targets"
        )));
        assert!(!names_a_target(&args(
            "cargo build --target-dir target/other"
        )));
    }

    #[test]
    fn a_per_unit_donor_gives_only_hash_named_units_with_a_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join(PROFILE);
        let build = profile.join("build").join("dep");
        write(&build.join(HASH).join("fingerprint/lib"), "fp");
        write(&build.join(OTHER).join("out/lib.rlib"), "no fingerprint");
        write(&build.join("not-a-hash/fingerprint/lib"), "fp");
        let packages: BTreeSet<String> = ["dep".to_string()].into();
        let found: Vec<(String, String)> = units(&profile, Layout::PerUnit, &packages)
            .into_iter()
            .map(|unit| (unit.package, unit.hash))
            .collect();
        assert_eq!(found, [("dep".to_string(), HASH.to_string())]);
    }

    /// A seeding cut short by its deadline copies a prefix of the units.
    /// A unit whose dependency was left out is rebuilt anyway, so units go
    /// in the order the donor built them, dependencies first: Cargo writes
    /// a unit's fingerprint only once the unit is built. In the donor of a
    /// 1,075-unit workspace, a cut after 458 units in name order left 242
    /// of them waiting on a dependency that was not copied; in build order,
    /// none.
    #[test]
    fn units_come_in_the_order_the_donor_built_them() {
        for layout in [Layout::Shared, Layout::PerUnit] {
            let dir = tempfile::tempdir().unwrap();
            let profile = dir.path().join(PROFILE);
            let fingerprint = |package: &str, hash: &str| match layout {
                Layout::Shared => profile
                    .join(".fingerprint")
                    .join(format!("{package}-{hash}")),
                Layout::PerUnit => profile
                    .join("build")
                    .join(package)
                    .join(hash)
                    .join("fingerprint"),
            };
            // Built in the order zeta, alpha, mid: neither the names' order
            // nor the hashes'.
            let built = [
                ("zeta", OTHER, 100),
                ("alpha", HASH, 200),
                ("mid", OTHER, 300),
            ];
            for (package, hash, _) in built {
                write(&fingerprint(package, hash).join("lib"), "fp");
            }
            for (package, hash, offset) in built {
                let at = filetime::FileTime::from_unix_time(1_700_000_000 + offset, 0);
                filetime::set_file_mtime(fingerprint(package, hash), at).unwrap();
                if layout == Layout::PerUnit {
                    // The unit's own directory changes at other times.
                    let early = filetime::FileTime::from_unix_time(1_600_000_000 - offset, 0);
                    filetime::set_file_mtime(marker(&profile, layout, &unit(package, hash)), early)
                        .unwrap();
                }
            }
            let packages: BTreeSet<String> = ["alpha", "mid", "zeta"].map(str::to_string).into();
            let found: Vec<String> = units(&profile, layout, &packages)
                .into_iter()
                .map(|unit| unit.package)
                .collect();
            assert_eq!(found, ["zeta", "alpha", "mid"], "{layout:?}");
        }
    }

    #[test]
    fn recognizes_cargos_target_info_probe() {
        let rustc = "rustc - --crate-name ___ --print=file-names --crate-type bin";
        assert!(is_target_info_probe(&args(rustc)));
        assert!(is_target_info_probe(&args(
            "rustc - --crate-name ___ --print file-names"
        )));
        for other in [
            "rustc -vV",
            "rustc - --crate-name app --print=file-names",
            "rustc - --crate-name ___ --print=cfg",
            "rustc - --crate-name ___ --print cfg",
            "rustc - --crate-name ___",
            "rustc --print=file-names ___",
            "rustc - --print file-names --crate-name",
        ] {
            assert!(!is_target_info_probe(&args(other)), "{other}");
        }
    }

    #[test]
    fn finds_the_new_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = checkout(dir.path(), "b");
        let member = checkout.workspace_root.join("crates/member");
        std::fs::create_dir_all(&member).unwrap();
        write(&member.join("Cargo.toml"), "");
        assert_eq!(new_target(&member, None, false), Some(checkout.clone()));
        assert_eq!(
            workspace_root(&member),
            Some(checkout.workspace_root.as_path())
        );
        let lock_only = checkout.workspace_root.join("lock-only");
        write(&lock_only.join("Cargo.lock"), LOCK);
        assert_eq!(
            workspace_root(&lock_only),
            Some(checkout.workspace_root.as_path())
        );
        assert_eq!(new_target(&checkout.workspace_root, None, true), None);
        let env = std::ffi::OsString::from("elsewhere/t");
        assert_eq!(
            new_target(&member, Some(&env), false).unwrap().target_dir,
            member.join("elsewhere/t")
        );
        let empty = std::ffi::OsString::new();
        assert_eq!(
            new_target(&member, Some(&empty), false),
            Some(checkout.clone())
        );
        // No lockfile anywhere up: nothing to seed from.
        assert_eq!(new_target(dir.path(), None, false), None);
        // Cargo has built there already.
        for built in [".rustc_info.json", "debug/.fingerprint", "debug/build"] {
            let target = checkout.target_dir.join(built);
            std::fs::create_dir_all(&target).unwrap();
            assert_eq!(new_target(&member, None, false), None, "{built}");
            std::fs::remove_dir_all(&checkout.target_dir).unwrap();
        }
        assert!(new_target(&member, None, false).is_some());
    }

    #[test]
    fn copies_only_packages_with_a_source() {
        let packages = registry_packages(LOCK);
        assert_eq!(
            packages.into_iter().collect::<Vec<_>>(),
            vec!["dep".to_owned(), "gitdep".to_owned()]
        );
        // A path package sharing a name keeps that name out.
        let shadowed = format!("{LOCK}\n[[package]]\nname = \"dep\"\nversion = \"2.0.0\"\n");
        assert_eq!(
            registry_packages(&shadowed).into_iter().collect::<Vec<_>>(),
            vec!["gitdep".to_owned()]
        );
        assert!(registry_packages("not toml [").is_empty());
        assert!(registry_packages("version = 3\n").is_empty());
        assert!(registry_packages("[[package]]\nsource = \"x\"\n").is_empty());
    }

    #[test]
    fn a_recent_donor_holding_more_units_is_used_before_the_last_one() {
        let dir = tempfile::tempdir().unwrap();
        let more = donor(dir.path(), "more", Layout::Shared);
        let named = format!("gitdep-{OTHER}");
        let profile = more.target_dir.join(PROFILE);
        write(&profile.join(".fingerprint").join(&named).join("lib"), "fp");
        write(
            &profile.join("deps").join(format!("libgitdep-{OTHER}.rlib")),
            "rlib",
        );
        let last = donor(dir.path(), "last", Layout::Shared);
        let new = checkout(dir.path(), "b");
        // Most recently used first, as the store lists them.
        let donors = [last.clone(), more.clone()];
        assert_eq!(
            seed(&new, VERSION, &donors, no_runs().path(), later()),
            Seeded {
                units: 2,
                donor: Some(more.workspace_root.clone()),
                ..Seeded::default()
            }
        );
        // Holding as many, the one used last goes first.
        let other = checkout(dir.path(), "c");
        let even = [last.clone(), donor(dir.path(), "even", Layout::Shared)];
        assert_eq!(
            seed(&other, VERSION, &even, no_runs().path(), later()).donor,
            Some(last.workspace_root.clone())
        );
    }

    #[test]
    fn seeds_editor_and_check_targets_only_from_the_matching_lane() {
        let dir = tempfile::tempdir().unwrap();
        for lane in ["check", "rust-analyzer"] {
            for layout in [Layout::Shared, Layout::PerUnit] {
                let label = format!("{lane}-{layout:?}");
                let wrong = donor(dir.path(), &format!("wrong-{label}"), layout);
                let mut matching = donor(dir.path(), &format!("matching-{label}"), layout);
                let old = matching.target_dir.clone();
                matching.target_dir = matching.workspace_root.join("elsewhere").join(lane);
                std::fs::create_dir_all(matching.target_dir.parent().unwrap()).unwrap();
                std::fs::rename(&old, &matching.target_dir).unwrap();
                if layout == Layout::Shared {
                    // Model compiler outputs built in this lane. Renaming the
                    // fixture alone leaves its dep-info naming the old target.
                    let profile = matching.target_dir.join(PROFILE);
                    for (package, hash) in [("dep", HASH), ("app", OTHER)] {
                        write(
                            &profile.join("deps").join(format!("{package}-{hash}.d")),
                            &format!(
                                "{}: registry.rs\n",
                                profile
                                    .join("deps")
                                    .join(format!("lib{package}-{hash}.rlib"))
                                    .to_str()
                                    .unwrap()
                                    .replace(' ', "\\ ")
                            ),
                        );
                    }
                }
                let mut target = checkout(dir.path(), &format!("new-{label}"));
                target.target_dir = target.target_dir.join(lane);
                let result = seed(
                    &target,
                    VERSION,
                    &[wrong, matching.clone()],
                    no_runs().path(),
                    later(),
                );
                assert_eq!(result.donor, Some(matching.workspace_root.clone()));
                assert_eq!(result.units, 1);
                assert!(
                    marker(
                        &target.target_dir.join(PROFILE),
                        layout,
                        &Unit {
                            package: "dep".into(),
                            hash: HASH.into()
                        }
                    )
                    .is_dir()
                );
                assert!(!target.workspace_root.join("target/debug").exists());
                let regular = checkout(dir.path(), &format!("regular-{label}"));
                assert_eq!(
                    seed(&regular, VERSION, &[matching], no_runs().path(), later()),
                    Seeded::default()
                );
            }
        }
    }

    #[cfg(unix)]
    fn launcher_record(profile: &Path, layout: Layout, relative: &str) -> PathBuf {
        let record = match layout {
            Layout::PerUnit => profile.join("build/dep/0123456789abcdef/out/.kache-launch"),
            Layout::Shared => profile.join("build/dep-0123456789abcdef/.kache-launch"),
        };
        let to_profile = match layout {
            Layout::PerUnit => "../../../../",
            Layout::Shared => "../../",
        };
        write(
            &record,
            &format!("build-script-build.kache-real\0{relative}\0{to_profile}\0"),
        );
        record
    }

    #[cfg(unix)]
    #[test]
    fn a_seed_brings_the_kache_its_launchers_run() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let shims = crate::build_script::SHIM_DIR;
            let profile = donor.target_dir.join(PROFILE);
            let script_directory = match layout {
                Layout::PerUnit => profile.join("build/dep/0123456789abcdef/out"),
                Layout::Shared => profile.join("build/dep-0123456789abcdef"),
            };
            std::fs::create_dir_all(&script_directory).unwrap();
            let script = script_directory.join("build_script_build");
            kache_fs::testutil::write_executable(
                &script,
                "#!/bin/sh\nprintf 'uncached fallback\\n'\n",
            );
            crate::build_script::install(&script).unwrap();
            let record = std::fs::read(script_directory.join(".kache-launch")).unwrap();
            let relative =
                std::ffi::OsStr::from_bytes(record.split(|&byte| byte == 0).nth(1).unwrap());
            let relative = Path::new(relative).to_path_buf();
            let pinned = profile.join(&relative);
            // Replace the installer's pin without writing through its inode.
            std::fs::remove_file(&pinned).unwrap();
            let global = dir.path().join("kache");
            kache_fs::testutil::write_executable(&global, "#!/bin/sh\nprintf 'seeded pin\\n'\n");
            std::fs::set_permissions(&global, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::hard_link(&global, &pinned).unwrap();
            write(&profile.join(shims).join("unused/kache"), "unused version");
            let cache = dir.path().join("cache");
            let hash = blake3::hash(b"rlib").to_hex().to_string();
            write(
                &kache_store::blob_path_in_store_dir(&cache.join("store"), &hash),
                "rlib",
            );
            let new = checkout(dir.path(), "b");
            let seeded = seed(&new, VERSION, std::slice::from_ref(&donor), &cache, later());
            assert_eq!(seeded.units, 1, "{layout:?}");
            let shim = new.target_dir.join(PROFILE).join(shims);
            let copied = new.target_dir.join(PROFILE).join(&relative);
            assert_eq!(
                std::fs::read(&copied).unwrap(),
                std::fs::read(&global).unwrap()
            );
            assert_eq!(entries(copied.parent().unwrap()).len(), 1, "{layout:?}");
            assert!(!shim.join("unused").exists());
            assert_eq!(
                std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::metadata(&global).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_ne!(
                kache_fs::file_identity(&copied).unwrap(),
                kache_fs::file_identity(&pinned).unwrap(),
                "{layout:?}: target profiles own distinct executable inodes"
            );
            let seeded_script = new
                .target_dir
                .join(PROFILE)
                .join(script.strip_prefix(&profile).unwrap());
            std::fs::remove_dir_all(&donor.target_dir).unwrap();
            std::fs::remove_file(global).unwrap();
            let output = Command::new(seeded_script).invocation_output().unwrap();
            assert!(output.status.success(), "{layout:?}");
            assert_eq!(output.stdout, b"seeded pin\n", "{layout:?}");
        }
    }

    #[test]
    fn a_seed_that_copies_nothing_pins_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let donor = donor(dir.path(), "a", Layout::PerUnit);
        let shims = crate::build_script::SHIM_DIR;
        write(
            &donor.target_dir.join(PROFILE).join(shims).join("pin/kache"),
            "kache",
        );
        let new = checkout(dir.path(), "b");
        let seeded = seed(
            &new,
            "rustc 2.0.0 (other)\n",
            std::slice::from_ref(&donor),
            Path::new("/nonexistent/kache-cache"),
            later(),
        );
        assert_eq!(seeded, Seeded::default());
        assert!(!new.target_dir.join(PROFILE).join(shims).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_pin_already_in_place_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        let shims = crate::build_script::SHIM_DIR;
        write(&from.join(shims).join("pin/kache"), "donor");
        write(&to.join(shims).join("pin/kache"), "ours");
        let record = launcher_record(&from, Layout::Shared, &format!("{shims}/pin/kache"));
        let original = kache_fs::file_identity(&to.join(shims).join("pin/kache")).unwrap();
        pin_launcher(&from, &to, &record, later()).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join(shims).join("pin/kache")).unwrap(),
            "ours"
        );
        assert_eq!(
            kache_fs::file_identity(&to.join(shims).join("pin/kache")).unwrap(),
            original
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_seeded_shared_pin_is_detached_and_a_symlink_donor_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        let relative = format!("{}/pin/kache", crate::build_script::SHIM_DIR);
        let source = from.join(&relative);
        let copied = to.join(&relative);
        write(&source, "donor executable");
        std::fs::create_dir_all(copied.parent().unwrap()).unwrap();
        std::fs::hard_link(&source, &copied).unwrap();
        let record = launcher_record(&from, Layout::Shared, &relative);
        pin_launcher(&from, &to, &record, later()).unwrap();
        assert_ne!(
            kache_fs::file_identity(&source).unwrap(),
            kache_fs::file_identity(&copied).unwrap()
        );
        assert_eq!(std::fs::read(&copied).unwrap(), b"donor executable");
        std::fs::remove_file(&source).unwrap();
        write(&source.with_file_name("real"), "symlink executable");
        std::os::unix::fs::symlink("real", &source).unwrap();
        std::fs::remove_file(&copied).unwrap();
        pin_launcher(&from, &to, &record, later()).unwrap();
        assert!(std::fs::symlink_metadata(&copied).unwrap().is_file());
        assert_eq!(std::fs::read(copied).unwrap(), b"symlink executable");
    }

    #[cfg(unix)]
    #[test]
    fn pin_copy_obeys_the_deadline_and_rejects_invalid_records() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        let relative = format!("{}/pin/kache", crate::build_script::SHIM_DIR);
        write(&from.join(&relative), "kache");
        let record = launcher_record(&from, Layout::Shared, &relative);
        assert_eq!(
            pin_launcher(&from, &to, &record, Instant::now())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(!to.exists());
        for relative in [
            "",
            "../outside/kache",
            "/tmp/pin/kache",
            ".kache-build-script-shims/pin/other",
            ".kache-build-script-shims/pin/kache/extra",
            "other/pin/kache",
        ] {
            let record = launcher_record(&from, Layout::Shared, relative);
            assert_eq!(
                pin_launcher(&from, &to, &record, later())
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidData,
                "{relative}"
            );
            assert!(!to.exists());
        }
        std::fs::write(&record, "no fields").unwrap();
        assert_eq!(
            pin_launcher(&from, &to, &record, later())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        std::fs::remove_file(&record).unwrap();
        pin_launcher(&from, &to, &record, later()).unwrap();
        assert!(!to.exists());
        std::fs::create_dir(&record).unwrap();
        assert!(pin_launcher(&from, &to, &record, later()).is_err());
    }

    #[test]
    fn a_cancelled_request_does_not_seed_and_another_can_reuse_the_donor() {
        use crate::invocation::process::{Cancellation, CancellationScope};

        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "donor", layout);
            let cache = no_runs();
            let cancelled_target = checkout(dir.path(), "cancelled");
            let cancelled = Cancellation::default();
            cancelled.cancel();
            {
                let _scope = CancellationScope::enter(Some(cancelled));
                assert_eq!(
                    seed(
                        &cancelled_target,
                        VERSION,
                        std::slice::from_ref(&donor),
                        cache.path(),
                        later(),
                    ),
                    Seeded::default(),
                    "{layout:?}"
                );
            }
            assert!(
                !marker(
                    &cancelled_target.target_dir.join(PROFILE),
                    layout,
                    &unit("dep", HASH)
                )
                .exists(),
                "{layout:?}"
            );
            let healthy_target = checkout(dir.path(), "healthy");
            {
                let _scope = CancellationScope::enter(Some(Cancellation::default()));
                let seeded = seed(
                    &healthy_target,
                    VERSION,
                    std::slice::from_ref(&donor),
                    cache.path(),
                    later(),
                );
                assert_eq!(seeded.units, 1, "{layout:?}");
                assert_eq!(seeded.donor, Some(donor.workspace_root.clone()));
            }
            assert!(
                marker(
                    &healthy_target.target_dir.join(PROFILE),
                    layout,
                    &unit("dep", HASH)
                )
                .is_dir(),
                "{layout:?}"
            );
        }
    }

    #[test]
    fn seeds_both_layouts_and_keeps_modification_times() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
            let marker_from = marker(&from, layout, &unit("dep", HASH));
            // Cargo's invocation stamps stay behind; a file of that name a
            // build script wrote to `out/` is copied.
            let stamps = match layout {
                Layout::PerUnit => vec![
                    marker_from.join("fingerprint").join(INVOCATION_STAMP),
                    marker_from.join("run").join(INVOCATION_STAMP),
                    marker_from.join("out").join(INVOCATION_STAMP),
                ],
                Layout::Shared => {
                    let run = from.join("build").join(format!("dep-{HASH}"));
                    vec![
                        marker_from.join(INVOCATION_STAMP),
                        run.join(INVOCATION_STAMP),
                        run.join("out").join(INVOCATION_STAMP),
                    ]
                }
            };
            for stamp in &stamps {
                write(stamp, "This file has an mtime of when this was started.");
            }
            for file in walk(&from) {
                filetime::set_file_mtime(&file, old).unwrap();
            }
            let new = checkout(dir.path(), "b");
            let seeded = seed(
                &new,
                VERSION,
                std::slice::from_ref(&donor),
                no_runs().path(),
                later(),
            );
            assert_eq!(
                seeded,
                Seeded {
                    units: 1,
                    donor: Some(donor.workspace_root.clone()),
                    ..Seeded::default()
                },
                "{layout:?}"
            );
            let to = new.target_dir.join(PROFILE);
            assert!(
                marker(&to, layout, &unit("dep", HASH)).is_dir(),
                "{layout:?}"
            );
            // The path package is never copied.
            assert!(
                !marker(&to, layout, &unit("app", OTHER)).exists(),
                "{layout:?}"
            );
            let copied: Vec<_> = walk(&to)
                .into_iter()
                .filter(|file| !file.ends_with(".cargo-lock"))
                .collect();
            let expected = match layout {
                Layout::PerUnit => 3,
                Layout::Shared => 6,
            };
            assert_eq!(copied.len(), expected, "{layout:?}: {copied:?}");
            let stamps: Vec<_> = copied
                .iter()
                .filter(|file| file.ends_with(INVOCATION_STAMP))
                .collect();
            assert!(
                stamps.len() == 1 && stamps[0].parent().is_some_and(|dir| dir.ends_with("out")),
                "{layout:?}: only out/'s file of the stamp's name is copied: {stamps:?}"
            );
            for file in &copied {
                let mtime = filetime::FileTime::from_last_modification_time(
                    &std::fs::metadata(file).unwrap(),
                );
                assert_eq!(mtime, old, "{}", file.display());
                assert!(!file.to_string_lossy().contains(STAGING));
            }
            assert!(marker_from.is_dir(), "the donor keeps its units");
            // A second seeding finds the unit present and copies nothing.
            assert_eq!(
                seed(
                    &new,
                    VERSION,
                    std::slice::from_ref(&donor),
                    no_runs().path(),
                    later()
                ),
                Seeded::default()
            );
        }
    }

    /// A run's files take older times only in a checkout with no other
    /// target, which could take them for fresh.
    #[test]
    fn other_targets_of_the_checkout_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let target = NewTarget {
            target_dir: root.join("target"),
            workspace_root: root.clone(),
        };
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        assert!(!checkout_has_other_targets(&target, &[]));
        let elsewhere = Donor {
            target_dir: dir.path().join("other/target"),
            workspace_root: dir.path().join("other"),
            rustc: None,
        };
        std::fs::create_dir_all(&elsewhere.target_dir).unwrap();
        assert!(!checkout_has_other_targets(
            &target,
            std::slice::from_ref(&elsewhere)
        ));
        let tracked = Donor {
            target_dir: dir.path().join("outside-target"),
            workspace_root: root.clone(),
            rustc: None,
        };
        std::fs::create_dir_all(&tracked.target_dir).unwrap();
        assert!(checkout_has_other_targets(&target, &[elsewhere, tracked]));
        // An untracked target below this one's sibling folders.
        let target = NewTarget {
            target_dir: root.join("target/clippy"),
            workspace_root: root.clone(),
        };
        assert!(!checkout_has_other_targets(&target, &[]));
        std::fs::create_dir_all(root.join("target/integrate-x/debug/.fingerprint")).unwrap();
        assert!(checkout_has_other_targets(&target, &[]));
    }

    /// A path package's units come from a target of the same checkout, whose
    /// files Cargo then compares as it would in the donor target, and from
    /// another checkout only with a dep-info whose files check out
    /// ([`workspace`]; these fixtures have none). A script output naming that
    /// checkout does not keep them out; one naming the donor's target does.
    #[test]
    fn workspace_units_are_seeded_from_the_same_checkout_or_checked() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            if layout == Layout::Shared {
                write(
                    &from.join(format!("build/app-{OTHER}/output")),
                    &format!(
                        "cargo:rerun-if-changed={}\n",
                        donor.workspace_root.join("app/migrations").display()
                    ),
                );
            }
            let same = NewTarget {
                target_dir: donor.workspace_root.join("target/integrate-clippy"),
                workspace_root: donor.workspace_root.clone(),
            };
            let seeded = seed(
                &same,
                VERSION,
                std::slice::from_ref(&donor),
                no_runs().path(),
                later(),
            );
            assert_eq!(seeded.units, 2, "{layout:?}");
            let to = same.target_dir.join(PROFILE);
            for (package, hash) in [("dep", HASH), ("app", OTHER)] {
                assert!(
                    marker(&to, layout, &unit(package, hash)).exists(),
                    "{layout:?} {package}"
                );
            }

            let other = checkout(dir.path(), "b");
            let seeded = seed(
                &other,
                VERSION,
                std::slice::from_ref(&donor),
                no_runs().path(),
                later(),
            );
            assert_eq!(seeded.units, 1, "{layout:?}: only the registry unit");
            assert!(
                !marker(&other.target_dir.join(PROFILE), layout, &unit("app", OTHER)).exists(),
                "{layout:?}"
            );
        }
        // The donor's own target named in a script's output keeps that unit
        // out of the same checkout too.
        let dir = tempfile::tempdir().unwrap();
        let donor = donor(dir.path(), "a", Layout::Shared);
        let from = donor.target_dir.join(PROFILE);
        write(
            &from.join(format!("build/app-{OTHER}/output")),
            &format!(
                "cargo:rustc-link-search=native={}\n",
                donor.target_dir.join("debug/native").display()
            ),
        );
        let app = unit("app", OTHER);
        assert!(names_the_donor(
            &from,
            Layout::Shared,
            &app,
            &needles_for(&donor, true)
        ));
        assert!(names_the_donor(
            &from,
            Layout::Shared,
            &app,
            &needles_for(&donor, false)
        ));
    }

    #[test]
    fn path_packages_and_the_workspace_switch() {
        assert_eq!(
            path_packages(LOCK).into_iter().collect::<Vec<_>>(),
            vec!["app".to_owned()]
        );
        // A registry package sharing the name keeps it out.
        let shadowed = format!(
            "{LOCK}\n[[package]]\nname = \"app\"\nversion = \"2.0.0\"\nsource = \"registry+x\"\n"
        );
        assert!(path_packages(&shadowed).is_empty());
        assert!(path_packages("not toml [").is_empty());
        for on in [None, Some(""), Some("1"), Some("on"), Some("yes")] {
            assert_eq!(
                workspace_units_wanted(on.map(OsStr::new)),
                WorkspaceUnits::Checked,
                "{on:?}"
            );
        }
        for same in ["same", " Same "] {
            assert_eq!(
                workspace_units_wanted(Some(OsStr::new(same))),
                WorkspaceUnits::SameCheckout
            );
        }
        for off in ["0", "false", "No", " off "] {
            assert_eq!(
                workspace_units_wanted(Some(OsStr::new(off))),
                WorkspaceUnits::Off,
                "{off}"
            );
        }
    }

    /// A cache at `cache` whose store holds each of `contents` as a blob, as
    /// kache stores what it compiles.
    fn cache_storing(cache: &Path, contents: &[&str]) -> PathBuf {
        for content in contents {
            let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
            write(
                &kache_store::blob_path_in_store_dir(&cache.join("store"), &hash),
                content,
            );
        }
        cache.to_path_buf()
    }

    /// A dependent's key hashes the bytes of what it links. A unit another
    /// kache, or a build without kache, compiled has other bytes than the
    /// cache restores for the same Cargo unit, so copying it would re-key
    /// every crate built on it.
    #[test]
    fn only_units_whose_artifacts_are_stored_blobs_are_seeded() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let donors = std::slice::from_ref(&donor);
            let unstored = cache_storing(&dir.path().join("unstored"), &[]);
            assert_eq!(
                seed(
                    &checkout(dir.path(), "b"),
                    VERSION,
                    donors,
                    &unstored,
                    later()
                ),
                Seeded::default(),
                "{layout:?}"
            );
            let stored = cache_storing(&dir.path().join("stored"), &["rlib"]);
            assert_eq!(
                seed(
                    &checkout(dir.path(), "c"),
                    VERSION,
                    donors,
                    &stored,
                    later()
                )
                .units,
                1,
                "{layout:?}"
            );
            // One artifact kache did not produce keeps the whole unit out.
            let profile = donor.target_dir.join(PROFILE);
            let rmeta = match layout {
                Layout::PerUnit => profile.join("build/dep").join(HASH).join("out/lib.rmeta"),
                Layout::Shared => profile.join("deps").join(format!("libdep-{HASH}.rmeta")),
            };
            write(&rmeta, "rmeta");
            assert_eq!(
                seed(
                    &checkout(dir.path(), "d"),
                    VERSION,
                    donors,
                    &stored,
                    later()
                ),
                Seeded::default(),
                "{layout:?}"
            );
        }
    }

    /// A donor artifact whose memo row holds for its whole current stamp, or
    /// that is a read-only link to the blob its row names, is taken as that
    /// blob without being read; anything else reads it. The donor's bytes
    /// here differ from the stored blob the row names, so only an artifact
    /// that was not read passes.
    #[test]
    fn the_store_vouches_for_a_donor_artifact_by_its_stamp_or_its_blobs_inode() {
        use kache_store::file_hash::FileFingerprint;
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let profile = donor.target_dir.join(PROFILE);
            let rlib = match layout {
                Layout::PerUnit => profile.join("build/dep").join(HASH).join("out/lib.rlib"),
                Layout::Shared => profile.join("deps").join(format!("libdep-{HASH}.rlib")),
            };
            write(&rlib, "RLIB");
            let deps = deps_by_hash(&profile.join("deps"));
            let cache = cache_storing(&dir.path().join("cache"), &["rlib"]);
            let stored = blake3::hash(b"rlib").to_hex().to_string();
            let stamp = FileFingerprint::from_path(&rlib).unwrap();
            let stored_at = |stamp: FileFingerprint, hash: &str| {
                let recorded = RecordedHashes::from([(stamp.path.clone(), (stamp, hash.into()))]);
                artifacts_stored(
                    &profile,
                    layout,
                    &unit("dep", HASH),
                    &deps,
                    &cache,
                    &recorded,
                    later(),
                )
            };
            assert!(stored_at(stamp.clone(), &stored), "{layout:?}");
            let unstored = blake3::hash(b"other").to_hex().to_string();
            assert!(!stored_at(stamp.clone(), &unstored), "{layout:?}");
            type Change = fn(&mut FileFingerprint);
            let changes: [(&str, Change); 4] = [
                ("size", |stamp| stamp.size += 1),
                ("mtime", |stamp| stamp.mtime_ns -= 1),
                ("ctime", |stamp| stamp.ctime_ns -= 1),
                ("inode", |stamp| stamp.inode += 1),
            ];
            for (field, change) in changes {
                let mut stale = stamp.clone();
                change(&mut stale);
                assert!(!stored_at(stale, &stored), "{layout:?} {field}");
            }
            // A link to the blob the row names is that blob at any stamp,
            // while nobody may write the inode. The blob's bytes here are not
            // the ones its name says, so again only an unread link passes.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let blob = kache_store::blob_path_in_store_dir(&cache.join("store"), &stored);
                std::fs::write(&blob, "RLIB").unwrap();
                std::fs::remove_file(&rlib).unwrap();
                std::fs::hard_link(&blob, &rlib).unwrap();
                let mode = |mode| {
                    std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(mode)).unwrap()
                };
                mode(0o444);
                let mut stale = FileFingerprint::from_path(&rlib).unwrap();
                stale.ctime_ns -= 1;
                assert!(stored_at(stale.clone(), &stored), "{layout:?} link");
                cache_storing(&dir.path().join("cache"), &["other"]);
                let other = blake3::hash(b"other").to_hex().to_string();
                assert!(!stored_at(stale.clone(), &other), "{layout:?} another blob");
                mode(0o644);
                assert!(!stored_at(stale, &stored), "{layout:?} writable link");
            }
        }
    }

    #[test]
    fn stored_artifact_validation_stops_at_the_seed_deadline() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let profile = donor.target_dir.join(PROFILE);
            let deps = deps_by_hash(&profile.join("deps"));
            let cache = cache_storing(&dir.path().join("cache"), &["rlib"]);
            let unit = unit("dep", HASH);
            assert!(artifacts_stored(
                &profile,
                layout,
                &unit,
                &deps,
                &cache,
                &RecordedHashes::new(),
                later()
            ));
            assert!(!artifacts_stored(
                &profile,
                layout,
                &unit,
                &deps,
                &cache,
                &RecordedHashes::new(),
                Instant::now(),
            ));
        }
    }

    fn unit(package: &str, hash: &str) -> Unit {
        Unit {
            package: package.into(),
            hash: hash.into(),
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in entries(dir) {
            if entry.is_dir() {
                files.extend(walk(&entry));
            } else {
                files.push(entry);
            }
        }
        files
    }

    #[test]
    fn skips_donors_that_cannot_give_this_build_its_units() {
        let dir = tempfile::tempdir().unwrap();
        let new = checkout(dir.path(), "b");
        let wrong_rustc = donor(dir.path(), "a", Layout::Shared);
        // Built by another compiler.
        assert_eq!(
            seed(
                &new,
                "rustc 9.9.9\n",
                std::slice::from_ref(&wrong_rustc),
                no_runs().path(),
                later()
            ),
            Seeded::default()
        );
        assert_eq!(
            seed(
                &new,
                "  ",
                std::slice::from_ref(&wrong_rustc),
                no_runs().path(),
                later()
            ),
            Seeded::default()
        );
        // A blank version is not a compiler, even where a donor recorded one.
        let blank = donor(dir.path(), "blank", Layout::Shared);
        let info = serde_json::json!({"outputs": {"1": {"stdout": "  "}}});
        write(
            &blank.target_dir.join(".rustc_info.json"),
            &info.to_string(),
        );
        assert_eq!(
            seed(
                &new,
                "  ",
                std::slice::from_ref(&blank),
                no_runs().path(),
                later()
            ),
            Seeded::default()
        );
        // A build holds the donor's lock: the next donor gives the units.
        let busy = donor(dir.path(), "busy", Layout::Shared);
        let held = try_lock(&busy.target_dir.join(PROFILE).join(".cargo-lock")).unwrap();
        let free = donor(dir.path(), "free", Layout::PerUnit);
        let seeded = seed(
            &new,
            VERSION,
            &[busy.clone(), free.clone()],
            no_runs().path(),
            later(),
        );
        assert_eq!(seeded.donor, Some(free.workspace_root.clone()));
        drop(held);
        // Another seeding reading the donor does not keep this one out.
        let reading = try_lock_shared(&busy.target_dir.join(PROFILE).join(".cargo-lock")).unwrap();
        let beside = checkout(dir.path(), "beside");
        let seeded = seed(
            &beside,
            VERSION,
            std::slice::from_ref(&busy),
            no_runs().path(),
            later(),
        );
        assert_eq!(seeded.donor, Some(busy.workspace_root.clone()));
        drop(reading);
        // The target itself, and a donor with no units, give nothing.
        let other = checkout(dir.path(), "c");
        let own = Donor {
            target_dir: other.target_dir.clone(),
            workspace_root: other.workspace_root.clone(),
            rustc: None,
        };
        let empty = donor(dir.path(), "empty", Layout::Shared);
        std::fs::remove_dir_all(empty.target_dir.join(PROFILE)).unwrap();
        std::fs::create_dir_all(empty.target_dir.join(PROFILE)).unwrap();
        assert_eq!(
            seed(&other, VERSION, &[own, empty], no_runs().path(), later()),
            Seeded::default()
        );
        // Past the deadline nothing is copied.
        let late = checkout(dir.path(), "late");
        assert_eq!(
            seed(
                &late,
                VERSION,
                std::slice::from_ref(&free),
                no_runs().path(),
                Instant::now()
            ),
            Seeded::default()
        );
        // A build holding the new target's lock is left alone.
        let locked = checkout(dir.path(), "locked");
        std::fs::create_dir_all(locked.target_dir.join(PROFILE)).unwrap();
        let ours = try_lock(&locked.target_dir.join(PROFILE).join(".cargo-lock")).unwrap();
        assert_eq!(
            seed(
                &locked,
                VERSION,
                std::slice::from_ref(&free),
                no_runs().path(),
                later()
            ),
            Seeded::default()
        );
        drop(ours);
        // Without a lockfile, or with only path packages, there is nothing to copy.
        let bare = checkout(dir.path(), "bare");
        std::fs::remove_file(bare.workspace_root.join("Cargo.lock")).unwrap();
        assert_eq!(
            seed(
                &bare,
                VERSION,
                std::slice::from_ref(&free),
                no_runs().path(),
                later()
            ),
            Seeded::default()
        );
        write(
            &bare.workspace_root.join("Cargo.lock"),
            "[[package]]\nname = \"app\"\n",
        );
        assert_eq!(
            seed(&bare, VERSION, &[free], no_runs().path(), later()),
            Seeded::default()
        );
    }

    fn script_stdout(profile: &Path, layout: Layout) -> PathBuf {
        match layout {
            Layout::PerUnit => profile.join("build/dep").join(HASH).join("run/stdout"),
            Layout::Shared => profile.join("build").join(format!("dep-{HASH}/output")),
        }
    }

    fn script_root_output(profile: &Path, layout: Layout) -> PathBuf {
        script_stdout(profile, layout).with_file_name("root-output")
    }

    fn write_script_stdout(profile: &Path, layout: Layout, text: &str) {
        write(&script_stdout(profile, layout), text);
        write(
            &script_root_output(profile, layout),
            build_script_out_dir(profile, layout, &unit("dep", HASH))
                .to_str()
                .unwrap(),
        );
    }

    fn cmake_cache(profile: &Path, layout: Layout) -> PathBuf {
        build_script_out_dir(profile, layout, &unit("dep", HASH)).join("build/CMakeCache.txt")
    }

    /// Record a build script whose stdout is only its own `OUT_DIR`.
    fn record_out_dir_stdout(profile: &Path, layout: Layout) -> PathBuf {
        let out_dir = build_script_out_dir(profile, layout, &unit("dep", HASH));
        let recorded = out_dir.display().to_string();
        write_script_stdout(
            profile,
            layout,
            &format!("cargo:rustc-link-search={recorded}\n"),
        );
        out_dir
    }

    /// Exercise the public seeding path, then remove each donor. Cargo's
    /// generated-input watcher and native search path must read the consumer.
    #[test]
    fn seeded_metadata_survives_donor_removal_and_tracks_generated_inputs() {
        use crate::build_script_inputs::{Resolved, Resolver, locate, resolve};
        use crate::cache_key::FileHasher;

        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("with spaces");
            let mut donor = donor(&root, "a", layout);
            let profile = donor.target_dir.join(PROFILE);
            if layout == Layout::Shared {
                // The library compile and build-script run are distinct units.
                std::fs::remove_file(script_stdout(&profile, layout)).unwrap();
                let _ = std::fs::remove_file(script_root_output(&profile, layout));
            }
            let script = unit("dep", OTHER);
            let script_marker = marker(&profile, layout, &script);
            write(
                &script_marker.join(match layout {
                    Layout::PerUnit => "fingerprint/run",
                    Layout::Shared => "run",
                }),
                "fp",
            );
            let old_out = build_script_out_dir(&profile, layout, &script);
            write(&old_out.join("generated.rs"), "pub const VALUE: u8 = 1;\n");
            write(&old_out.join("native/libseed.a"), "native fixture");
            let stdout = crate::cargo_layout::build_script_stdout(&old_out).unwrap();
            write(
                &stdout,
                &format!(
                    "cargo:rerun-if-changed={}/generated.rs\ncargo::rustc-link-search=native={}/native\ncargo:rustc-env=GENERATED={}/generated.rs\nopaque {}\n",
                    old_out.display(),
                    old_out.display(),
                    old_out.display(),
                    old_out.display()
                ),
            );
            write(
                &stdout.with_file_name("root-output"),
                old_out.to_str().unwrap(),
            );
            let depinfo = |profile: &Path| match layout {
                Layout::PerUnit => {
                    build_script_out_dir(profile, layout, &unit("dep", HASH)).join("dep.d")
                }
                Layout::Shared => profile.join("deps").join(format!("dep-{HASH}.d")),
            };
            let make_path = |path: &Path| path.to_str().unwrap().replace(' ', "\\ ");
            write(
                &depinfo(&profile),
                &format!(
                    "{}: {}\n",
                    make_path(&depinfo(&profile).with_extension("rlib")),
                    make_path(&old_out.join("generated.rs")),
                ),
            );
            if layout == Layout::Shared {
                write(
                    &profile
                        .join("build")
                        .join(format!("dep-{HASH}/build_script_build-{HASH}.d")),
                    &std::fs::read_to_string(depinfo(&profile)).unwrap(),
                );
            }
            let data_file = |profile: &Path| {
                build_script_out_dir(profile, layout, &unit("dep", HASH)).join("nested/config.d")
            };
            let opaque_data = format!("value: {}/is-data-not-depinfo\n", profile.display());
            write(&data_file(&profile), &opaque_data);
            let cache = no_runs();
            for name in ["b", "c"] {
                let current = checkout(&root, name);
                let original = std::fs::read(depinfo(&donor.target_dir.join(PROFILE))).unwrap();
                let old_mtime = filetime::FileTime::from_unix_time(1_000_000_000, 0);
                filetime::set_file_mtime(depinfo(&donor.target_dir.join(PROFILE)), old_mtime)
                    .unwrap();
                let seeded = seed(
                    &current,
                    VERSION,
                    std::slice::from_ref(&donor),
                    cache.path(),
                    later(),
                );
                assert_eq!(seeded.units, 2, "{layout:?}, {name}");
                assert_eq!(
                    std::fs::read(depinfo(&donor.target_dir.join(PROFILE))).unwrap(),
                    original,
                    "the donor is never rewritten"
                );
                std::fs::remove_dir_all(&donor.target_dir).unwrap();
                let profile = current.target_dir.join(PROFILE);
                assert_eq!(
                    filetime::FileTime::from_last_modification_time(
                        &std::fs::metadata(depinfo(&profile)).unwrap()
                    ),
                    old_mtime
                );
                let out = build_script_out_dir(&profile, layout, &script);
                let text = std::fs::read_to_string(depinfo(&profile)).unwrap();
                let inputs = crate::cache_key::parse_dep_info(&text);
                assert_eq!(
                    std::fs::read_to_string(&inputs[0]).unwrap(),
                    "pub const VALUE: u8 = 1;\n"
                );
                assert_eq!(inputs, [out.join("generated.rs")], "{layout:?}, {name}");
                assert!(text.starts_with(&make_path(&depinfo(&profile).with_extension("rlib"))));
                assert_eq!(
                    std::fs::read_to_string(data_file(&profile)).unwrap(),
                    opaque_data,
                    "nested .d files retain their data bytes"
                );
                if layout == Layout::Shared {
                    let text = std::fs::read_to_string(
                        profile
                            .join("build")
                            .join(format!("dep-{HASH}/build_script_build-{HASH}.d")),
                    )
                    .unwrap();
                    assert_eq!(
                        crate::cache_key::parse_dep_info(&text),
                        inputs,
                        "direct build-script compile dep-info also moves"
                    );
                }
                let stdout = crate::cargo_layout::build_script_stdout(&out).unwrap();
                assert_eq!(
                    std::fs::read_to_string(stdout.with_file_name("root-output")).unwrap(),
                    out.to_str().unwrap()
                );
                let output = std::fs::read_to_string(&stdout).unwrap();
                let native = output
                    .lines()
                    .find_map(|line| line.strip_prefix("cargo::rustc-link-search=native="))
                    .unwrap();
                assert_eq!(
                    std::fs::read_to_string(Path::new(native).join("libseed.a")).unwrap(),
                    "native fixture"
                );
                assert!(output.contains(&format!(
                    "cargo:rustc-env=GENERATED={}/generated.rs",
                    out.display()
                )));
                assert!(
                    output.contains(&format!("opaque {}", old_out.display())),
                    "non-directive stdout is opaque"
                );

                // Use the existing declared-input consumer, rather than only
                // checking that the copied text no longer spells the donor.
                let rustc_out = match layout {
                    Layout::PerUnit => build_script_out_dir(&profile, layout, &unit("dep", HASH)),
                    Layout::Shared => profile.join("deps"),
                };
                let invocation = crate::invocation::InvocationEnv::from_parts(
                    [
                        ("OUT_DIR", out.clone().into_os_string()),
                        (
                            "CARGO_MANIFEST_DIR",
                            current.workspace_root.clone().into_os_string(),
                        ),
                        ("CARGO_PKG_NAME", "dep".into()),
                        ("CARGO_HOME", root.join("cargo-home").into_os_string()),
                    ],
                    Some(current.workspace_root.clone()),
                    None,
                    dir.path().to_path_buf(),
                );
                let args = crate::args::RustcArgs::parse_for(
                    &[
                        "rustc".into(),
                        "--crate-name".into(),
                        "dep".into(),
                        "src/lib.rs".into(),
                        "--out-dir".into(),
                        rustc_out.to_str().unwrap().into(),
                    ],
                    &invocation,
                )
                .unwrap();
                let var = |key: &str| invocation.var_os(key);
                let located = locate(&args, &var, Some(&current.workspace_root), false).unwrap();
                let snapshot = || {
                    let mut file_hasher = FileHasher::new_for(&invocation);
                    file_hasher.arm_too_new_guard(1, 0);
                    let outside = |path: &Path| Ok(path.to_string_lossy().as_bytes().to_vec());
                    match resolve(
                        &located,
                        &Resolver {
                            file_hasher: &file_hasher,
                            var: &var,
                            cache_dir: cache.path(),
                            excluded: &[],
                            outside: &outside,
                            max_entries: 100,
                        },
                    )
                    .unwrap()
                    {
                        Resolved::Folded(snapshot) => snapshot,
                        other => panic!("expected declared inputs, got {other:?}"),
                    }
                };
                let before = snapshot();
                assert_eq!(before.paths, 1);
                write(&out.join("generated.rs"), "pub const VALUE: u8 = 200;\n");
                assert_ne!(
                    before.digest,
                    snapshot().digest,
                    "{layout:?}: generated input changed"
                );
                write(&out.join("generated.rs"), "pub const VALUE: u8 = 1;\n");
                donor = Donor {
                    target_dir: current.target_dir,
                    workspace_root: current.workspace_root,
                    rustc: Some(VERSION.into()),
                };
            }
        }
    }

    #[test]
    fn an_older_donors_raw_depinfo_is_not_seeded_again() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let older = donor(dir.path(), "older", layout);
            let borrower = donor(dir.path(), "borrower", layout);
            let depinfo = |profile: &Path| match layout {
                Layout::PerUnit => {
                    build_script_out_dir(profile, layout, &unit("dep", HASH)).join("dep.d")
                }
                Layout::Shared => profile.join("deps").join(format!("dep-{HASH}.d")),
            };
            let library = |profile: &Path| match layout {
                Layout::PerUnit => {
                    build_script_out_dir(profile, layout, &unit("dep", HASH)).join("lib.rlib")
                }
                Layout::Shared => profile.join("deps").join(format!("libdep-{HASH}.rlib")),
            };
            let rule = |profile: &Path| {
                format!(
                    "{}: registry.rs\n",
                    library(profile).to_str().unwrap().replace(' ', "\\ ")
                )
            };
            let old_profile = older.target_dir.join(PROFILE);
            let borrowed_profile = borrower.target_dir.join(PROFILE);
            write(&depinfo(&old_profile), &rule(&old_profile));
            std::fs::copy(depinfo(&old_profile), depinfo(&borrowed_profile)).unwrap();
            std::fs::remove_dir_all(&older.target_dir).unwrap();
            let current = checkout(dir.path(), "consumer");
            assert_eq!(
                seed(
                    &current,
                    VERSION,
                    std::slice::from_ref(&borrower),
                    no_runs().path(),
                    later()
                )
                .units,
                0,
                "{layout:?}: older Kache copied the first donor's rule verbatim"
            );
            assert!(
                !marker(
                    &current.target_dir.join(PROFILE),
                    layout,
                    &unit("dep", HASH)
                )
                .exists()
            );

            // Correct current-donor metadata remains eligible; the registry
            // dependency is unchanged and is never treated as an output root.
            write(&depinfo(&borrowed_profile), &rule(&borrowed_profile));
            let healthy = checkout(dir.path(), "healthy");
            assert_eq!(
                seed(&healthy, VERSION, &[borrower], no_runs().path(), later()).units,
                1,
                "{layout:?}: a donor-owned rule is still seeded"
            );
            assert!(
                marker(
                    &healthy.target_dir.join(PROFILE),
                    layout,
                    &unit("dep", HASH)
                )
                .is_dir()
            );
            assert_eq!(
                std::fs::read_to_string(depinfo(&healthy.target_dir.join(PROFILE))).unwrap(),
                rule(&healthy.target_dir.join(PROFILE))
            );
        }
    }

    #[test]
    fn unreadable_or_ambiguous_seed_metadata_does_not_publish_a_fingerprint() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            for malformed in [
                "reserved",
                "binary",
                "oversized",
                "wrong-root",
                "empty-root",
                "missing-root",
                "missing-stdout",
            ] {
                let dir = tempfile::tempdir().unwrap();
                let donor = donor(dir.path(), "a", layout);
                let from = donor.target_dir.join(PROFILE);
                let depinfo = match layout {
                    Layout::PerUnit => {
                        build_script_out_dir(&from, layout, &unit("dep", HASH)).join("dep.d")
                    }
                    Layout::Shared => from.join("deps").join(format!("dep-{HASH}.d")),
                };
                match malformed {
                    "reserved" => write(&depinfo, "output: __kache_root__/source.rs\n"),
                    "binary" => {
                        write(&depinfo, "");
                        std::fs::write(&depinfo, [0xff]).unwrap();
                    }
                    "oversized" => {
                        write(&depinfo, "");
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&depinfo)
                            .unwrap()
                            .set_len((16 << 20) + 1)
                            .unwrap();
                    }
                    "wrong-root" => {
                        write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
                        write(&script_root_output(&from, layout), "/unrecognized/out");
                    }
                    "empty-root" => {
                        write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
                        write(&script_root_output(&from, layout), "");
                    }
                    "missing-root" => {
                        write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
                        let _ = std::fs::remove_file(script_root_output(&from, layout));
                    }
                    "missing-stdout" => {
                        write(
                            &script_root_output(&from, layout),
                            build_script_out_dir(&from, layout, &unit("dep", HASH))
                                .to_str()
                                .unwrap(),
                        );
                        let _ = std::fs::remove_file(script_stdout(&from, layout));
                    }
                    _ => unreachable!(),
                }
                let current = checkout(dir.path(), "b");
                assert_eq!(
                    seed(&current, VERSION, &[donor], no_runs().path(), later()).units,
                    0,
                    "{layout:?}: {malformed}"
                );
                assert!(
                    !marker(
                        &current.target_dir.join(PROFILE),
                        layout,
                        &unit("dep", HASH)
                    )
                    .exists(),
                    "{layout:?}: {malformed}"
                );
            }
        }
    }

    #[test]
    fn unpaired_stdout_from_an_older_or_aliased_donor_does_not_publish_a_fingerprint() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            let older = dir.path().join("older/target/debug");
            #[cfg(unix)]
            let profiles = {
                let alias = dir.path().join("elsewhere-profile");
                std::os::unix::fs::symlink(&from, &alias).unwrap();
                vec![older, alias]
            };
            #[cfg(not(unix))]
            let profiles = vec![older];
            let _ = std::fs::remove_file(script_root_output(&from, layout));
            for (index, profile) in profiles.into_iter().enumerate() {
                let recorded = build_script_out_dir(&profile, layout, &dep);
                write(
                    &script_stdout(&from, layout),
                    &format!("cargo:rustc-link-search={}\n", recorded.display()),
                );
                assert!(
                    !names_the_donor(&from, layout, &dep, &donor_needles(&donor)),
                    "the lexical donor scan cannot resolve this old or aliased record"
                );
                let current = checkout(dir.path(), &format!("consumer-{index}"));
                assert_eq!(
                    seed(
                        &current,
                        VERSION,
                        std::slice::from_ref(&donor),
                        no_runs().path(),
                        later()
                    )
                    .units,
                    0,
                    "{layout:?}: {recorded:?}"
                );
                assert!(!marker(&current.target_dir.join(PROFILE), layout, &dep).exists());
            }
        }
    }

    #[test]
    fn leaves_build_scripts_that_name_the_donor() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            let run = match layout {
                Layout::PerUnit => from.join("build/dep").join(HASH).join("run"),
                Layout::Shared => from.join("build").join(format!("dep-{HASH}")),
            };
            let (stdout, root) = match layout {
                Layout::PerUnit => ("stdout", "root-output"),
                Layout::Shared => ("output", "root-output"),
            };
            let out_dir = run.join("out");
            let target = donor.target_dir.display().to_string();
            let workspace = donor.workspace_root.display().to_string();
            for (text, root_output, named) in [
                (String::from("cargo:rustc-cfg=x\n"), "", false),
                (
                    format!("cargo:rustc-link-search={}\n", out_dir.display()),
                    "out",
                    false,
                ),
                (
                    format!("cargo:rustc-link-search={target}/native\n"),
                    "out",
                    true,
                ),
                (format!("cargo:rerun-if-changed={workspace}/x\n"), "", true),
            ] {
                write(&run.join(stdout), &text);
                let recorded = if root_output.is_empty() {
                    String::new()
                } else {
                    out_dir.display().to_string()
                };
                write(&run.join(root), &recorded);
                assert_eq!(
                    names_the_donor(&from, layout, &dep, &donor_needles(&donor)),
                    named,
                    "{layout:?}: {text}"
                );
            }
            std::fs::remove_file(run.join(stdout)).unwrap();
            assert!(!names_the_donor(&from, layout, &dep, &donor_needles(&donor)));
            // A unit that names the donor is not copied.
            write(
                &run.join(stdout),
                &format!("cargo:rustc-link-search={target}/x\n"),
            );
            let new = checkout(dir.path(), "b");
            assert_eq!(
                seed(
                    &new,
                    VERSION,
                    std::slice::from_ref(&donor),
                    no_runs().path(),
                    later()
                ),
                Seeded::default()
            );
        }
    }

    #[test]
    fn an_output_file_that_names_the_donor_is_not_seeded() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            let out_dir = record_out_dir_stdout(&from, layout);
            // The stdout check strips OUT_DIR. The cache file is not rewritten,
            // and `OUT_DIR/build` still contains the donor target directory.
            write(
                &cmake_cache(&from, layout),
                &format!("CMAKE_CACHEFILE_DIR:INTERNAL={}/build\n", out_dir.display()),
            );
            assert!(
                !names_the_donor(&from, layout, &dep, &donor_needles(&donor)),
                "{layout:?}: stdout only names OUT_DIR"
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                "{layout:?}"
            );
            let new = checkout(dir.path(), "b");
            assert_eq!(
                seed(
                    &new,
                    VERSION,
                    std::slice::from_ref(&donor),
                    no_runs().path(),
                    later()
                ),
                Seeded::default(),
                "{layout:?}"
            );
            assert!(
                !marker(&new.target_dir.join(PROFILE), layout, &dep).exists(),
                "{layout:?}"
            );
        }
    }

    #[test]
    fn an_output_file_that_does_not_name_the_donor_is_seeded() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            record_out_dir_stdout(&from, layout);
            let cache = "portable-cache\n";
            write(&cmake_cache(&from, layout), cache);
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                "{layout:?}"
            );
            let new = checkout(dir.path(), "b");
            assert_eq!(
                seed(
                    &new,
                    VERSION,
                    std::slice::from_ref(&donor),
                    no_runs().path(),
                    later()
                ),
                Seeded {
                    units: 1,
                    donor: Some(donor.workspace_root.clone()),
                    ..Seeded::default()
                },
                "{layout:?}"
            );
            let copied = cmake_cache(&new.target_dir.join(PROFILE), layout);
            assert_eq!(
                std::fs::read_to_string(&copied).unwrap(),
                cache,
                "{layout:?}"
            );
        }
    }

    #[test]
    fn each_donor_path_is_enough_and_an_unread_file_keeps_the_unit_out() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
            let cache = cmake_cache(&from, layout);

            let mut named = donor.clone();
            named.target_dir = PathBuf::from("/needle-target");
            named.workspace_root = PathBuf::from("/needle-workspace");
            write(&cache, "see /needle-workspace/src\n");
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&named), later()),
                "{layout:?} workspace"
            );
            write(&cache, "see /needle-target/debug\n");
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&named), later()),
                "{layout:?} target"
            );
            write(&cache, "see /neither\n");
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&named), later()),
                "{layout:?}"
            );
            // Longer than both needles, and neither needle is in it. A
            // shorter miss makes `windows` empty, so `==` and `!=` agree.
            write(&cache, "xxxx see /neither xxxxxxxxx\n");
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&named), later()),
                "{layout:?}: a longer file that names neither path"
            );

            let mut blank = donor.clone();
            blank.target_dir.clear();
            blank.workspace_root.clear();
            write(&cache, "x\n");
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&blank), later()),
                "{layout:?} empty"
            );

            let needle = donor.target_dir.to_string_lossy().into_owned();
            std::fs::remove_file(script_stdout(&from, layout)).unwrap();
            write(&cache, &format!("{needle}\n"));
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                "{layout:?}: no script record, so out/ is the unit's artifact"
            );

            write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
            write(&cache, &format!("{needle}\n"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let out = build_script_out_dir(&from, layout, &dep);
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o000)).unwrap();
                assert!(
                    std::fs::read(&cache).is_err(),
                    "{layout:?}: mode 000 must be unreadable here"
                );
                assert!(
                    out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                    "{layout:?}: an unreadable output file is not copied"
                );
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();

                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o000)).unwrap();
                assert!(
                    out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                    "{layout:?}: an unlisted out/ is not copied"
                );
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();

                let outside = dir.path().join("outside.txt");
                write(&outside, &needle);
                write(&cache, "portable-cache\n");
                let link = out.join("linked-cache");
                std::os::unix::fs::symlink(&outside, &link).unwrap();
                assert!(
                    !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                    "{layout:?}: a symlink is not followed"
                );
            }
        }
    }

    #[test]
    fn a_donor_path_with_backslashes_is_also_searched_with_slashes_and_doubled() {
        let needles = |target: &str, workspace: &str| {
            donor_needles(&Donor {
                target_dir: PathBuf::from(target),
                workspace_root: PathBuf::from(workspace),
                rustc: None,
            })
        };
        assert_eq!(
            needles(r"C:\Users\me\ws-a\target", r"C:\Users\me\ws-a"),
            [
                "C:/Users/me/ws-a/target",
                r"C:\\Users\\me\\ws-a\\target",
                r"C:\Users\me\ws-a\target",
                "C:/Users/me/ws-a",
                r"C:\\Users\\me\\ws-a",
                r"C:\Users\me\ws-a",
            ]
        );
        assert_eq!(needles("/ws-a/target", "/ws-a"), ["/ws-a/target", "/ws-a"]);
        assert!(needles("", "").is_empty());
    }

    /// CMake on Windows writes the build directory into `CMakeCache.txt` as
    /// `D:/target/ws-a/debug/...`, and a script can print it that way. A
    /// string literal, JSON and `{:?}` double each `\`, and a drive letter
    /// can come in either case. Each keeps the unit out. The match is on
    /// text, so this runs on every platform.
    #[test]
    fn other_spellings_of_a_windows_donor_keep_the_unit_out() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            let mut windows = donor.clone();
            windows.target_dir = PathBuf::from(r"D:\target\ws-a");
            windows.workspace_root = PathBuf::from(r"C:\Users\me\ws-a");
            let stdout = script_stdout(&from, layout);
            let cache = cmake_cache(&from, layout);

            write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
            write(&cache, "CMAKE_HOME_DIRECTORY:INTERNAL=C:/Users/me/vendor\n");
            assert!(
                !names_the_donor(&from, layout, &dep, &donor_needles(&windows)),
                "{layout:?}"
            );
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&windows), later()),
                "{layout:?}"
            );

            write(
                &cache,
                "CMAKE_CACHEFILE_DIR:INTERNAL=D:/target/ws-a/debug/build/dep/out/build\n",
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&windows), later()),
                "{layout:?}: the target directory"
            );
            write(
                &cache,
                "CMAKE_HOME_DIRECTORY:INTERNAL=C:/Users/me/ws-a/vendor\n",
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&windows), later()),
                "{layout:?}: the workspace"
            );
            write(
                &cache,
                r#"include_bytes!("D:\\target\\ws-a\\debug\\build\\dep\\out\\t.bin")"#,
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&windows), later()),
                "{layout:?}: doubled backslashes"
            );
            for (text, named) in [
                ("c:/Users/me/ws-a/vendor\n", true),
                (r"c:\Users\me\ws-a\vendor", true),
                ("E:/Users/me/ws-a/vendor\n", false),
                (":/Users/me/ws-a/vendor\n", false),
            ] {
                write(&cache, text);
                assert_eq!(
                    out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&windows), later()),
                    named,
                    "{layout:?}: {text}"
                );
            }
            let mut lowercase = windows.clone();
            lowercase.workspace_root = PathBuf::from(r"c:\Users\me\ws-a");
            write(&cache, "C:/Users/me/ws-a/vendor\n");
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&lowercase), later()),
                "{layout:?}: a lowercase drive in the donor"
            );

            write(
                &stdout,
                "cargo:rustc-link-search=native=D:/target/ws-a/debug/gn_out/obj\n",
            );
            assert!(names_the_donor(&from, layout, &dep, &donor_needles(&windows)), "{layout:?}");
            write(
                &stdout,
                r#"cargo:warning=using "c:\\Users\\me\\ws-a\\vendor""#,
            );
            assert!(names_the_donor(&from, layout, &dep, &donor_needles(&windows)), "{layout:?}");
        }
    }

    /// Only a drive letter, a letter followed by `:`, matches in either
    /// case. A path that starts with any other letter matches as spelled.
    #[test]
    fn only_a_drive_letter_matches_in_either_case() {
        assert!(contains_path(b"at c:/ws-a/x", "C:/ws-a"));
        assert!(!contains_path(b"at C:/WS-A/x", "C:/ws-a"));
        assert!(contains_path(b"at ws-a/x", "ws-a"));
        assert!(!contains_path(b"at Ws-a/x", "ws-a"));
    }

    /// A reader of `data`, at most `step` bytes a read, that returns nothing
    /// before `until`.
    struct Reads<'a> {
        data: &'a [u8],
        step: usize,
        until: Instant,
    }

    impl std::io::Read for Reads<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            while Instant::now() < self.until {
                std::thread::sleep(Duration::from_millis(1));
            }
            let read = buffer.len().min(self.step).min(self.data.len());
            buffer[..read].copy_from_slice(&self.data[..read]);
            self.data = &self.data[read..];
            Ok(read)
        }
    }

    /// Each read is searched with the end of the one before, so a path
    /// split between reads anywhere is found, the longest one included.
    #[test]
    fn a_donor_path_split_between_reads_is_found() {
        let needles = [r"C:\Users\me\ws-a".to_string(), "/ws-a".to_string()];
        let mut scan = OutScan::new(&needles, later());
        for needle in &needles {
            for step in 1..=8 {
                for at in 0..step {
                    let mut data = vec![b'x'; at];
                    data.extend_from_slice(needle.as_bytes());
                    data.extend_from_slice(b"xxx");
                    let reads = Reads {
                        data: &data,
                        step,
                        until: Instant::now(),
                    };
                    assert!(
                        scan.read_keeps_out(reads),
                        "{needle} at {at}, {step} bytes a read"
                    );
                }
            }
        }
        let clean = br"xx:\Users\me\ws-a /ws-b";
        for step in 1..=8 {
            let reads = Reads {
                data: clean,
                step,
                until: Instant::now(),
            };
            assert!(!scan.read_keeps_out(reads), "{step} bytes a read");
        }
    }

    /// A file is read a chunk at a time, all of it: a donor path at the end
    /// of one longer than a chunk, or across the first two chunks, keeps the
    /// unit out, and the same file without it is seeded.
    #[test]
    fn a_donor_path_past_the_first_chunk_of_a_file_keeps_the_unit_out() {
        use std::io::{Seek as _, Write as _};
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
            let cache = cmake_cache(&from, layout);
            write(&cache, "");
            let needle = donor.target_dir.to_string_lossy().into_owned();
            let len = 3 * OUT_SCAN_CHUNK as u64;
            let chunk = OUT_SCAN_CHUNK as u64;
            let needle_len = needle.len() as u64;
            for at in [len - needle_len, chunk - needle_len / 2] {
                // Sparse: zeros around the path, without writing them.
                let mut file = std::fs::File::create(&cache).unwrap();
                file.set_len(len).unwrap();
                file.seek(std::io::SeekFrom::Start(at)).unwrap();
                file.write_all(needle.as_bytes()).unwrap();
                assert!(
                    out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                    "{layout:?}: at {at}"
                );
            }
            std::fs::File::create(&cache).unwrap().set_len(len).unwrap();
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                "{layout:?}: clean"
            );
        }
    }

    /// The deadline is checked before each read, so a file still being read
    /// when it passes keeps the unit out.
    #[test]
    fn the_deadline_passing_between_two_reads_keeps_the_unit_out() {
        let needles = ["/ws-a".to_string()];
        let data = vec![b'x'; 2 * OUT_SCAN_CHUNK];
        let deadline = Instant::now() + Duration::from_millis(50);
        let reads = Reads {
            data: &data,
            step: OUT_SCAN_CHUNK,
            until: deadline,
        };
        assert!(OutScan::new(&needles, deadline).read_keeps_out(reads));
        let reads = Reads {
            data: &data,
            step: OUT_SCAN_CHUNK,
            until: Instant::now(),
        };
        assert!(!OutScan::new(&needles, later()).read_keeps_out(reads));
    }

    /// A read that fails part way leaves the rest unread, which keeps the
    /// unit out.
    #[test]
    fn a_failed_read_keeps_the_unit_out() {
        struct Fails;
        impl std::io::Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::Other.into())
            }
        }
        let needles = ["/ws-a".to_string()];
        let clean = Reads {
            data: b"clean",
            step: 2,
            until: Instant::now(),
        };
        let reads = std::io::Read::chain(clean, Fails);
        assert!(OutScan::new(&needles, later()).read_keeps_out(reads));
    }

    /// Every file under `out/` counts toward one budget. A unit whose files
    /// add up to it is read; a byte more, or the deadline passing first,
    /// keeps the unit out unread.
    #[test]
    fn an_out_dir_past_the_scan_budget_or_the_deadline_keeps_the_unit_out() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        write(&out.join("first.o"), "first");
        write(&out.join("build/CMakeCache.txt"), "cache");
        let needles = ["/ws-a".to_string()];
        for (budget, keeps_out) in [(10, false), (9, true)] {
            let mut scan = OutScan::new(&needles, later());
            scan.bytes_left = budget;
            assert_eq!(scan.keeps_out(&out), keeps_out, "a budget of {budget}");
        }

        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let dep = unit("dep", HASH);
            write_script_stdout(&from, layout, "cargo:rustc-cfg=x\n");
            let cache = cmake_cache(&from, layout);
            write(&cache, "");
            // Sparse, and past the budget on its own, so it is never read.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&cache)
                .unwrap();
            file.set_len(OUT_SCAN_BYTES + 1).unwrap();
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                "{layout:?}: past the budget"
            );
            file.set_len(0).unwrap();
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), later()),
                "{layout:?}"
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor_needles(&donor), Instant::now()),
                "{layout:?}: the deadline passed"
            );
        }
    }

    #[test]
    fn a_unit_that_cannot_be_copied_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let donor = donor(dir.path(), "a", Layout::PerUnit);
        let unit_dir = donor.target_dir.join(PROFILE).join("build/dep").join(HASH);
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", unit_dir.join("link")).unwrap();
        #[cfg(not(unix))]
        std::fs::remove_dir_all(unit_dir.join("out")).unwrap();
        let new = checkout(dir.path(), "b");
        let seeded = seed(
            &new,
            VERSION,
            std::slice::from_ref(&donor),
            no_runs().path(),
            later(),
        );
        #[cfg(unix)]
        {
            assert_eq!(seeded, Seeded::default());
            let package = new.target_dir.join(PROFILE).join("build/dep");
            assert_eq!(entries(&package), Vec::<PathBuf>::new());
        }
        #[cfg(not(unix))]
        let _ = seeded;
    }

    /// rdkafka-sys leaves `librdkafka.so -> librdkafka.so.1` in its
    /// `OUT_DIR`. A link that stays inside the unit is copied as the same
    /// link; one that leaves it, or is the whole unit, fails the copy.
    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_unit_is_copied_as_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("unit");
        write(&from.join("out/src/librdkafka.so.1"), "so");
        std::os::unix::fs::symlink("librdkafka.so.1", from.join("out/src/librdkafka.so")).unwrap();
        write(&from.join("out/include/rdkafka.h"), "h");
        std::os::unix::fs::symlink("../include", from.join("out/src/include")).unwrap();
        let to = dir.path().join("copy");
        place_tree(
            &from,
            &to,
            no_runs().path(),
            &FileCounts::default(),
            &Staging::beside(),
            later(),
            None,
        )
        .unwrap();
        let link = to.join("out/src/librdkafka.so");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("librdkafka.so.1")
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "so");
        assert_eq!(
            std::fs::read_to_string(to.join("out/src/include/rdkafka.h")).unwrap(),
            "h"
        );

        std::os::unix::fs::symlink("../../../elsewhere", from.join("out/src/up")).unwrap();
        assert!(
            place_tree(
                &from,
                &dir.path().join("escaped"),
                no_runs().path(),
                &FileCounts::default(),
                &Staging::beside(),
                later(),
                None
            )
            .is_err()
        );
        assert!(!dir.path().join("escaped").exists());

        let linked_unit = dir.path().join("linked-unit");
        std::os::unix::fs::symlink("unit", &linked_unit).unwrap();
        assert!(
            place_tree(
                &linked_unit,
                &dir.path().join("whole"),
                no_runs().path(),
                &FileCounts::default(),
                &Staging::beside(),
                later(),
                None
            )
            .is_err()
        );
    }

    /// A hermetic run leaves `OUT_DIR` as an absolute link to its sealed,
    /// shared run. Seeding with the cache that holds the run links the new
    /// unit's `OUT_DIR` to it and records the link; with any other cache the
    /// unit is left out. Both layouts.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_sealed_hermetic_run_is_linked_again_and_recorded() {
        use crate::build_script::hermetic_test_support::{linked_to, sealed_run, tempdir};
        const RUN: &str = "1111222233334444";
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempdir();
            let donor = donor(dir.path(), "a", layout);
            let from = donor.target_dir.join(PROFILE);
            let run = match layout {
                Layout::PerUnit => {
                    let run = from.join("build/dep").join(RUN);
                    write(&run.join("fingerprint/run-build-script"), "fp");
                    write(&run.join("run/stdout"), "cargo:rerun-if-changed=build.rs\n");
                    run
                }
                Layout::Shared => {
                    let named = format!("dep-{RUN}");
                    write(&from.join(".fingerprint").join(&named).join("run"), "fp");
                    let run = from.join("build").join(&named);
                    write(&run.join("output"), "cargo:rerun-if-changed=build.rs\n");
                    run
                }
            };
            let below = run.join("out");
            let below = below.strip_prefix(&donor.target_dir).unwrap();
            let cache = cache_storing(&dir.path().join("cache"), &["rlib"]);
            let shared = sealed_run(&cache, &"ab".repeat(32), below);
            std::os::unix::fs::symlink(&shared, run.join("out")).unwrap();
            let stdout = crate::cargo_layout::build_script_stdout(&run.join("out")).unwrap();
            write(
                &stdout,
                &format!("cargo:rerun-if-changed={}/gen.rs\n", shared.display()),
            );
            write(
                &stdout.with_file_name("root-output"),
                shared.to_str().unwrap(),
            );
            let elsewhere = cache_storing(&dir.path().join("other-cache"), &["rlib"]);
            let new = checkout(dir.path(), "b");
            let seeded = seed(
                &new,
                VERSION,
                std::slice::from_ref(&donor),
                &elsewhere,
                later(),
            );
            assert_eq!(seeded.units, 1, "{layout:?}: only the lib unit");
            let to = new.target_dir.join(below);
            assert!(std::fs::symlink_metadata(&to).is_err(), "{layout:?}");
            assert!(
                !marker(&new.target_dir.join(PROFILE), layout, &unit("dep", RUN)).exists(),
                "{layout:?}"
            );

            let new = checkout(dir.path(), "c");
            let seeded = seed(&new, VERSION, std::slice::from_ref(&donor), &cache, later());
            assert_eq!(seeded.units, 2, "{layout:?}: the lib and run units");
            let to = new.target_dir.join(below);
            assert_eq!(std::fs::read_link(&to).unwrap(), shared, "{layout:?}");
            assert_eq!(
                std::fs::read_to_string(to.join("gen.rs")).unwrap(),
                "pub const N: u8 = 1;"
            );
            assert_eq!(linked_to(&shared, below), vec![to.clone()], "{layout:?}");
            let stdout = crate::cargo_layout::build_script_stdout(&to).unwrap();
            assert_eq!(
                std::fs::read_to_string(stdout.with_file_name("root-output")).unwrap(),
                to.to_str().unwrap()
            );
            let text = std::fs::read(&stdout).unwrap();
            let declared = crate::build_script::declarations::cargo_declarations(&text, None);
            assert_eq!(
                std::fs::read_to_string(&declared.paths[0]).unwrap(),
                "pub const N: u8 = 1;"
            );
        }
    }

    #[test]
    fn a_deps_file_is_indexed_by_the_unit_hash_in_its_name() {
        let hash = "0123456789abcdef";
        for name in [
            format!("libfoo_bar-{hash}.rlib"),
            format!("libfoo_bar-{hash}.so.1"),
            format!("foo-bar-{hash}"),
            format!("foo_bar-{hash}.d"),
        ] {
            assert_eq!(unit_hashes(&name).collect::<Vec<_>>(), vec![hash], "{name}");
        }
        for name in [
            format!("foo-{hash}0.rlib"),
            format!("foo-{hash}x"),
            "foo-0123456789abcdeg.rlib".to_owned(),
            format!("foo_{hash}.rlib"),
        ] {
            assert!(unit_hashes(&name).next().is_none(), "{name}");
        }

        let dir = tempfile::tempdir().unwrap();
        for name in [
            format!("libfoo-{hash}.rlib"),
            format!("foo-{hash}.d"),
            "other.d".to_owned(),
        ] {
            write(&dir.path().join(name), "x");
        }
        let index = deps_by_hash(dir.path());
        assert_eq!(index.len(), 1);
        assert_eq!(
            index[hash],
            vec![
                dir.path().join(format!("foo-{hash}.d")),
                dir.path().join(format!("libfoo-{hash}.rlib"))
            ]
        );
    }

    #[test]
    fn a_passed_deadline_stops_a_copy_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("unit");
        write(&from.join("a/file"), "x");
        let to = dir.path().join("copy");
        let error = place_tree(
            &from,
            &to,
            no_runs().path(),
            &FileCounts::default(),
            &Staging::beside(),
            Instant::now(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(entries(dir.path()), vec![from.clone()]);
        place_tree(
            &from,
            &to,
            no_runs().path(),
            &FileCounts::default(),
            &Staging::beside(),
            later(),
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(to.join("a/file")).unwrap(), "x");
    }

    /// A seeding's note goes where the log's file layer writes:
    /// `KACHE_LOG_FILE_PATH`, never the default log beside the default cache
    /// folder (found on Windows, where that is `%LOCALAPPDATA%\\kache`).
    #[test]
    fn a_seeding_note_honours_the_log_file_path() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("os-cache");
        let env = crate::invocation::InvocationEnv::from_parts(
            [("KACHE_LOG_FILE_PATH", "logs/seed.log")],
            Some(root.path().to_owned()),
            None,
            root.path().to_owned(),
        )
        .with_cache_dir(Some(cache.clone()));
        note_in_log_for(&env, "seeded target");
        let logged = std::fs::read_to_string(root.path().join("logs/seed.log")).unwrap();
        assert!(logged.contains("INFO kache::target_seed: seeded target"), "{logged}");
        assert!(!cache.exists(), "nothing is written beside the default cache folder");
    }

    /// A target directory seeding creates carries Cargo's tag, so kache
    /// tracks it like one Cargo created, even when nothing was seeded.
    #[test]
    fn a_target_seeding_creates_is_tagged_like_cargos() {
        let dir = tempfile::tempdir().unwrap();
        let new = checkout(dir.path(), "b");
        assert_eq!(
            seed(&new, VERSION, &[], no_runs().path(), later()),
            Seeded::default()
        );
        let tag = std::fs::read_to_string(new.target_dir.join("CACHEDIR.TAG")).unwrap();
        assert_eq!(tag, CARGO_CACHEDIR_TAG);
        assert!(crate::machine::target_root_is_safe(
            &new.target_dir,
            &new.workspace_root
        ));
    }

    /// Cargo does not tag a target directory that already exists, and
    /// neither does seeding.
    #[test]
    fn an_existing_untagged_target_stays_untagged() {
        let dir = tempfile::tempdir().unwrap();
        let new = checkout(dir.path(), "b");
        std::fs::create_dir_all(&new.target_dir).unwrap();
        seed(&new, VERSION, &[], no_runs().path(), later());
        assert!(new.target_dir.join(PROFILE).is_dir());
        assert!(!new.target_dir.join("CACHEDIR.TAG").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_copied_file_takes_the_given_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("build-script-build");
        write(&from, "binary");
        std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o644)).unwrap();
        let recorded = dir.path().join("recorded");
        write(&recorded, "");
        std::fs::set_permissions(&recorded, std::fs::Permissions::from_mode(0o757)).unwrap();
        // Without the process's creation mask, and with it as a seeding
        // reads it, which takes the file's word for no mode it may change.
        let known = FileCounts {
            umask: creation_mask(),
            ..FileCounts::default()
        };
        for (name, counts) in [("copy", FileCounts::default()), ("known", known)] {
            let to = dir.path().join(name);
            place_file(
                &from,
                Path::new(""),
                &to,
                &std::fs::metadata(&recorded).unwrap(),
                &counts,
            )
            .unwrap();
            let mode = std::fs::metadata(&to).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o757, "{name}");
            // The recorded stat is not the file's: what it holds is copied.
            assert_eq!(std::fs::read_to_string(&to).unwrap(), "binary", "{name}");
        }
    }

    /// Whether this process writes files whatever their mode (root).
    #[cfg(unix)]
    fn writes_anything() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_is_linked_and_a_writable_one_copied() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("unit");
        let blob = from.join("libdep-0123456789abcdef.rlib");
        write(&blob, "rlib");
        std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o440)).unwrap();
        let dep_info = from.join("dep-0123456789abcdef.d");
        write(&dep_info, "d");
        let to = dir.path().join("placed");
        let counts = FileCounts::default();
        place_tree(
            &from,
            &to,
            no_runs().path(),
            &counts,
            &Staging::beside(),
            later(),
            None,
        )
        .unwrap();
        let linked = to.join("libdep-0123456789abcdef.rlib");
        let copied = to.join("dep-0123456789abcdef.d");
        assert_eq!(std::fs::read_to_string(&linked).unwrap(), "rlib");
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "d");
        let same_inode = |a: &Path, b: &Path| {
            std::fs::metadata(a).unwrap().ino() == std::fs::metadata(b).unwrap().ino()
        };
        if writes_anything() {
            assert!(!same_inode(&blob, &linked), "root must copy");
            return;
        }
        if kache_store::link::try_reflink(&dep_info, &dir.path().join("probe")).is_ok() {
            // A clone is preferred where the filesystem makes one.
            assert!(!same_inode(&blob, &linked));
            return;
        }
        assert!(same_inode(&blob, &linked));
        assert!(!same_inode(&dep_info, &copied));
        assert_eq!(counts.linked.load(Ordering::Relaxed), 1);
        assert_eq!(counts.copied.load(Ordering::Relaxed), 1);
        // The shared inode cannot be written through the new name.
        let opened = std::fs::OpenOptions::new().write(true).open(&linked);
        assert_eq!(
            opened.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read_to_string(&blob).unwrap(), "rlib");
    }

    /// The events inotify reports for the entries of one directory.
    #[cfg(target_os = "linux")]
    struct Watch(std::os::fd::OwnedFd);

    #[cfg(target_os = "linux")]
    impl Watch {
        fn new(dir: &Path) -> Self {
            use std::os::fd::FromRawFd;
            use std::os::unix::ffi::OsStrExt;
            // SAFETY: inotify_init1 has no preconditions; a valid descriptor
            // is owned from here on.
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            assert!(fd >= 0, "{}", std::io::Error::last_os_error());
            // SAFETY: fd was just returned by inotify_init1 and is open.
            let watch = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
            let mask = libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_TO
                | libc::IN_MODIFY
                | libc::IN_CLOSE_WRITE;
            // SAFETY: fd is an inotify descriptor and path a C string.
            assert!(unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) } >= 0);
            watch
        }

        /// Each event so far: its mask and the entry's name.
        fn events(&self) -> Vec<(u32, String)> {
            use std::io::Read;
            use std::os::fd::AsFd;
            let mut file = std::fs::File::from(self.0.as_fd().try_clone_to_owned().unwrap());
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(read) = file.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            let mut events = Vec::new();
            let mut at = 0;
            let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
            while at + 16 <= bytes.len() {
                let (mask, len) = (word(at + 4), word(at + 12) as usize);
                let name = &bytes[at + 16..at + 16 + len];
                let name = name.split(|&byte| byte == 0).next().unwrap_or_default();
                events.push((mask, String::from_utf8_lossy(name).into_owned()));
                at += 16 + len;
            }
            events
        }
    }

    /// Under a busy journal, every metadata call of a copy waits its turn.
    /// A filesystem that cannot clone (ext4) refuses the first clone of a
    /// seeding, and each later attempt only created and removed a file in
    /// the target. And a final name never holds a file being written: each
    /// one appears whole, by link or by rename, so a seeding cut short
    /// leaves no partial artifact.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_seeding_asks_for_a_clone_once_and_never_writes_a_final_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("probe"), "probe");
        if kache_store::link::try_reflink(&dir.path().join("probe"), &dir.path().join("clone"))
            .is_ok()
        {
            eprintln!("skipping: this filesystem clones");
            return;
        }
        let from = dir.path().join("donor");
        let blobs = ["a", "b", "c", "d"].map(|name| from.join(format!("lib{name}-0123.rlib")));
        for (blob, text) in blobs.iter().zip(["a", "b", "c", "d"]) {
            write(blob, text);
            std::fs::set_permissions(blob, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let dep_info = from.join("dep-0123.d");
        write(&dep_info, "d");
        let deps = dir.path().join("deps");
        std::fs::create_dir(&deps).unwrap();
        let watch = Watch::new(&deps);
        let counts = FileCounts::default();
        // As a copy thread stages: in a directory of its own.
        let staging = Staging::in_profile(dir.path());
        for file in blobs.iter().chain([&dep_info]) {
            let to = deps.join(file_name(file));
            place_tree(
                file,
                &to,
                no_runs().path(),
                &counts,
                &staging,
                later(),
                None,
            )
            .unwrap();
            assert_eq!(
                std::fs::read(&to).unwrap(),
                std::fs::read(file).unwrap(),
                "{}",
                to.display()
            );
        }
        let events = watch.events();
        let removed = events
            .iter()
            .filter(|(mask, _)| mask & libc::IN_DELETE != 0)
            .count();
        assert!(removed <= 1, "a clone is asked for once: {events:?}");
        for (mask, name) in &events {
            if !name.starts_with(STAGING) {
                assert_eq!(
                    mask & (libc::IN_MODIFY | libc::IN_CLOSE_WRITE),
                    0,
                    "{name} was written in place: {events:?}"
                );
            }
        }
        if !writes_anything() {
            // Once the filesystem has refused a clone, a file nobody may
            // write is linked straight to its name.
            let renamed = events
                .iter()
                .filter(|(mask, name)| mask & libc::IN_MOVED_TO != 0 && name.ends_with(".rlib"))
                .count();
            assert!(renamed <= 1, "{events:?}");
            assert_eq!(counts.linked.load(Ordering::Relaxed), 4);
        }
    }

    /// A copy thread places its staged units' outputs before any marker, and
    /// a unit one of whose outputs cannot be placed keeps out its marker;
    /// the others are placed whole.
    #[test]
    fn a_unit_whose_output_cannot_be_placed_keeps_out_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, deps, fingerprints) = (
            dir.path().join("staging"),
            dir.path().join("deps"),
            dir.path().join(".fingerprint"),
        );
        // A name a rename cannot take: a directory that holds a file.
        write(&deps.join("taken/file"), "x");
        std::fs::create_dir(&fingerprints).unwrap();
        let rename = |name: &str, to: PathBuf| {
            let at = staging.join(name);
            write(&at.join("file"), name);
            Place::Rename {
                staging: at,
                to,
                placed: Vec::new(),
                runs: Vec::new(),
            }
        };
        let units = vec![
            StagedUnit {
                outputs: vec![rename("good-out", deps.join("good"))],
                marker: rename("good-fp", fingerprints.join("good")),
            },
            StagedUnit {
                outputs: vec![rename("bad-out", deps.join("taken"))],
                marker: rename("bad-fp", fingerprints.join("bad")),
            },
        ];
        assert_eq!(place_staged(units, &FileCounts::default()), 1);
        assert!(deps.join("good/file").is_file());
        assert!(fingerprints.join("good/file").is_file());
        assert!(!fingerprints.join("bad").exists());
        assert!(
            entries(&staging).is_empty(),
            "what was staged for the unit left out goes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_under_a_build_scripts_out_dir_is_copied() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("dep-0123456789abcdef");
        let generated = from.join("out/bindings.rs");
        write(&generated, "generated");
        std::fs::set_permissions(&generated, std::fs::Permissions::from_mode(0o444)).unwrap();
        let to = dir.path().join("placed");
        let counts = FileCounts::default();
        place_tree(
            &from,
            &to,
            no_runs().path(),
            &counts,
            &Staging::beside(),
            later(),
            None,
        )
        .unwrap();
        let placed = to.join("out/bindings.rs");
        assert_ne!(
            std::fs::metadata(&generated).unwrap().ino(),
            std::fs::metadata(&placed).unwrap().ino()
        );
        assert_eq!(counts.linked.load(Ordering::Relaxed), 0);
        assert_eq!(
            std::fs::metadata(&placed).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_on_another_filesystem_is_copied() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let Ok(other) = tempfile::tempdir_in("/dev/shm") else {
            eprintln!("skipping: no /dev/shm");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        if std::fs::metadata(other.path()).unwrap().dev()
            == std::fs::metadata(dir.path()).unwrap().dev()
        {
            eprintln!("skipping: /dev/shm is on the same filesystem");
            return;
        }
        let blob = other.path().join("libdep-0123456789abcdef.rmeta");
        write(&blob, "rmeta");
        std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o444)).unwrap();
        let metadata = std::fs::metadata(&blob).unwrap();
        let to = dir.path().join("libdep-0123456789abcdef.rmeta");
        let counts = FileCounts::default();
        place_file(&blob, Path::new(""), &to, &metadata, &counts).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "rmeta");
        assert_eq!(counts.linked.load(Ordering::Relaxed), 0);
        assert_eq!(counts.copied.load(Ordering::Relaxed), 1);
        let placed = std::fs::metadata(&to).unwrap();
        assert_eq!(placed.permissions().mode() & 0o777, 0o444);
        assert_eq!(placed.mtime(), metadata.mtime());
    }

    #[cfg(windows)]
    #[test]
    fn a_copied_file_can_be_made_readonly_after_setting_its_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("source");
        write(&from, "binary");
        let recorded = dir.path().join("recorded");
        write(&recorded, "");
        let mut permissions = std::fs::metadata(&recorded).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&recorded, permissions).unwrap();
        let to = dir.path().join("copy");
        place_file(
            &from,
            Path::new(""),
            &to,
            &std::fs::metadata(&recorded).unwrap(),
            &FileCounts::default(),
        )
        .unwrap();
        assert!(std::fs::metadata(&to).unwrap().permissions().readonly());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "binary");
    }

    #[test]
    fn a_target_built_while_waiting_for_its_lock_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let donor = donor(dir.path(), "a", Layout::Shared);
        let new = checkout(dir.path(), "b");
        write(&new.target_dir.join(".rustc_info.json"), "{}");
        assert_eq!(
            seed(
                &new,
                VERSION,
                std::slice::from_ref(&donor),
                no_runs().path(),
                later()
            ),
            Seeded::default()
        );
        assert!(!new.target_dir.join(PROFILE).join(".fingerprint").exists());
    }

    #[test]
    fn reads_the_compiler_a_target_was_built_by() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path();
        assert!(!built_by(target, VERSION));
        write(&target.join(".rustc_info.json"), "not json");
        assert!(!built_by(target, VERSION));
        write(&target.join(".rustc_info.json"), "{\"outputs\": 3}");
        assert!(!built_by(target, VERSION));
        let info =
            serde_json::json!({"outputs": {"a": {"stdout": "other"}, "b": {"stdout": VERSION}}});
        write(&target.join(".rustc_info.json"), &info.to_string());
        assert!(built_by(target, VERSION));
        assert!(!built_by(target, "rustc 2"));
    }

    #[test]
    fn locks_exclusively() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".cargo-lock");
        let first = try_lock(&path).unwrap();
        assert!(try_lock(&path).is_none());
        // Stands in for a child forked while the lock was held.
        let duplicate = first.0.try_clone().unwrap();
        drop(first);
        assert!(try_lock(&path).is_some(), "the lock was not released");
        drop(duplicate);
        assert!(try_lock(&dir.path().join("missing/.cargo-lock")).is_none());
    }

    #[test]
    fn donor_locks_are_shared_with_readers_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".cargo-lock");
        let first = try_lock_shared(&path).unwrap();
        let second = try_lock_shared(&path).unwrap();
        assert!(
            try_lock(&path).is_none(),
            "a build got in while seedings read"
        );
        drop((first, second));
        let building = try_lock(&path).unwrap();
        assert!(
            try_lock_shared(&path).is_none(),
            "a seeding read a donor being built"
        );
        drop(building);
        assert!(
            try_lock_shared(&path).is_some(),
            "the lock was not released"
        );
    }

    #[test]
    fn asks_the_probed_compiler_for_its_version() {
        let version = rustc_version(Some("rustc")).expect("rustc on the test PATH");
        assert!(version.starts_with("rustc "), "{version}");
        assert!(version.contains("host: "), "{version}");
        assert_eq!(rustc_version(None), None);
        assert_eq!(rustc_version(Some("/nonexistent/rustc")), None);
        #[cfg(unix)]
        assert_eq!(rustc_version(Some("false")), None);
        assert_eq!(SEED_DEADLINE, Duration::from_secs(10));
    }

    /// One Cargo unit in a shared-layout profile: its fingerprint directory
    /// with a hash file and JSON naming its dependencies by fingerprint, and
    /// its `deps/` outputs.
    fn fingerprinted_unit(
        profile: &Path,
        package: &str,
        hash: &str,
        fingerprint: u64,
        needs: &[u64],
        outputs: &[&str],
    ) {
        let dir = profile
            .join(".fingerprint")
            .join(format!("{package}-{hash}"));
        write(&dir.join(format!("lib-{package}")), &hex_le(fingerprint));
        let deps: Vec<_> = needs
            .iter()
            .map(|need| serde_json::json!([1, "d", false, need]))
            .collect();
        write(
            &dir.join(format!("lib-{package}.json")),
            &serde_json::json!({"deps": deps}).to_string(),
        );
        write(&dir.join("invoked.timestamp"), "");
        for ext in outputs {
            write(
                &profile
                    .join("deps")
                    .join(format!("lib{package}-{hash}.{ext}")),
                "x",
            );
        }
    }

    /// A check build needs what the donor's checked workspace units reach,
    /// through checked and built dependencies alike, and not what only a
    /// built (test) workspace unit reaches.
    #[test]
    fn a_check_needs_what_the_checked_workspace_units_reach() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("debug");
        let h = |n: u8| format!("{n:016x}");
        // Registry: `dep` checked, `dep` built (for the test), `mac` a proc
        // macro the check uses, `mac_dep` built for it, `only_test` built for
        // the test alone.
        fingerprinted_unit(&profile, "dep", &h(1), 11, &[], &["rmeta"]);
        fingerprinted_unit(&profile, "dep", &h(2), 12, &[], &["rlib", "rmeta"]);
        fingerprinted_unit(&profile, "mac_dep", &h(3), 13, &[], &["rlib", "rmeta"]);
        fingerprinted_unit(&profile, "mac", &h(4), 14, &[13], &["so"]);
        fingerprinted_unit(&profile, "only_test", &h(5), 15, &[12], &["rlib", "rmeta"]);
        // Workspace: `app` checked, and built as a test.
        fingerprinted_unit(&profile, "app", &h(6), 16, &[11, 14], &["rmeta"]);
        fingerprinted_unit(
            &profile,
            "app",
            &h(7),
            17,
            &[12, 14, 15],
            &["rlib", "rmeta"],
        );
        let registry: BTreeSet<String> = ["dep", "mac", "mac_dep", "only_test"]
            .map(String::from)
            .into();
        let deps = deps_by_hash(&profile.join("deps"));
        let demand = check_demand(&profile, &registry, &deps).unwrap();
        for wanted in [
            format!("dep-{}", h(1)),
            format!("mac-{}", h(4)),
            format!("mac_dep-{}", h(3)),
        ] {
            assert!(demand.contains(&wanted), "{wanted}");
        }
        for unwanted in [format!("dep-{}", h(2)), format!("only_test-{}", h(5))] {
            assert!(!demand.contains(&unwanted), "{unwanted}");
        }
    }

    /// An example's check is a root of a check's demand, as a library's is,
    /// and is seeded with its outputs, which Cargo keeps in `examples/`, not
    /// `deps/`: there the copy finds them, puts them and rebases their
    /// dep-info. Cargo's uplifted copy without a hash is no unit's.
    #[test]
    fn a_checked_example_is_seeded_with_its_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let h = |n: u8| format!("{n:016x}");
        let donor = empty_donor(dir.path(), "d");
        let profile = donor.target_dir.join(PROFILE);
        fingerprinted_unit(&profile, "dep", &h(1), 11, &[], &["rmeta"]);
        fingerprinted_unit(&profile, "dep", &h(2), 12, &[], &["rlib", "rmeta"]);
        fingerprinted_unit(&profile, "app", &h(3), 13, &[11], &["rmeta"]);
        // `app`'s example `demo`, checked against `app` and the built `dep`.
        fingerprinted_unit(&profile, "app", &h(4), 14, &[13, 12], &[]);
        let examples = profile.join("examples");
        let rmeta = examples.join(format!("libdemo-{}.rmeta", h(4)));
        write(&rmeta, "x");
        write(
            &examples.join(format!("demo-{}.d", h(4))),
            &format!("{}: examples/demo.rs\n", rmeta.display()),
        );
        write(&examples.join("demo"), "uplifted");
        let registry: BTreeSet<String> = ["dep"].map(String::from).into();
        let demand = check_demand(&profile, &registry, &outputs_by_hash(&profile)).unwrap();
        for wanted in [h(1), h(2), h(3), h(4)] {
            assert!(demand.iter().any(|unit| unit.ends_with(&wanted)), "{wanted}");
        }
        assert!(check_demand(&profile, &registry, &deps_by_hash(&profile.join("deps")))
            .unwrap()
            .iter()
            .all(|unit| !unit.ends_with(&h(2)) && !unit.ends_with(&h(4))));

        let same = NewTarget {
            target_dir: donor.workspace_root.join("target/integrate-clippy"),
            workspace_root: donor.workspace_root.clone(),
        };
        let stored = cache_storing(&dir.path().join("cache"), &["x"]);
        let seeded = seed_for_lane(
            &same,
            VERSION,
            std::slice::from_ref(&donor),
            &stored,
            &no_recorded_hashes,
            SeedDeadline::fixed(later()),
            true,
        );
        assert_eq!(seeded.units, 4);
        let to = same.target_dir.join(PROFILE);
        let seeded_rmeta = to.join("examples").join(format!("libdemo-{}.rmeta", h(4)));
        assert_eq!(std::fs::read_to_string(&seeded_rmeta).unwrap(), "x");
        let dep_info =
            std::fs::read_to_string(to.join("examples").join(format!("demo-{}.d", h(4)))).unwrap();
        assert!(
            dep_info.starts_with(&format!("{}: ", seeded_rmeta.display())),
            "{dep_info}"
        );
        assert!(!to.join("examples/demo").exists());
        assert!(!to.join("deps").join(format!("libdemo-{}.rmeta", h(4))).exists());
    }

    /// Units are taken until the deadline, and past it while one keeps
    /// being copied at least every [`SEED_STALL`], until the limit.
    #[test]
    fn past_the_deadline_units_are_taken_while_they_keep_coming() {
        let second = |n: u64| Duration::from_secs(n);
        let progress = Progress::new(SeedDeadline::from_start(Instant::now()));
        let at = |n: u64| progress.started + second(n);
        assert!(progress.takes_more(at(9)));
        // Nothing copied since the copy began.
        assert!(!progress.takes_more(at(11)));
        progress.copied(at(20));
        assert!(progress.takes_more(at(21)));
        assert!(!progress.takes_more(at(20) + SEED_STALL));
        progress.copied(at(59));
        assert!(progress.takes_more(at(60) - Duration::from_millis(1)));
        assert!(!progress.takes_more(at(60)));
        // A copy from another thread that ran late moves nothing back.
        progress.copied(at(30));
        assert!(progress.takes_more(at(60) - Duration::from_millis(1)));
        assert_eq!(SEED_LIMIT, second(60));
    }

    /// A seeding whose deadline passed before the copy began still copies
    /// what it can before its limit, and none past the limit.
    #[test]
    fn a_seeding_past_its_deadline_copies_until_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let donor = donor(dir.path(), "a", Layout::Shared);
        let seed_by = |name: &str, deadline: SeedDeadline| {
            seed_for_lane(
                &checkout(dir.path(), name),
                VERSION,
                std::slice::from_ref(&donor),
                no_runs().path(),
                &no_recorded_hashes,
                deadline,
                false,
            )
        };
        let passed = Instant::now();
        let late = seed_by(
            "late",
            SeedDeadline {
                soft: passed,
                limit: later(),
            },
        );
        assert_eq!((late.units, late.left), (1, 0));
        assert_eq!(seed_by("over", SeedDeadline::fixed(passed)), Seeded::default());
    }

    /// A checkout at `root/name` with `LOCK`, whose target `VERSION` built:
    /// no units yet ([`fingerprinted_unit`] adds them).
    fn empty_donor(root: &Path, name: &str) -> Donor {
        let workspace_root = root.join(name);
        write(
            &workspace_root.join("Cargo.toml"),
            "[package]\nname = \"app\"\n",
        );
        write(&workspace_root.join("Cargo.lock"), LOCK);
        let target_dir = workspace_root.join("target");
        let info = serde_json::json!({"outputs": {"1": {"stdout": VERSION}}});
        write(&target_dir.join(".rustc_info.json"), &info.to_string());
        Donor {
            target_dir,
            workspace_root,
            rustc: None,
        }
    }

    /// A check build goes to the donor whose check gives it the most units,
    /// not the one holding the most: a target that also built tests holds
    /// more units, most of which a check never reads.
    #[test]
    fn a_check_build_takes_the_donor_whose_check_gives_the_most() {
        let dir = tempfile::tempdir().unwrap();
        let h = |n: u8| format!("{n:016x}");
        // Used last: `dep` and `gitdep` built for a test, `dep` checked, and
        // the workspace's check read only the checked `dep`.
        let tests = empty_donor(dir.path(), "tests");
        let profile = tests.target_dir.join(PROFILE);
        fingerprinted_unit(&profile, "dep", &h(1), 11, &[], &["rlib", "rmeta"]);
        fingerprinted_unit(&profile, "gitdep", &h(2), 12, &[], &["rlib", "rmeta"]);
        fingerprinted_unit(&profile, "dep", &h(3), 13, &[], &["rmeta"]);
        fingerprinted_unit(&profile, "app", &h(4), 14, &[13], &["rmeta"]);
        fingerprinted_unit(&profile, "app", &h(5), 15, &[11, 12], &["rlib", "rmeta"]);
        // Used before: a check of the whole workspace, `dep` and `gitdep`.
        let checks = empty_donor(dir.path(), "checks");
        let profile = checks.target_dir.join(PROFILE);
        fingerprinted_unit(&profile, "dep", &h(3), 13, &[], &["rmeta"]);
        fingerprinted_unit(&profile, "gitdep", &h(6), 16, &[], &["rmeta"]);
        fingerprinted_unit(&profile, "app", &h(7), 17, &[13, 16], &["rmeta"]);
        let donors = [tests.clone(), checks.clone()];
        let stored = cache_storing(&dir.path().join("cache"), &["x"]);
        let check = checkout(dir.path(), "check");
        assert_eq!(
            seed_for_lane(
                &check,
                VERSION,
                &donors,
                &stored,
                &no_recorded_hashes,
                SeedDeadline::fixed(later()),
                true
            ),
            Seeded {
                units: 2,
                donor: Some(checks.workspace_root.clone()),
                ..Seeded::default()
            }
        );
        // A build still takes the donor holding the most.
        let build = checkout(dir.path(), "build");
        assert_eq!(
            seed_for_lane(
                &build,
                VERSION,
                &donors,
                &stored,
                &no_recorded_hashes,
                SeedDeadline::fixed(later()),
                false
            ),
            Seeded {
                units: 3,
                donor: Some(tests.workspace_root.clone()),
                ..Seeded::default()
            }
        );
    }

    /// A check donor with no checked workspace unit gives every unit, but
    /// counts only those a check compiled: its metadata-only ones.
    #[test]
    fn a_donor_without_a_workspace_check_counts_its_checked_units() {
        let dir = tempfile::tempdir().unwrap();
        let h = |n: u8| format!("{n:016x}");
        let profile = dir.path().join("debug");
        fingerprinted_unit(&profile, "dep", &h(1), 11, &[], &["rlib", "rmeta"]);
        fingerprinted_unit(&profile, "dep", &h(2), 12, &[], &["rmeta"]);
        fingerprinted_unit(&profile, "app", &h(3), 13, &[11], &["rlib", "rmeta"]);
        let registry: BTreeSet<String> = ["dep"].map(String::from).into();
        assert_eq!(check_units(&profile, &registry, &registry), 1);
        fingerprinted_unit(&profile, "app", &h(4), 14, &[11, 12], &["rmeta"]);
        assert_eq!(check_units(&profile, &registry, &registry), 2);
    }

    /// Without a checked workspace unit, nothing says what a check needs.
    #[test]
    fn no_checked_workspace_unit_gives_no_demand() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("debug");
        fingerprinted_unit(
            &profile,
            "dep",
            &format!("{:016x}", 2),
            12,
            &[],
            &["rlib", "rmeta"],
        );
        fingerprinted_unit(
            &profile,
            "app",
            &format!("{:016x}", 7),
            17,
            &[12],
            &["rlib", "rmeta"],
        );
        let registry: BTreeSet<String> = ["dep"].map(String::from).into();
        let deps = deps_by_hash(&profile.join("deps"));
        assert!(check_demand(&profile, &registry, &deps).is_none());
    }

    #[test]
    fn the_subcommand_skips_options_and_toolchains() {
        let line = |text: &str| args(text);
        assert_eq!(
            cargo_subcommand(&line("cargo +nightly -q check --workspace")),
            Some("check")
        );
        assert_eq!(
            cargo_subcommand(&line("/usr/bin/cargo clippy")),
            Some("clippy")
        );
        assert_eq!(cargo_subcommand(&line("cargo test --no-run")), Some("test"));
        assert_eq!(cargo_subcommand(&line("cargo")), None);
    }
}
