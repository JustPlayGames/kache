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

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
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
    /// Each path package's directory, relative to the workspace root, as
    /// this checkout's manifests place it.
    roots: HashMap<String, PathBuf>,
    /// What each package's directory comparison found, once made.
    packages: Mutex<HashMap<String, Option<PackageCheck>>>,
    deadline: Instant,
}

/// A package directory found the same in both checkouts.
#[derive(Debug, Clone, Copy)]
struct PackageCheck {
    /// When the comparison started: no later change to this checkout's
    /// copy went unseen by it.
    checked_from: SystemTime,
    /// The newest modification time of the donor's copy.
    donor_newest: SystemTime,
}

/// A unit that may be copied, and the time its dep-info takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Verified {
    pub(super) stamp: SystemTime,
}

impl CrossCheckout {
    /// The check for `donor_root`'s units of `packages`, or `None` when this
    /// checkout's manifests cannot be read by `deadline`.
    pub(super) fn new(
        donor_root: &Path,
        our_root: &Path,
        packages: &BTreeSet<String>,
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
            roots,
            packages: Mutex::new(HashMap::new()),
            deadline,
        })
    }

    /// Whether the donor's unit of `package`, whose fingerprint directory is
    /// `fingerprint`, was built from this checkout's bytes.
    pub(super) fn verify(&self, package: &str, fingerprint: &Path) -> Option<Verified> {
        let relative = self.roots.get(package)?;
        let check = self.package(package, relative)?;
        let dep_infos: Vec<PathBuf> = super::entries(fingerprint)
            .into_iter()
            .filter(|file| super::file_name(file).starts_with("dep-"))
            .collect();
        if dep_infos.is_empty() {
            return None;
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
                // Relative to the target: generated by the donor's run.
                if (file.kind == PathKind::TargetRoot && !path.is_absolute())
                    || Instant::now() >= self.deadline
                    || !self.outside_file_holds(&theirs, &ours, path, reference)
                {
                    return None;
                }
            }
        }
        Some(Verified {
            stamp: check.checked_from,
        })
    }

    /// Date and rebase the copied unit's dep-infos at `fingerprint`
    /// ([`date_and_rebase`]).
    pub(super) fn date(&self, fingerprint: &Path, stamp: SystemTime) -> std::io::Result<()> {
        date_and_rebase(fingerprint, &self.donor_root, &self.our_root, stamp)
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
            if path.starts_with(&self.our_root) && !path.starts_with(&self.donor_root) {
                return false;
            }
            let Ok(within) = path.strip_prefix(&self.donor_root) else {
                // The same file for both checkouts.
                return modified(path).is_some_and(|time| time <= reference);
            };
            let our_file = self.our_root.join(within);
            return within.components().all(|c| matches!(c, Component::Normal(_)))
                && modified(path).is_some_and(|time| time <= reference)
                && same_bytes(path, &our_file, self.deadline);
        }
        let (Some(their_file), Some(our_file)) = (normal(&theirs.join(path)), normal(&ours.join(path)))
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
        if let Some(known) = self.packages.lock().ok()?.get(package) {
            return *known;
        }
        let checked_from = SystemTime::now();
        let found = compare_trees(
            &self.donor_root.join(relative),
            &self.our_root.join(relative),
            self.deadline,
        )
        .map(|donor_newest| PackageCheck {
            checked_from,
            donor_newest,
        });
        self.packages
            .lock()
            .ok()?
            .insert(package.to_owned(), found);
        found
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
        if manifest.is_file()
            && let Some(name) = package_name(&manifest)
            && packages.contains(&name)
        {
            let relative = dir.strip_prefix(root).ok()?.to_path_buf();
            found
                .entry(name)
                .and_modify(|known| *known = None)
                .or_insert(Some(relative));
        }
        let Ok(listed) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listed.flatten() {
            seen += 1;
            if seen > MAX_MANIFEST_WALK {
                return None;
            }
            let name = entry.file_name();
            if SKIPPED_DIRS.iter().any(|skip| name == OsStr::new(skip)) {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(entry.path());
            }
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
        let their_entries = sorted_entries(&their_dir)?;
        if their_entries != sorted_entries(&our_dir)? {
            return None;
        }
        for name in their_entries {
            let (their_path, our_path) = (their_dir.join(&name), our_dir.join(&name));
            let their_meta = std::fs::symlink_metadata(&their_path).ok()?;
            let our_meta = std::fs::symlink_metadata(&our_path).ok()?;
            newest = newest.max(their_meta.modified().ok()?);
            let (their_kind, our_kind) = (their_meta.file_type(), our_meta.file_type());
            if their_kind.is_symlink() || our_kind.is_symlink() {
                if !(their_kind.is_symlink() && our_kind.is_symlink())
                    || std::fs::read_link(&their_path).ok()? != std::fs::read_link(&our_path).ok()?
                {
                    return None;
                }
            } else if their_kind.is_dir() && our_kind.is_dir() {
                pending.push((their_path, our_path));
            } else if their_kind.is_file() && our_kind.is_file() {
                if their_meta.len() != our_meta.len()
                    || !same_bytes(&their_path, &our_path, deadline)
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

fn sorted_entries(dir: &Path) -> Option<Vec<std::ffi::OsString>> {
    let mut names = std::fs::read_dir(dir)
        .ok()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .ok()?;
    names.sort();
    Some(names)
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
/// checkout's files where it named the donor's, in full, and give it the
/// time `stamp`. A dep-info that cannot be read or written is removed: the
/// unit is then built, as without seeding.
pub(super) fn date_and_rebase(
    fingerprint: &Path,
    donor_root: &Path,
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
                if let Ok(within) = Path::new(&entry.path).strip_prefix(donor_root) {
                    entry.path = our_root.join(within).to_str()?.to_owned();
                }
            }
            info.encode()
        });
        let written = match rebased {
            Some(bytes) => std::fs::write(&file, bytes)
                .and_then(|()| filetime::set_file_mtime(&file, time)),
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
