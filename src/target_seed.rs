//! Registry build units copied into a new checkout's target directory before
//! Cargo decides what to build.
//!
//! The first thing Cargo sends a new target directory's `RUSTC_WRAPPER` is
//! its target-info probe (`rustc - --crate-name ___ --print=file-names ...`),
//! before it takes the build lock and before it checks any unit's freshness.
//! Cargo caches the answer in `<target>/.rustc_info.json`, so a target
//! directory without that file is one Cargo has not built yet. The wrapper
//! seeds it before it answers the probe, for up to [`SEED_DEADLINE`]: it
//! copies the units another checkout already built for the same registry and
//! git packages, and Cargo then finds them fresh and skips them. The wrapper
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
//! - Only packages with a `source` in `Cargo.lock` are copied. A path
//!   package's fingerprint trusts its sources' modification times, so a copy
//!   from a checkout with different sources could pass as fresh.
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

/// How long seeding copies for. Copying stops at this deadline, between
/// files, and Cargo builds whatever was not copied.
///
/// A unit left out costs far more later than copying it now: the wrapper
/// restores it from the store in a compiler call of its own (about 50 ms of
/// CPU each in a 1,300-unit workspace, against about 1 ms to copy). On a
/// loaded machine 3 s cut seedings of 1,400-1,500 units to 78-166.
pub(crate) const SEED_DEADLINE: Duration = Duration::from_secs(10);

/// Units copied at once. A copy is mostly metadata calls, which overlap: on
/// btrfs, 1159 units took 2.9 s on one thread, 0.6 s on four and 0.5 s on
/// eight.
const COPY_THREADS: usize = 4;

/// The profile directory seeded.
const PROFILE: &str = "debug";

/// Where a unit is copied before it is renamed into place.
const STAGING: &str = ".kache-seeding-";

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

/// The donor's units of `packages`.
fn units(profile: &Path, layout: Layout, packages: &BTreeSet<String>) -> Vec<Unit> {
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

/// A path into the donor in the unit's recorded build-script output, other
/// than its own `OUT_DIR`, which Cargo rewrites when it replays the output.
fn names_the_donor(profile: &Path, layout: Layout, unit: &Unit, donor: &Donor) -> bool {
    let run = match layout {
        Layout::PerUnit => profile
            .join("build")
            .join(&unit.package)
            .join(&unit.hash)
            .join("run"),
        Layout::Shared => profile
            .join("build")
            .join(format!("{}-{}", unit.package, unit.hash)),
    };
    let stdout = match layout {
        Layout::PerUnit => run.join("stdout"),
        Layout::Shared => run.join("output"),
    };
    let Ok(output) = std::fs::read(stdout) else {
        return false;
    };
    let output = String::from_utf8_lossy(&output);
    let out_dir = std::fs::read_to_string(run.join("root-output")).unwrap_or_default();
    let rest = match out_dir.trim() {
        "" => output.into_owned(),
        out_dir => output.replace(out_dir, ""),
    };
    donor_needles(donor)
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
    donor: &Donor,
    deadline: Instant,
) -> bool {
    let stdout = match layout {
        Layout::PerUnit => profile
            .join("build")
            .join(&unit.package)
            .join(&unit.hash)
            .join("run/stdout"),
        Layout::Shared => profile
            .join("build")
            .join(format!("{}-{}", unit.package, unit.hash))
            .join("output"),
    };
    if !stdout.is_file() {
        return false;
    }
    let needles = donor_needles(donor);
    OutScan::new(&needles, deadline).keeps_out(&build_script_out_dir(profile, layout, unit))
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

/// Whether every artifact of the unit, what a dependent's `--extern` reads,
/// is byte for byte a blob in this cache. A dependent's key hashes those
/// bytes, and another kache or a build without kache writes different ones
/// for the same Cargo unit; such a unit is left for Cargo to build through
/// the cache.
fn artifacts_stored(
    from: &Path,
    layout: Layout,
    unit: &Unit,
    deps: &HashMap<String, Vec<PathBuf>>,
    cache_dir: &Path,
    deadline: Instant,
) -> bool {
    let mut files = match layout {
        Layout::PerUnit => vec![from.join("build").join(&unit.package).join(&unit.hash)],
        Layout::Shared => deps.get(&unit.hash).cloned().unwrap_or_default(),
    };
    let store = cache_dir.join("store");
    while let Some(file) = files.pop() {
        if Instant::now() >= deadline {
            return false;
        }
        if std::fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_dir()) {
            files.extend(entries(&file));
        } else if file.extension().is_some_and(|ext| {
            ["rlib", "rmeta", "dylib", "so", "dll"].contains(&&*ext.to_string_lossy())
        }) && !kache_store::file_hash::hash_file(&file)
            .is_ok_and(|hash| kache_store::blob_path_in_store_dir(&store, &hash).is_file())
        {
            return false;
        }
    }
    true
}

/// Copy one unit from the donor profile `from` to `to`. The unit's marker
/// goes last, by rename. `deps` is [`deps_by_hash`] of the donor's `deps/`
/// for the shared layout.
fn copy_unit(
    from: &Path,
    to: &Path,
    layout: Layout,
    unit: &Unit,
    deps: &HashMap<String, Vec<PathBuf>>,
    cache_dir: &Path,
    counts: &FileCounts,
    deadline: Instant,
) -> std::io::Result<()> {
    let relocation = relocation::Relocation::for_unit(from, to, layout, unit, deadline)?;
    #[cfg(unix)]
    {
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
            std::fs::create_dir_all(&package)?;
            place_tree(
                &from.join("build").join(&unit.package).join(&unit.hash),
                &package.join(&unit.hash),
                cache_dir,
                counts,
                deadline,
                Some(&relocation),
            )
        }
        Layout::Shared => {
            let named = format!("{}-{}", unit.package, unit.hash);
            let to_deps = to.join("deps");
            std::fs::create_dir_all(&to_deps)?;
            for file in deps.get(&unit.hash).into_iter().flatten() {
                let name = file_name(file);
                if !to_deps.join(&name).exists() {
                    place_tree(
                        file,
                        &to_deps.join(&name),
                        cache_dir,
                        counts,
                        deadline,
                        Some(&relocation),
                    )?;
                }
            }
            let build = from.join("build").join(&named);
            if build.is_dir() {
                std::fs::create_dir_all(to.join("build"))?;
                place_tree(
                    &build,
                    &to.join("build").join(&named),
                    cache_dir,
                    counts,
                    deadline,
                    Some(&relocation),
                )?;
            }
            std::fs::create_dir_all(to.join(".fingerprint"))?;
            place_tree(
                &from.join(".fingerprint").join(&named),
                &to.join(".fingerprint").join(&named),
                cache_dir,
                counts,
                deadline,
                Some(&relocation),
            )
        }
    }
}

/// Copy `from` (a file or a directory) beside `to` and rename it there.
fn place_tree(
    from: &Path,
    to: &Path,
    cache_dir: &Path,
    counts: &FileCounts,
    deadline: Instant,
    relocation: Option<&relocation::Relocation>,
) -> std::io::Result<()> {
    let staging = to.with_file_name(format!("{STAGING}{}-{}", file_name(to), std::process::id()));
    let placed = std::sync::Mutex::new(Vec::new());
    let tree = Placing {
        from,
        to,
        cache_dir,
        counts,
        placed: &placed,
        relocation,
    };
    // Held past the rename, so a sweep keeps each run the tree links to until
    // its recorded `OUT_DIR` does.
    let mut runs = Vec::new();
    let copied = copy_tree(from, &staging, tree, &mut runs, deadline)
        .and_then(|()| std::fs::rename(&staging, to));
    if copied.is_ok() {
        let placed = placed.into_inner().unwrap_or_default();
        let mut all = counts.placed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        all.extend(placed.into_iter().map(|mut file: PlacedFile| {
            file.at = match file.at.strip_prefix(&staging) {
                Ok(below) if below.as_os_str().is_empty() => to.to_path_buf(),
                Ok(below) => to.join(below),
                Err(_) => file.at,
            };
            file
        }));
    }
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&staging);
    }
    copied
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
    relocation: Option<&'a relocation::Relocation>,
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

/// Copy `from`, a file or directory in `tree`, to `to`, keeping every file's
/// modification time. A symbolic link that stays inside the tree is copied
/// as a link, and a build script's `OUT_DIR` linked to its sealed hermetic
/// run is linked to the same run; any other symbolic link fails the copy,
/// and so does reaching `deadline` before a file. The runs linked to are
/// added to `runs`, held.
#[cfg_attr(not(unix), allow(clippy::only_used_in_recursion))]
fn copy_tree(
    from: &Path,
    to: &Path,
    tree: Placing,
    runs: &mut Vec<HeldRun>,
    deadline: Instant,
) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        std::fs::create_dir(to)?;
        for entry in entries(from) {
            copy_tree(&entry, &to.join(file_name(&entry)), tree, runs, deadline)?;
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
    }
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a file or a directory",
            from.display()
        )));
    }
    let below = from.strip_prefix(tree.from).unwrap_or(Path::new(""));
    // Dep-info and a build script's records name the donor's paths: they
    // are written anew for this target, never linked, and never memoised
    // as the donor's bytes.
    let contents = tree
        .relocation
        .map(|relocation| relocation.contents(from, deadline))
        .transpose()?
        .flatten();
    if let Some(contents) = contents {
        std::fs::write(to, &contents)?;
        tree.counts.copied.fetch_add(1, Ordering::Relaxed);
        return stamp(to, &metadata);
    }
    place_file(from, below, to, &metadata, tree.counts)?;
    if metadata.len() >= crate::target_dedup::MIN_BYTES {
        let before = kache_store::file_hash::FileFingerprint {
            path: from.to_string_lossy().into_owned(),
            size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            mtime_ns: kache_store::file_hash::metadata_mtime_ns(&metadata),
            ctime_ns: kache_store::file_hash::metadata_ctime_ns(&metadata),
            inode: kache_store::file_hash::metadata_inode(&metadata),
        };
        tree.placed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(PlacedFile {
                from: from.to_path_buf(),
                before,
                at: to.to_path_buf(),
            });
    }
    Ok(())
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

/// Files a seeding gave by hard link and by copy or clone.
#[derive(Debug, Default)]
struct FileCounts {
    linked: AtomicUsize,
    copied: AtomicUsize,
    /// Files placed for good, at their final paths.
    placed: std::sync::Mutex<Vec<PlacedFile>>,
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
    if kache_store::link::try_reflink(from, to).is_err() {
        #[cfg(unix)]
        if shares_inode(below, metadata) && link_read_only(from, to) {
            counts.linked.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        std::fs::copy(from, to)?;
    }
    counts.copied.fetch_add(1, Ordering::Relaxed);
    stamp(to, metadata)
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

/// What a seeding did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Seeded {
    pub(crate) units: usize,
    pub(crate) donor: Option<PathBuf>,
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
    seed_for_lane(target, rustc_version, donors, cache_dir, deadline, false)
}

/// [`seed`], and for a build that only checks (`check_lane`), only the units
/// a check of the donor's workspace used ([`check_demand`]). Without such a
/// record in the donor, every unit, as [`seed`].
pub(crate) fn seed_for_lane(
    target: &NewTarget,
    rustc_version: &str,
    donors: &[Donor],
    cache_dir: &Path,
    deadline: Instant,
    check_lane: bool,
) -> Seeded {
    let Ok(lockfile) = std::fs::read_to_string(target.workspace_root.join("Cargo.lock")) else {
        return Seeded::default();
    };
    let packages = registry_packages(&lockfile);
    if packages.is_empty() || rustc_version.trim().is_empty() {
        return Seeded::default();
    }
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
    for (donor, layout) in ranked_donors(target, rustc_version, donors, &packages, check_lane) {
        if Instant::now() >= deadline {
            break;
        }
        let from = donor.target_dir.join(PROFILE);
        let Some(_theirs) = try_lock_shared(&from.join(".cargo-lock")) else {
            continue;
        };
        let deps = match layout {
            Layout::Shared => deps_by_hash(&from.join("deps")),
            Layout::PerUnit => HashMap::new(),
        };
        let mut units = units(&from, layout, &packages);
        if check_lane
            && layout == Layout::Shared
            && let Some(demand) = check_demand(&from, &packages, &deps)
        {
            let all = units.len();
            units.retain(|unit| demand.contains(&format!("{}-{}", unit.package, unit.hash)));
            tracing::info!(
                "check build: seeding {} of {all} registry units the donor's check used",
                units.len()
            );
        }
        let next = AtomicUsize::new(0);
        let copied = AtomicUsize::new(0);
        let files = FileCounts::default();
        let cancellation = crate::invocation::process::current();
        std::thread::scope(|scope| {
            for _ in 0..COPY_THREADS {
                let cancellation = cancellation.clone();
                let (units, next, copied, from, to, deps, files) =
                    (&units, &next, &copied, &from, &to, &deps, &files);
                scope.spawn(move || {
                    let _cancellation =
                        crate::invocation::process::CancellationScope::borrow(cancellation);
                    while let Some(unit) = units.get(next.fetch_add(1, Ordering::Relaxed)) {
                        if Instant::now() >= deadline
                            || crate::invocation::process::check().is_err()
                        {
                            break;
                        }
                        if marker(to, layout, unit).exists()
                            || names_the_donor(from, layout, unit, donor)
                            || out_dir_names_the_donor(from, layout, unit, donor, deadline)
                            || !artifacts_stored(from, layout, unit, deps, cache_dir, deadline)
                        {
                            continue;
                        }
                        match copy_unit(from, to, layout, unit, deps, cache_dir, files, deadline) {
                            Ok(()) => {
                                copied.fetch_add(1, Ordering::Relaxed);
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
                });
            }
        });
        let copied = copied.into_inner();
        if copied > 0 {
            return Seeded {
                units: copied,
                donor: Some(donor.workspace_root.clone()),
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
fn ranked_donors<'a>(
    target: &'a NewTarget,
    rustc_version: &'a str,
    donors: &'a [Donor],
    packages: &BTreeSet<String>,
    check_lane: bool,
) -> impl Iterator<Item = (&'a Donor, Layout)> + 'a {
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
                    if check_lane && layout == Layout::Shared {
                        check_units(&profile, packages)
                    } else {
                        units(&profile, layout, packages).len()
                    }
                })
            })
            .collect();
        candidates
            .iter()
            .zip(counts)
            .map(|(&(donor, layout), count)| (donor, layout, count.join().unwrap_or(0)))
            .collect()
    });
    // Stable: equals keep their order of use.
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    ranked
        .into_iter()
        .map(|(donor, layout, _)| (donor, layout))
        .chain(usable)
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
fn check_units(profile: &Path, packages: &BTreeSet<String>) -> usize {
    let deps = deps_by_hash(&profile.join("deps"));
    let held = units(profile, Layout::Shared, packages);
    match check_demand(profile, packages, &deps) {
        Some(demand) => held
            .iter()
            .filter(|unit| demand.contains(&format!("{}-{}", unit.package, unit.hash)))
            .count(),
        None => held
            .iter()
            .filter(|unit| metadata_only(&deps, &unit.hash))
            .count(),
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

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    entries(dir)
        .into_iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()))
        .collect()
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
            } else if file_name != "invoked.timestamp"
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
                Layout::PerUnit => 2,
                Layout::Shared => 5,
            };
            assert_eq!(copied.len(), expected, "{layout:?}: {copied:?}");
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
                later()
            ));
            assert!(!artifacts_stored(
                &profile,
                layout,
                &unit,
                &deps,
                &cache,
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
                    !names_the_donor(&from, layout, &dep, &donor),
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
                    names_the_donor(&from, layout, &dep, &donor),
                    named,
                    "{layout:?}: {text}"
                );
            }
            std::fs::remove_file(run.join(stdout)).unwrap();
            assert!(!names_the_donor(&from, layout, &dep, &donor));
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
                !names_the_donor(&from, layout, &dep, &donor),
                "{layout:?}: stdout only names OUT_DIR"
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
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
                !out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
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
                out_dir_names_the_donor(&from, layout, &dep, &named, later()),
                "{layout:?} workspace"
            );
            write(&cache, "see /needle-target/debug\n");
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &named, later()),
                "{layout:?} target"
            );
            write(&cache, "see /neither\n");
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &named, later()),
                "{layout:?}"
            );
            // Longer than both needles, and neither needle is in it. A
            // shorter miss makes `windows` empty, so `==` and `!=` agree.
            write(&cache, "xxxx see /neither xxxxxxxxx\n");
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &named, later()),
                "{layout:?}: a longer file that names neither path"
            );

            let mut blank = donor.clone();
            blank.target_dir.clear();
            blank.workspace_root.clear();
            write(&cache, "x\n");
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &blank, later()),
                "{layout:?} empty"
            );

            let needle = donor.target_dir.to_string_lossy().into_owned();
            std::fs::remove_file(script_stdout(&from, layout)).unwrap();
            write(&cache, &format!("{needle}\n"));
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
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
                    out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
                    "{layout:?}: an unreadable output file is not copied"
                );
                std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();

                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o000)).unwrap();
                assert!(
                    out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
                    "{layout:?}: an unlisted out/ is not copied"
                );
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();

                let outside = dir.path().join("outside.txt");
                write(&outside, &needle);
                write(&cache, "portable-cache\n");
                let link = out.join("linked-cache");
                std::os::unix::fs::symlink(&outside, &link).unwrap();
                assert!(
                    !out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
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
                !names_the_donor(&from, layout, &dep, &windows),
                "{layout:?}"
            );
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &windows, later()),
                "{layout:?}"
            );

            write(
                &cache,
                "CMAKE_CACHEFILE_DIR:INTERNAL=D:/target/ws-a/debug/build/dep/out/build\n",
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &windows, later()),
                "{layout:?}: the target directory"
            );
            write(
                &cache,
                "CMAKE_HOME_DIRECTORY:INTERNAL=C:/Users/me/ws-a/vendor\n",
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &windows, later()),
                "{layout:?}: the workspace"
            );
            write(
                &cache,
                r#"include_bytes!("D:\\target\\ws-a\\debug\\build\\dep\\out\\t.bin")"#,
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &windows, later()),
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
                    out_dir_names_the_donor(&from, layout, &dep, &windows, later()),
                    named,
                    "{layout:?}: {text}"
                );
            }
            let mut lowercase = windows.clone();
            lowercase.workspace_root = PathBuf::from(r"c:\Users\me\ws-a");
            write(&cache, "C:/Users/me/ws-a/vendor\n");
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &lowercase, later()),
                "{layout:?}: a lowercase drive in the donor"
            );

            write(
                &stdout,
                "cargo:rustc-link-search=native=D:/target/ws-a/debug/gn_out/obj\n",
            );
            assert!(names_the_donor(&from, layout, &dep, &windows), "{layout:?}");
            write(
                &stdout,
                r#"cargo:warning=using "c:\\Users\\me\\ws-a\\vendor""#,
            );
            assert!(names_the_donor(&from, layout, &dep, &windows), "{layout:?}");
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
                    out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
                    "{layout:?}: at {at}"
                );
            }
            std::fs::File::create(&cache).unwrap().set_len(len).unwrap();
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
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
                out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
                "{layout:?}: past the budget"
            );
            file.set_len(0).unwrap();
            assert!(
                !out_dir_names_the_donor(&from, layout, &dep, &donor, later()),
                "{layout:?}"
            );
            assert!(
                out_dir_names_the_donor(&from, layout, &dep, &donor, Instant::now()),
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
        use crate::build_script::hermetic_test_support::{linked_to, sealed_run};
        const RUN: &str = "1111222233334444";
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
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
        std::fs::set_permissions(&recorded, std::fs::Permissions::from_mode(0o751)).unwrap();
        let to = dir.path().join("copy");
        place_file(
            &from,
            Path::new(""),
            &to,
            &std::fs::metadata(&recorded).unwrap(),
            &FileCounts::default(),
        )
        .unwrap();
        let mode = std::fs::metadata(&to).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o751);
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "binary");
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
        place_tree(&from, &to, no_runs().path(), &counts, later(), None).unwrap();
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
        place_tree(&from, &to, no_runs().path(), &counts, later(), None).unwrap();
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
            seed_for_lane(&check, VERSION, &donors, &stored, later(), true),
            Seeded {
                units: 2,
                donor: Some(checks.workspace_root.clone()),
                ..Seeded::default()
            }
        );
        // A build still takes the donor holding the most.
        let build = checkout(dir.path(), "build");
        assert_eq!(
            seed_for_lane(&build, VERSION, &donors, &stored, later(), false),
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
        assert_eq!(check_units(&profile, &registry), 1);
        fingerprinted_unit(&profile, "app", &h(4), 14, &[11, 12], &["rmeta"]);
        assert_eq!(check_units(&profile, &registry), 2);
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
