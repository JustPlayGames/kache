//! Import compatible local rustc state without sharing a writable session.

use super::*;
use std::io;

pub(super) const MARKER: &str = "worktree-seed-v1";
/// Under a profile's policy root: one file per published seed, named by its
/// key and holding its unit's key, so a receiver finds a donor's unit
/// without reading every unit's marker.
const SEEDS: &str = "seeds-v2";
/// Under a profile's policy root: this folder keeps every unit's graph for
/// others to start from (`[cache] keep_incremental = "all"`).
const KEEP_ALL: &str = "keep-all";
/// Under the cache folder: one file per profile kept for others, holding its
/// path. Such folders are tried as donors before any other.
const KEPT_DONORS: &str = "incremental-donors";
/// Donors without a seed index (an older release's) whose units a receiver
/// reads one by one.
const MAX_SCANNED_DONORS: usize = 8;
/// The fixed directory a private compile's cwd is remapped to, the same in
/// every worktree. It must be an absolute path on the host: on Windows a
/// path without a drive never qualifies, so it is `C:\src` there.
///
/// Elsewhere it is where a stored compile puts the workspace
/// ([`crate::path_normalizer::workspace_flag_target`]): a graph is only as
/// good as its upstream crates' metadata. A private compile of a library
/// differs from its stored compile in the crate hash alone when both name the
/// workspace alike, so a unit whose graph was built on the private one (a
/// folder that keeps every unit, above all) re-runs the queries on that
/// crate and finds the same answers. With `/src` every span of that crate
/// differed too, and an imported graph of a crate built on many of ours was
/// worth no more than a cold compile.
const REMAP_PREFIX: &str = if cfg!(windows) {
    r"C:\src"
} else if cfg!(target_os = "linux") {
    "/proc/self/cwd"
} else {
    "/kache/workspace"
};
const MAX_DONORS: usize = 256;
/// Next to a published seed: what its graph was made against
/// ([`ExternRecord`]).
const EXTERNS: &str = "externs-v1";
const MAX_COPY_FILES: usize = 262_144;
/// A test program's graph of a large crate, with its object files, is
/// several gigabytes.
const MAX_COPY_BYTES: u64 = 16 << 30;

impl Lease {
    /// Normalize only modeled path operands. An unsupported invocation keeps
    /// its ordinary target-local behavior. The donor is a Cargo profile path,
    /// for example `/work/previous/target/debug`, never a shared live graph.
    pub(crate) fn worktree_compiler_args(
        &mut self,
        args: &RustcArgs,
        donor_profile: Option<&Path>,
        enabled: bool,
    ) -> Vec<String> {
        let donors: Vec<_> = donor_profile.into_iter().map(Path::to_path_buf).collect();
        self.worktree_compiler_args_from(args, &donors, enabled)
    }

    pub(crate) fn worktree_compiler_args_from(
        &mut self,
        args: &RustcArgs,
        donor_profiles: &[PathBuf],
        enabled: bool,
    ) -> Vec<String> {
        self.worktree_seed_key = None;
        self.worktree_seed_externs = None;
        // An import stays this lease's, through a later preparation that
        // finds the state already there, and so does a skipped one.
        self.worktree_bootstrap = false;
        let isolated = self.compiler_args(args);
        if !enabled || args.inner_rustc.is_some() || path_exists(&self.bootstrap_env_marker()) {
            return isolated;
        }
        let Some(cwd) = args.invocation_env().cwd().map(Path::to_owned) else {
            return isolated;
        };
        let Some(crate_name) = args.crate_name.as_deref() else {
            return isolated;
        };
        let prefix = args
            .unstable_flags
            .iter()
            .find_map(|flag| flag.strip_prefix("remap-cwd-prefix="));
        let mut candidate = isolated.clone();
        if prefix.is_none() {
            candidate.push(format!("-Zremap-cwd-prefix={REMAP_PREFIX}"));
        }
        let prefix = prefix.unwrap_or(REMAP_PREFIX);
        let Some(mut normalized) = portable_args(&candidate, args.source_file.as_deref(), &cwd)
        else {
            return isolated;
        };
        // A mismatched invocation had incremental stripped by compiler_args.
        if strip_incremental_refs(&normalized).len() == normalized.len() {
            return isolated;
        }
        let current_bootstrap = args.invocation_env().var_os("RUSTC_BOOTSTRAP");
        let probe = |bootstrap, restrict| {
            crate::cache_key::rustc_accepts_cwd_remap_for(
                &args.rustc,
                crate_name,
                prefix,
                bootstrap,
                restrict,
                args.invocation_env(),
            )
        };
        let inject_bootstrap = match probe(current_bootstrap.as_deref(), false) {
            Some(true) => false,
            Some(false)
                if bootstrap_override_allowed(
                    current_bootstrap.as_deref(),
                    &args.unstable_flags,
                ) && probe(Some(std::ffi::OsStr::new("1")), true) == Some(true) =>
            {
                true
            }
            _ => return isolated,
        };
        let bootstrap = if inject_bootstrap {
            Some(std::ffi::OsStr::new("1"))
        } else {
            current_bootstrap.as_deref()
        };
        if inject_bootstrap {
            // Bootstrap permits cwd remapping, not unstable source features.
            normalized.push("-Zallow-features=".into());
        }
        let Some(identity) =
            crate::cache_key::rustc_version_text_for(&args.rustc, args.invocation_env())
                .filter(|text| !text.is_empty())
        else {
            return isolated;
        };
        let target = args
            .target_dir()
            .and_then(|target| relative(target.to_str()?, &cwd));
        let key = portable_key(
            args,
            &normalized,
            &self.unit.policy_guard,
            bootstrap,
            &identity,
            target.as_deref(),
        );
        let externs = ExternRecord::of(
            args,
            &cwd,
            ExternRecord::read(&self.unit.unit_dir.join(EXTERNS)).as_ref(),
        );
        if empty_directory(&self.unit.rustc_dir)
            && let Import::Copied {
                profile,
                other_libraries,
            } = import_from(&self.unit, donor_profiles, &key, externs.as_ref())
        {
            tracing::debug!(donor = %profile.display(), other_libraries, "imported worktree incremental seed");
            self.worktree_seed_donor = Some(profile);
            self.worktree_seed_other_libraries = other_libraries;
        }
        self.worktree_seed_externs = externs;
        self.worktree_seed_key = Some(key);
        self.worktree_bootstrap = inject_bootstrap;
        normalized
    }

    pub(crate) fn worktree_bootstrap(&self) -> bool {
        self.worktree_bootstrap
    }

    /// The other checkout's profile whose incremental state was imported.
    pub(crate) fn worktree_seed_donor(&self) -> Option<&Path> {
        self.worktree_seed_donor.as_deref()
    }

    /// Whether the imported graph was made against other builds of some
    /// libraries than this checkout's, as the donor's record shows
    /// ([`ExternRecord`]).
    pub(crate) fn worktree_seed_other_libraries(&self) -> bool {
        self.worktree_seed_other_libraries && self.worktree_seed_donor.is_some()
    }

    pub(crate) fn worktree_portable(&self) -> bool {
        self.worktree_seed_key.is_some()
    }

    pub(crate) fn needs_worktree_donor(&self) -> bool {
        !path_exists(&self.bootstrap_env_marker()) && empty_directory(&self.unit.rustc_dir)
    }

    fn bootstrap_env_marker(&self) -> PathBuf {
        self.unit.unit_dir.join("bootstrap-env-dep")
    }

    /// A source that reads bootstrap must see the caller's original value.
    /// Remember the refusal so later edits need no speculative attempt.
    pub(crate) fn decline_worktree_bootstrap(&mut self) -> bool {
        self.worktree_seed_key = None;
        self.worktree_seed_donor = None;
        self.worktree_bootstrap = false;
        let Ok(mut marker) = tempfile::NamedTempFile::new_in(&self.unit.unit_dir) else {
            return false;
        };
        use std::io::Write;
        marker.write_all(b"source reads RUSTC_BOOTSTRAP\n").is_ok()
            && marker.persist(self.bootstrap_env_marker()).is_ok()
            && remove_path_safely(&self.unit.rustc_dir)
            && ensure_real_directory(&self.unit.rustc_dir)
    }
}

pub(crate) fn compiler_supports_automatic_seed(version: &str) -> bool {
    let mut words = version.split_whitespace();
    if words.next() != Some("rustc") {
        return false;
    }
    let Some(release) = words.next() else {
        return false;
    };
    let mut parts = release.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return false;
    };
    match (major.parse::<u64>(), minor.parse::<u64>()) {
        (Ok(major), Ok(minor)) => (major, minor) >= (1, 98),
        _ => false,
    }
}

fn bootstrap_override_allowed(current: Option<&std::ffi::OsStr>, flags: &[String]) -> bool {
    current.is_none()
        && flags
            .iter()
            .all(|flag| flag.starts_with("remap-cwd-prefix=") || flag == "incremental-info")
}

pub(super) fn publish(unit: &AdaptiveUnit, key: Option<&str>, externs: Option<&ExternRecord>) {
    let Some(key) = key else { return };
    // What the graph was made against goes first: a receiver that finds the
    // marker reads the record of this compile, never of an earlier one.
    let record = unit.unit_dir.join(EXTERNS);
    let recorded = match externs {
        Some(externs) => write_small_file(&unit.unit_dir, &record, externs.render().as_bytes()),
        None => remove_path_safely(&record),
    };
    if recorded && write_small_file(&unit.unit_dir, &unit.unit_dir.join(MARKER), key.as_bytes()) {
        index_seed(unit, key);
    }
}

/// What a unit's graph was made against: the `--out-dir` its arguments
/// named and the metadata of every library it read by `--extern`, a library
/// its checkout compiled privately marked as such.
///
/// A graph used against other builds of its libraries compiles again what
/// those builds changed. A library compiled privately (on an incremental
/// lane, with `-C incremental`) has another crate hash than the same library
/// compiled for the store, as rustc then hashes every item's span into it:
/// a dependent's graph used against the other build compiles that library's
/// generic items again and keeps the rest (rustc 1.99.1: 541 of 35,769
/// instances), as after an edit of it. A library built with other source
/// paths costs far more (30,856 of them). On a large workspace a graph made
/// against other builds of some registry crates still served: the first
/// compiles after a base crate's edit took 40 s with such graphs and 270 s
/// without any.
///
/// So a receiver imports a donor's graph whatever it was made against, and
/// the record tells the imports made against other builds apart. Among
/// donors, one that wrote to the same `--out-dir` goes first, as a
/// different `--out-dir` (another target folder) compiles every codegen
/// unit again; then one made against the receiver's libraries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExternRecord {
    out_dir: String,
    /// By `name=file name` (Cargo's file names carry the unit's metadata
    /// hash, alike in every checkout).
    externs: BTreeMap<String, ExternDigest>,
    /// The libraries its checkout compiled privately.
    private: std::collections::BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternDigest {
    digest: String,
    /// Size and modification time when hashed: the unit's next compile
    /// hashes again only the libraries that changed since.
    size: u64,
    modified_ns: u128,
}

impl ExternRecord {
    /// The record of `args`' libraries, read where rustc reads them: a
    /// library's `.rmeta` beside its `.rlib` when there is one (the metadata
    /// its graph depends on, without the object code), the file named
    /// otherwise. Digests of `previous` serve files whose size and time did
    /// not change. None when a library cannot be read.
    pub(crate) fn of(args: &RustcArgs, cwd: &Path, previous: Option<&Self>) -> Option<Self> {
        let out_dir = match &args.out_dir {
            Some(dir) => relative(dir.to_str()?, cwd)?,
            None => String::new(),
        };
        let mut record = Self {
            out_dir,
            ..Self::default()
        };
        for dependency in &args.externs {
            let Some(path) = dependency.path.as_deref() else {
                continue;
            };
            let resolved = args.resolve_invocation_path(path)?;
            let name = format!(
                "{}={}",
                dependency.name,
                resolved.file_name()?.to_str()?
            );
            let metadata = match resolved.extension().and_then(|ext| ext.to_str()) {
                Some("rlib") => Some(resolved.with_extension("rmeta"))
                    .filter(|rmeta| fs::symlink_metadata(rmeta).is_ok_and(|meta| meta.is_file()))
                    .unwrap_or_else(|| resolved.clone()),
                _ => resolved.clone(),
            };
            let meta = fs::metadata(&metadata).ok()?;
            let size = meta.len();
            let modified_ns = meta
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            let digest = match previous.and_then(|previous| previous.externs.get(&name)) {
                Some(known) if known.size == size && known.modified_ns == modified_ns => {
                    known.digest.clone()
                }
                _ => kache_store::file_hash::hash_file(&metadata).ok()?,
            };
            if super::private_outputs::is_private(&resolved) {
                record.private.insert(name.clone());
            }
            record.externs.insert(
                name,
                ExternDigest {
                    digest,
                    size,
                    modified_ns,
                },
            );
        }
        Some(record)
    }

    fn read(path: &Path) -> Option<Self> {
        if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= 1 << 20) {
            return None;
        }
        Self::parse(&fs::read_to_string(path).ok()?)
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let out_dir = lines.next()?.strip_prefix("out-dir\t")?.to_owned();
        let mut externs = BTreeMap::new();
        let mut private = std::collections::BTreeSet::new();
        for line in lines {
            let mut fields = line.split('\t');
            let (Some("extern"), Some(name), Some(digest), Some(size), Some(modified), flag, None) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return None;
            };
            match flag {
                None => {}
                Some("private") => {
                    private.insert(name.to_owned());
                }
                Some(_) => return None,
            }
            externs.insert(
                name.to_owned(),
                ExternDigest {
                    digest: digest.to_owned(),
                    size: size.parse().ok()?,
                    modified_ns: modified.parse().ok()?,
                },
            );
        }
        Some(Self {
            out_dir,
            externs,
            private,
        })
    }

    fn render(&self) -> String {
        let mut text = format!("out-dir\t{}\n", self.out_dir);
        for (name, extern_digest) in &self.externs {
            text.push_str(&format!(
                "extern\t{name}\t{}\t{}\t{}{}\n",
                extern_digest.digest,
                extern_digest.size,
                extern_digest.modified_ns,
                if self.private.contains(name) {
                    "\tprivate"
                } else {
                    ""
                }
            ));
        }
        text
    }

    /// Whether a graph made against `donor` was made against this
    /// receiver's libraries: every one is the donor's, apart from those this
    /// checkout or the donor's compiled privately (the same crate, built on
    /// an incremental lane).
    fn same_libraries(&self, donor: &Self) -> bool {
        self.externs.iter().all(|(name, mine)| {
            donor.externs.get(name).is_some_and(|theirs| {
                theirs.digest == mine.digest
                    || self.private.contains(name)
                    || donor.private.contains(name)
            })
        })
    }
}

/// What looking for a donor's graph came to.
#[derive(Debug, PartialEq, Eq)]
enum Import {
    /// The graph of this donor profile was copied; `other_libraries` when
    /// the donor's record shows it was made against other builds of some
    /// libraries than the receiver's.
    Copied {
        profile: PathBuf,
        other_libraries: bool,
    },
    /// No donor had it.
    Absent,
}

/// Name the unit in its profile's seed index under `key`, unless it is
/// named there already. The unit's marker stays what a receiver checks: an
/// entry whose unit has since reset or changed is skipped.
fn index_seed(unit: &AdaptiveUnit, key: &str) {
    let Some(auto_root) = unit.unit_dir.parent().and_then(Path::parent) else {
        return;
    };
    let index = auto_root.join(SEEDS);
    let entry = index.join(key);
    if fs::read(&entry).ok().as_deref() == Some(unit.unit_key.as_bytes()) {
        return;
    }
    if ensure_real_directory(&index) {
        write_small_file(&index, &entry, unit.unit_key.as_bytes());
    }
}

/// Replace `path` with `bytes` through a file staged in `dir`.
fn write_small_file(dir: &Path, path: &Path, bytes: &[u8]) -> bool {
    let Ok(mut file) = tempfile::NamedTempFile::new_in(dir) else {
        return false;
    };
    use std::io::Write;
    file.write_all(bytes).is_ok() && file.persist(path).is_ok()
}

/// Mark `profile` as kept for others: every unit's graph stays, and the
/// idle clean-ups leave it to whoever keeps the folder.
pub(crate) fn mark_keep_all(profile: &Path) {
    let auto_root = profile.join(AUTO_ROOT);
    let marker = auto_root.join(KEEP_ALL);
    if !fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file()) && real_directory(&auto_root)
    {
        write_small_file(&auto_root, &marker, b"every unit's graph is kept here\n");
    }
}

/// Whether `profile` is kept for others ([`mark_keep_all`]).
pub(crate) fn keep_all_profile(profile: &Path) -> bool {
    fs::symlink_metadata(profile.join(AUTO_ROOT).join(KEEP_ALL)).is_ok_and(|meta| meta.is_file())
}

/// Record `profile` among the folders kept for others under `cache_dir`,
/// which every receiver on the machine tries first ([`kept_donors`]).
pub(crate) fn register_kept_donor(cache_dir: &Path, profile: &Path) {
    let Some(path) = profile.to_str() else { return };
    let registry = cache_dir.join(KEPT_DONORS);
    let entry = registry.join(&blake3::hash(path.as_bytes()).to_hex().as_str()[..32]);
    if fs::read(&entry).ok().as_deref() == Some(path.as_bytes()) {
        return;
    }
    if ensure_real_directory(&registry) {
        write_small_file(&registry, &entry, path.as_bytes());
    }
}

/// The profiles kept for others that [`register_kept_donor`] recorded under
/// `cache_dir` and that are still there. An entry whose folder is gone is
/// forgotten.
pub(crate) fn kept_donors(cache_dir: &Path) -> Vec<PathBuf> {
    let registry = cache_dir.join(KEPT_DONORS);
    let Ok(entries) = fs::read_dir(&registry) else {
        return Vec::new();
    };
    let mut donors = Vec::new();
    for entry in entries.take(MAX_DONORS).flatten() {
        let path = entry.path();
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() <= 4096) {
            continue;
        }
        let Ok(profile) = fs::read_to_string(&path).map(PathBuf::from) else {
            continue;
        };
        if keep_all_profile(&profile) {
            donors.push(profile);
        } else if definitely_missing(&profile) {
            let _ = fs::remove_file(&path);
        }
    }
    donors
}

/// Copy the newest graph of a unit that published `key` in one of
/// `profiles` into the receiver's empty graph, and tell whether it was made
/// against the receiver's libraries (`receiver_externs`, [`ExternRecord`]).
/// The profiles whose seed index names the key come first: one whose unit
/// wrote to the receiver's `--out-dir` before any other, then one whose
/// graph was made against the receiver's libraries, then a folder kept for
/// others, then the newest entry. Profiles without an index (an older
/// release's) have their units read one by one after that.
fn import_from(
    receiver: &AdaptiveUnit,
    profiles: &[PathBuf],
    key: &str,
    receiver_externs: Option<&ExternRecord>,
) -> Import {
    let mut indexed = Vec::new();
    let mut unindexed = Vec::new();
    for profile in profiles {
        let index = profile.join(AUTO_ROOT).join(SEEDS);
        match fs::symlink_metadata(index.join(key)) {
            Ok(meta) if meta.is_file() => {
                let Some(unit_key) = fs::read_to_string(index.join(key))
                    .ok()
                    .filter(|name| valid_hex_digest(name))
                else {
                    continue;
                };
                let donor_externs = ExternRecord::read(
                    &profile
                        .join(AUTO_ROOT)
                        .join(POLICY_VERSION)
                        .join(&unit_key)
                        .join(EXTERNS),
                );
                let (same_out_dir, same_libraries) = receiver_externs
                    .zip(donor_externs)
                    .map_or((false, false), |(mine, theirs)| {
                        (theirs.out_dir == mine.out_dir, mine.same_libraries(&theirs))
                    });
                indexed.push((
                    (same_out_dir, same_libraries, keep_all_profile(profile)),
                    meta.modified().ok(),
                    profile,
                    unit_key,
                ));
            }
            Ok(_) => {}
            Err(_) if !real_directory(&index) => unindexed.push(profile),
            Err(_) => {}
        }
    }
    indexed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, _, profile, unit_key) in &indexed {
        let outcome = import_unit(receiver, profile, unit_key, key, receiver_externs);
        if outcome != Import::Absent {
            return outcome;
        }
    }
    for profile in unindexed.into_iter().take(MAX_SCANNED_DONORS) {
        let outcome = import(receiver, profile, key, receiver_externs);
        if outcome != Import::Absent {
            return outcome;
        }
    }
    Import::Absent
}

/// The arguments a seed's key folds, with the target folder named alike in
/// every folder: `--out-dir`, `-L` and `--extern` are not among the options
/// rustc's graph records, so a graph from a folder of the build machine's
/// own (`target/integrate-stack`) is not thrown away in a checkout's
/// `target`. It keeps its analysis there, but not its code: rustc compiles
/// every codegen unit again when the `--out-dir` it names differs, so a
/// receiver prefers a donor that wrote to its own ([`ExternRecord`]), and
/// the libraries must be the donor's either way. Only a whole
/// leading path component is replaced, at the start of an argument or after
/// an `=` (`dependency=target/...`), never inside a value like
/// `target_os="linux"`.
fn target_neutral<'a>(argument: &'a str, target: Option<&str>) -> std::borrow::Cow<'a, str> {
    let Some(target) = target.filter(|target| !target.is_empty()) else {
        return argument.into();
    };
    let starts = std::iter::once(0).chain(argument.match_indices('=').map(|(at, _)| at + 1));
    for start in starts {
        let Some(rest) = argument[start..].strip_prefix(target) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with(std::path::MAIN_SEPARATOR) {
            return format!("{}\0target{rest}", &argument[..start]).into();
        }
    }
    argument.into()
}

fn portable_key(
    args: &RustcArgs,
    normalized: &[String],
    guard: &[u8],
    bootstrap: Option<&std::ffi::OsStr>,
    identity: &str,
    target: Option<&str>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    fold(&mut hasher, b"kache-worktree-incremental-v3");
    fold(&mut hasher, guard);
    fold(&mut hasher, identity.as_bytes());
    match bootstrap {
        Some(value) => {
            fold(&mut hasher, b"bootstrap");
            fold(&mut hasher, value.as_encoded_bytes());
        }
        None => fold(&mut hasher, b"no-bootstrap"),
    }
    fold(&mut hasher, args.rustc.as_os_str().as_encoded_bytes());
    fold_compiler_stamp(&mut hasher, &args.rustc);
    for arg in diagnostics_neutral(&strip_incremental_refs(normalized)) {
        fold(&mut hasher, target_neutral(&arg, target).as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// `args` without what only shapes how rustc reports diagnostics, which its
/// graph does not record either (`--color`, `--error-format`,
/// `--diagnostic-width`, and of `--json` what renders a diagnostic): a
/// terminal's width or a runner's colors do not decide which graph a unit
/// can start from. `--json=artifacts` and `future-incompat` stay; rustc's
/// graph records them.
fn diagnostics_neutral(args: &[&String]) -> Vec<String> {
    const REPORTING: [&str; 3] = ["--color", "--error-format", "--diagnostic-width"];
    let mut kept = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, attached) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
        if REPORTING.contains(&flag) {
            if attached.is_none() {
                iter.next();
            }
            continue;
        }
        if flag == "--json" {
            let value = match attached {
                Some(value) => value,
                None => match iter.next() {
                    Some(value) => value.as_str(),
                    None => break,
                },
            };
            let tracked: Vec<&str> = value
                .split(',')
                .filter(|part| !part.starts_with("diagnostic-"))
                .collect();
            kept.push(format!("--json={}", tracked.join(",")));
            continue;
        }
        kept.push((*arg).clone());
    }
    kept
}

fn relative(value: &str, cwd: &Path) -> Option<String> {
    let path = Path::new(value);
    match path.strip_prefix(cwd) {
        Ok(tail) if !tail.as_os_str().is_empty() => Some(tail.to_str()?.to_owned()),
        Ok(_) => Some(".".into()),
        Err(_) => Some(value.to_owned()),
    }
}

fn path_operand(flag: &str, value: &str, cwd: &Path) -> Option<String> {
    if matches!(flag, "--extern" | "-L") {
        if let Some((name, path)) = value.split_once('=') {
            return Some(format!("{name}={}", relative(path, cwd)?));
        }
        if flag == "--extern" {
            return Some(value.into());
        }
    }
    relative(value, cwd)
}

/// An allowlist prevents treating a cfg string or an unknown flag's operand
/// as a filesystem path. Unmodeled flags decline portable seeding.
fn portable_args(args: &[String], source: Option<&Path>, cwd: &Path) -> Option<Vec<String>> {
    let source = source?;
    // Absolute and relative source operands produce different file!() values
    // even with remap-cwd-prefix. Keep that caller-visible choice intact.
    if source.is_absolute() {
        return None;
    }
    let source = source.to_str()?;
    let mut result = Vec::with_capacity(args.len());
    let mut remap_count = 0;
    let mut saw_source = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, attached) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        if matches!(flag, "--out-dir" | "-o" | "--extern" | "-L") {
            let value = attached.or_else(|| iter.next().map(String::as_str))?;
            let rewritten = path_operand(flag, value, cwd)?;
            if attached.is_some() {
                result.push(format!("{flag}={rewritten}"));
            } else {
                result.extend([flag.into(), rewritten]);
            }
        } else if matches!(flag, "-C" | "--codegen" | "-Z") {
            let value = attached.or_else(|| iter.next().map(String::as_str))?;
            if flag == "-Z" && value.starts_with("remap-cwd-prefix=") {
                let prefix = value.strip_prefix("remap-cwd-prefix=")?;
                if !Path::new(prefix).is_absolute() {
                    return None;
                }
                remap_count += 1;
            }
            result.push(arg.clone());
            if attached.is_none() {
                result.push(value.into());
            }
        } else if let Some(value) = arg.strip_prefix("-Z") {
            if let Some(prefix) = value.strip_prefix("remap-cwd-prefix=") {
                if !Path::new(prefix).is_absolute() {
                    return None;
                }
                remap_count += 1;
            }
            result.push(arg.clone());
        } else if arg.starts_with("-C") || arg.starts_with("--codegen=") {
            result.push(arg.clone());
        } else if let Some(value) = arg.strip_prefix("-L").filter(|value| !value.is_empty()) {
            result.push(format!("-L{}", path_operand("-L", value, cwd)?));
        } else if matches!(
            flag,
            "--crate-name"
                | "--crate-type"
                | "--edition"
                | "--emit"
                | "--cfg"
                | "--check-cfg"
                | "--cap-lints"
                | "--force-warn"
                | "--error-format"
                | "--json"
                | "--color"
                | "--diagnostic-width"
                | "--sysroot"
                | "--target"
                | "-l"
                | "-A"
                | "-W"
                | "-D"
                | "-F"
                | "--allow"
                | "--warn"
                | "--deny"
                | "--forbid"
        ) {
            result.push(arg.clone());
            if attached.is_none() {
                result.push(iter.next()?.clone());
            }
        } else if matches!(arg.as_str(), "--test" | "-O" | "-g") {
            result.push(arg.clone());
        } else if !arg.starts_with('-') && arg == source && !saw_source {
            result.push(arg.clone());
            saw_source = true;
        } else {
            return None;
        }
    }
    (remap_count == 1 && saw_source).then_some(result)
}

/// Whether `profile` is its own canonical path: no symlinked ancestor.
fn canonical_spelling(profile: &Path) -> bool {
    fs::canonicalize(profile).is_ok_and(|canonical| same_spelling(&canonical, profile))
}

/// Whether `canonical` spells `profile`. On Windows `canonicalize` answers
/// the verbatim `\\?\C:\...` spelling of a plain `C:\...` path, which names
/// the same file; a canonical path on Unix never has that prefix.
fn same_spelling(canonical: &Path, profile: &Path) -> bool {
    if canonical == profile {
        return true;
    }
    canonical
        .to_str()
        .is_some_and(|text| Path::new(&*kache_fs::paths::strip_verbatim_prefix(text)) == profile)
}

pub(super) fn empty_directory(path: &Path) -> bool {
    real_directory(path) && fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Whether `profile` may give a seed: an absolute path that is its own
/// canonical spelling, so no symlinked ancestor leads elsewhere, with the
/// policy's folders in place. Nothing is created in the donor tree.
fn donor_profile(profile: &Path) -> bool {
    safe_absolute_path(profile)
        && canonical_spelling(profile)
        && real_directory(&profile.join(AUTO_ROOT))
        && real_directory(&profile.join(AUTO_ROOT).join(POLICY_VERSION))
}

/// Read the units of `profile` one by one for a seed published under `key`.
fn import(
    receiver: &AdaptiveUnit,
    profile: &Path,
    key: &str,
    receiver_externs: Option<&ExternRecord>,
) -> Import {
    if !donor_profile(profile) {
        return Import::Absent;
    }
    let Ok(entries) = fs::read_dir(profile.join(AUTO_ROOT).join(POLICY_VERSION)) else {
        return Import::Absent;
    };
    for entry in entries.take(MAX_DONORS).flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let outcome = copy_unit(receiver, profile, &name, key, receiver_externs);
        if outcome != Import::Absent {
            return outcome;
        }
    }
    Import::Absent
}

/// Copy the graph of the unit `unit_key` of `profile`, which the seed index
/// named under `key`.
fn import_unit(
    receiver: &AdaptiveUnit,
    profile: &Path,
    unit_key: &str,
    key: &str,
    receiver_externs: Option<&ExternRecord>,
) -> Import {
    if !donor_profile(profile) {
        return Import::Absent;
    }
    copy_unit(receiver, profile, unit_key, key, receiver_externs)
}

/// Copy the newest graph of the donor unit `name` of `profile` into the
/// receiver's empty graph, if that unit published `key` and no compile
/// holds it.
fn copy_unit(
    receiver: &AdaptiveUnit,
    profile: &Path,
    name: &str,
    key: &str,
    receiver_externs: Option<&ExternRecord>,
) -> Import {
    let unit_dir = profile.join(AUTO_ROOT).join(POLICY_VERSION).join(name);
    if !valid_hex_digest(name) || !real_directory(&unit_dir) || unit_dir == receiver.unit_dir {
        return Import::Absent;
    }
    let donor = AdaptiveUnit {
        source_file: None,
        dep_info: None,
        invocation_cwd: None,
        original_incremental: profile.join("incremental"),
        unit_key: name.to_owned(),
        state_path: unit_dir.join("state.json"),
        lock_path: unit_dir.join("unit.lock"),
        rustc_dir: unit_dir.join("rustc"),
        unit_dir,
        policy_guard: receiver.policy_guard.clone(),
    };
    // Open only an existing regular lock; the owner always creates it
    // before writing a marker. Never wait on a busy donor.
    if unsafe_file(&donor.lock_path) {
        return Import::Absent;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&donor.lock_path)
    else {
        return Import::Absent;
    };
    if file.try_lock().is_err() {
        return Import::Absent;
    }
    let _lock = UnitLock(file);
    let marker = donor.unit_dir.join(MARKER);
    if !fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file() && meta.len() == 64)
        || fs::read_to_string(marker).ok().as_deref() != Some(key)
        || !matches!(
            donor.load_state(),
            LoadedState::Missing
                | LoadedState::Valid(DiskState {
                    in_flight: false,
                    ..
                })
        )
        || !real_directory(&donor.rustc_dir)
    {
        return Import::Absent;
    }
    // A graph made against other builds of some libraries still serves
    // (ExternRecord); the record tells such an import apart. A donor
    // without one (an older release's) tells nothing.
    let other_libraries = receiver_externs
        .zip(ExternRecord::read(&donor.unit_dir.join(EXTERNS)))
        .is_some_and(|(mine, theirs)| !mine.same_libraries(&theirs));
    let Ok(stage) = tempfile::Builder::new()
        .prefix("worktree-seed-")
        .tempdir_in(&receiver.unit_dir)
    else {
        return Import::Absent;
    };
    let mut budget = CopyBudget {
        files: MAX_COPY_FILES,
        bytes: MAX_COPY_BYTES,
    };
    if copy_tree(&donor.rustc_dir, stage.path(), &mut budget, 0).is_err()
        || empty_directory(stage.path())
    {
        return Import::Absent;
    }
    // The receiver's unit lock has been held since before this method.
    // Rename replaces only its empty graph, never partially imports one.
    if fs::rename(stage.path(), &receiver.rustc_dir).is_ok() {
        Import::Copied {
            profile: profile.to_path_buf(),
            other_libraries,
        }
    } else {
        Import::Absent
    }
}

struct CopyBudget {
    files: usize,
    bytes: u64,
}

fn finalized_session(path: &Path, depth: usize) -> bool {
    if depth != 2 {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let parts: Vec<_> = name.split('-').collect();
    matches!(parts.as_slice(), ["s", time, random, hash]
    if *hash != "working"
        && [time, random, hash].into_iter().all(|part| {
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }))
}

pub(super) fn session_timestamp(path: &Path, depth: usize) -> Option<u64> {
    if !finalized_session(path, depth) {
        return None;
    }
    u64::from_str_radix(path.file_name()?.to_str()?.split('-').nth(1)?, 36).ok()
}

/// rustc selects the greatest base-36 timestamp in the session name. Keep
/// unfamiliar layouts intact, and retain ties rather than choosing between them.
pub(super) fn newest_session(entries: &[fs::DirEntry], depth: usize) -> Option<u64> {
    if depth != 1 {
        return None;
    }
    let mut newest = None;
    for entry in entries {
        if entry.file_type().ok()?.is_dir() {
            let timestamp = session_timestamp(&entry.path(), depth + 1)?;
            newest = Some(newest.map_or(timestamp, |current: u64| current.max(timestamp)));
        }
    }
    newest
}

fn copy_tree(source: &Path, dest: &Path, budget: &mut CopyBudget, depth: usize) -> io::Result<()> {
    if depth > 8 {
        return Err(io::Error::other("incremental tree too deep"));
    }
    let entries = fs::read_dir(source)?
        .take(budget.files.saturating_add(1))
        .collect::<io::Result<Vec<_>>>()?;
    let newest = newest_session(&entries, depth);
    for entry in entries {
        let meta = fs::symlink_metadata(entry.path())?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with("-working") {
            return Err(io::Error::other("unfinished incremental session"));
        }
        budget.files = budget
            .files
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("too many incremental files"))?;
        let target = dest.join(name);
        if meta.is_dir() {
            if let Some(newest) = newest
                && session_timestamp(&entry.path(), depth + 1)
                    .is_some_and(|timestamp| timestamp < newest)
            {
                continue;
            }
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target, budget, depth + 1)?;
        } else if meta.is_file() {
            budget.bytes = budget
                .bytes
                .checked_sub(meta.len())
                .ok_or_else(|| io::Error::other("incremental state too large"))?;
            let path = entry.path();
            // Codegen can rewrite inherited ThinLTO bitcode in place. Share
            // filesystem blocks where possible, never the donor's inode.
            if kache_fs::try_reflink(&path, &target).is_err() {
                // Never overwrite a stage file that might already link to a donor.
                let mut input = File::open(path)?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                io::copy(&mut input, &mut output)?;
            }
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_last_modification_time(&meta),
            )?;
            fs::set_permissions(&target, meta.permissions())?;
        } else {
            return Err(io::Error::other("non-regular incremental entry"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIRTUAL_PREFIX: &str = REMAP_PREFIX;

    /// The remap target names an absolute path on the host, or no Windows
    /// invocation qualifies for a seed (`/src` has no drive there). Off
    /// Windows it is where a stored compile maps the workspace: a graph
    /// built on privately compiled crates then fits a checkout that
    /// restored them, but for their crate hash.
    #[test]
    fn the_remap_prefix_is_absolute_on_this_host() {
        assert!(Path::new(REMAP_PREFIX).is_absolute(), "{REMAP_PREFIX}");
        if !cfg!(windows) {
            assert_eq!(
                REMAP_PREFIX,
                crate::path_normalizer::workspace_flag_target()
            );
        }
    }

    /// A donor profile is accepted under its own canonical spelling; on
    /// Windows `canonicalize` adds the verbatim prefix, which names the same
    /// directory.
    #[test]
    fn a_canonical_profile_is_accepted_and_a_symlinked_one_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(temp.path()).unwrap();
        let plain = match canonical.to_str() {
            Some(text) => PathBuf::from(kache_fs::paths::strip_verbatim_prefix(text).into_owned()),
            None => canonical.clone(),
        };
        assert!(canonical_spelling(&canonical), "{}", canonical.display());
        assert!(canonical_spelling(&plain), "{}", plain.display());
        assert!(!canonical_spelling(&plain.join("missing")));
        #[cfg(unix)]
        {
            let link = plain.join("link");
            std::os::unix::fs::symlink(&plain, &link).unwrap();
            assert!(!canonical_spelling(&link));
        }
    }

    #[test]
    fn the_verbatim_prefix_spells_the_same_path() {
        assert!(same_spelling(Path::new("/work/a"), Path::new("/work/a")));
        assert!(!same_spelling(Path::new("/work/a"), Path::new("/work/b")));
        assert!(same_spelling(
            Path::new(r"\\?\C:\work\a"),
            Path::new(r"\\?\C:\work\a")
        ));
        assert!(same_spelling(
            Path::new(r"\\?\C:\work\a"),
            Path::new(r"C:\work\a")
        ));
        assert!(!same_spelling(
            Path::new(r"\\?\C:\work\a"),
            Path::new(r"C:\work\b")
        ));
        assert!(same_spelling(
            Path::new(r"\\?\UNC\host\share\a"),
            Path::new(r"\\?\UNC\host\share\a")
        ));
        assert!(same_spelling(
            Path::new(r"\\?\UNC\host\share\a"),
            Path::new(r"\\host\share\a")
        ));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = Path::new(std::ffi::OsStr::from_bytes(b"/work/\xff"));
            assert!(same_spelling(raw, raw));
            assert!(!same_spelling(raw, Path::new("/work/a")));
        }
    }

    #[test]
    fn imports_newest_session_by_numeric_name_and_retains_ties() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let krate = source.join("crate");
        for name in ["s-z-abc-old", "s-10-abc-new", "s-10-other-tied"] {
            let session = krate.join(name);
            fs::create_dir_all(&session).unwrap();
            fs::write(session.join("dep-graph.bin"), name).unwrap();
        }
        fs::write(krate.join("s-z-abc.lock"), b"private lock").unwrap();
        let entries = fs::read_dir(&krate)
            .unwrap()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(newest_session(&entries, 1), Some(36));
        assert_eq!(newest_session(&entries, 0), None);
        assert_eq!(newest_session(&entries, 2), None);
        let mut budget = CopyBudget {
            files: 16,
            bytes: 1024,
        };
        copy_tree(&source, &dest, &mut budget, 0).unwrap();
        assert!(!dest.join("crate/s-z-abc-old").exists());
        for name in ["s-10-abc-new", "s-10-other-tied"] {
            assert_eq!(
                fs::read(dest.join("crate").join(name).join("dep-graph.bin")).unwrap(),
                name.as_bytes()
            );
        }
        assert_eq!(
            fs::read(dest.join("crate/s-z-abc.lock")).unwrap(),
            b"private lock"
        );
        assert!(krate.join("s-z-abc-old/dep-graph.bin").exists());
    }

    #[test]
    fn unknown_session_layouts_keep_all_state_and_working_sessions_still_decline() {
        for unknown in [
            "unfamiliar",
            "s-zzzzzzzzzzzzzzzzzzzz-abc-hash",
            "s-20-abc-working",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let dest = temp.path().join("dest");
            fs::create_dir(&dest).unwrap();
            for name in ["s-z-abc-old", "s-10-abc-new", unknown] {
                let session = source.join("crate").join(name);
                fs::create_dir_all(&session).unwrap();
                fs::write(session.join("dep-graph.bin"), name).unwrap();
            }
            let mut budget = CopyBudget {
                files: 16,
                bytes: 1024,
            };
            let result = copy_tree(&source, &dest, &mut budget, 0);
            if unknown.ends_with("-working") {
                assert!(result.is_err());
            } else {
                result.unwrap();
                for name in ["s-z-abc-old", "s-10-abc-new", unknown] {
                    assert_eq!(
                        fs::read(dest.join("crate").join(name).join("dep-graph.bin")).unwrap(),
                        name.as_bytes()
                    );
                }
            }
        }
    }

    #[test]
    fn entry_budget_cannot_silently_truncate_a_wide_directory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("one"), b"1").unwrap();
        fs::write(source.join("two"), b"2").unwrap();
        for files in [0, 1] {
            let dest = temp.path().join(format!("dest-{files}"));
            fs::create_dir(&dest).unwrap();
            let mut budget = CopyBudget { files, bytes: 8 };
            assert!(copy_tree(&source, &dest, &mut budget, 0).is_err());
        }
    }

    /// What a test's donors and receivers compiled against: no libraries,
    /// into Cargo's usual `--out-dir`.
    fn no_externs() -> ExternRecord {
        ExternRecord {
            out_dir: "target/debug/deps".into(),
            ..ExternRecord::default()
        }
    }

    fn copied(outcome: Import) -> bool {
        matches!(outcome, Import::Copied { .. })
    }

    fn fixture(root: &Path) -> (RustcArgs, AdaptiveUnit) {
        let profile = root.join("target/debug");
        fs::create_dir_all(profile.join("deps")).unwrap();
        let args = RustcArgs::parse(&[
            "/toolchain/bin/rustc".into(),
            "--crate-name".into(),
            "sample".into(),
            "src/lib.rs".into(),
            "--out-dir".into(),
            profile.join("deps").display().to_string(),
            "-Cincremental=".to_owned() + profile.join("incremental").to_str().unwrap(),
            "-Cextra-filename=-1234abcd".into(),
            format!("-Zremap-cwd-prefix={VIRTUAL_PREFIX}"),
        ])
        .unwrap();
        let unit = AdaptiveUnit::eligible(&args, b"guard").unwrap();
        (args, unit)
    }

    #[test]
    fn path_operands_are_portable_but_cfg_and_linker_literals_are_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("one");
        let deps = cwd.join("target").join("debug").join("deps");
        let literal = cwd.join("src").join("lib.rs").display().to_string();
        let linker = format!("-Clink-arg=-Wl,-rpath,{}", cwd.join("lib").display());
        let remap = format!("-Zremap-cwd-prefix={VIRTUAL_PREFIX}");
        let args = vec![
            "--crate-name".into(),
            "sample".into(),
            "src/lib.rs".into(),
            format!("--out-dir={}", deps.display()),
            "--extern".into(),
            format!("dep={}", deps.join("libdep.rlib").display()),
            format!("-Ldependency={}", deps.display()),
            "--cfg".into(),
            literal.clone(),
            linker.clone(),
            remap.clone(),
        ];
        let normalized = portable_args(&args, Some(Path::new("src/lib.rs")), &cwd).unwrap();
        let relative = PathBuf::from("target").join("debug").join("deps");
        assert_eq!(
            normalized,
            vec![
                "--crate-name".into(),
                "sample".into(),
                "src/lib.rs".into(),
                format!("--out-dir={}", relative.display()),
                "--extern".into(),
                format!("dep={}", relative.join("libdep.rlib").display()),
                format!("-Ldependency={}", relative.display()),
                "--cfg".into(),
                literal,
                linker,
                remap,
            ]
        );
        let mut unsupported = args.to_vec();
        unsupported.push(format!(
            "--future-path-option={}",
            cwd.join("input").display()
        ));
        assert!(portable_args(&unsupported, Some(Path::new("src/lib.rs")), &cwd).is_none());
        let without_remap = &args[..args.len() - 1];
        assert!(portable_args(without_remap, Some(Path::new("src/lib.rs")), &cwd).is_none());
        let mut twice = args.to_vec();
        twice.extend([
            "-Z".into(),
            format!("remap-cwd-prefix={}", cwd.join("other").display()),
        ]);
        assert!(portable_args(&twice, Some(Path::new("src/lib.rs")), &cwd).is_none());
    }

    #[test]
    fn source_operand_must_occur_once_and_match_the_declared_input() {
        let cwd = tempfile::tempdir().unwrap();
        let remap = format!("-Zremap-cwd-prefix={VIRTUAL_PREFIX}");
        let args = vec!["src/lib.rs".into(), remap.clone()];
        assert_eq!(
            portable_args(&args, Some(Path::new("src/lib.rs")), cwd.path()),
            Some(args.clone())
        );
        for invalid in [
            vec!["src/lib.rs".into(), "src/lib.rs".into(), remap.clone()],
            vec!["other.rs".into(), remap.clone()],
            vec![remap.clone()],
        ] {
            assert!(
                portable_args(&invalid, Some(Path::new("src/lib.rs")), cwd.path()).is_none(),
                "{invalid:?}"
            );
        }
        assert!(
            portable_args(
                &["-source.rs".into(), remap],
                Some(Path::new("-source.rs")),
                cwd.path()
            )
            .is_none()
        );
    }

    /// Cargo passes a `[lints]` table as long-form lint flags
    /// (`--warn=unexpected_cfgs`); they are lint names, not paths, and seed
    /// like their short forms, attached or as a separate operand.
    #[test]
    fn long_form_lint_flags_seed_like_their_short_forms() {
        let cwd = tempfile::tempdir().unwrap();
        let remap = format!("-Zremap-cwd-prefix={VIRTUAL_PREFIX}");
        for flag in ["--warn", "--allow", "--deny", "--forbid", "--force-warn"] {
            for lint in [
                vec![format!("{flag}=unexpected_cfgs")],
                vec![flag.to_string(), "unexpected_cfgs".to_string()],
            ] {
                let mut args: Vec<String> = vec!["src/lib.rs".into()];
                args.extend(lint);
                args.push(remap.clone());
                assert_eq!(
                    portable_args(&args, Some(Path::new("src/lib.rs")), cwd.path()),
                    Some(args.clone()),
                    "{args:?}"
                );
            }
            let dangling = vec!["src/lib.rs".into(), remap.clone(), flag.to_string()];
            assert!(
                portable_args(&dangling, Some(Path::new("src/lib.rs")), cwd.path()).is_none(),
                "{dangling:?}"
            );
        }
    }

    #[test]
    fn split_remap_flags_and_bare_search_paths_keep_their_meaning() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let args = vec![
            "src/lib.rs".into(),
            "-L".into(),
            cwd.display().to_string(),
            "--extern".into(),
            "dep".into(),
            "-Z".into(),
            format!("remap-cwd-prefix={VIRTUAL_PREFIX}"),
            "-Z".into(),
            "incremental-info".into(),
        ];
        let mut expected = args.clone();
        expected[2] = ".".into();
        assert_eq!(
            portable_args(&args, Some(Path::new("src/lib.rs")), cwd),
            Some(expected)
        );
    }

    #[test]
    fn portable_identity_changes_with_compiler_flags_and_policy() {
        let temp = tempfile::tempdir().unwrap();
        let (args_a, _) = fixture(&temp.path().join("one"));
        let (mut args_b, _) = fixture(&temp.path().join("two/deeper"));
        let a = portable_args(
            &args_a.all_args,
            args_a.source_file.as_deref(),
            &temp.path().join("one"),
        )
        .unwrap();
        let b = portable_args(
            &args_b.all_args,
            args_b.source_file.as_deref(),
            &temp.path().join("two/deeper"),
        )
        .unwrap();
        let key = portable_key(&args_a, &a, b"guard", None, "compiler-v1", None);
        assert_ne!(
            key,
            portable_key(
                &args_a,
                &a,
                b"guard",
                Some(std::ffi::OsStr::new("1")),
                "compiler-v1",
                None,
            )
        );
        assert_ne!(
            key,
            portable_key(&args_a, &a, b"guard", None, "compiler-v2", None)
        );
        assert_eq!(
            key,
            portable_key(&args_b, &b, b"guard", None, "compiler-v1", None)
        );
        assert_ne!(
            key,
            portable_key(&args_b, &b, b"other", None, "compiler-v1", None)
        );
        args_b.rustc = "/different/rustc".into();
        assert_ne!(
            key,
            portable_key(&args_b, &b, b"guard", None, "compiler-v1", None)
        );
        let mut flags = a.clone();
        flags.push("-Copt-level=2".into());
        assert_ne!(
            key,
            portable_key(&args_a, &flags, b"guard", None, "compiler-v1", None)
        );
        let mut remap = a.clone();
        *remap.last_mut().unwrap() = "-Zremap-cwd-prefix=/different".into();
        assert_ne!(
            key,
            portable_key(&args_a, &remap, b"guard", None, "compiler-v1", None)
        );

        // How diagnostics are shown is no part of the graph; which JSON
        // notices rustc emits is.
        let with = |extra: &[&str]| {
            let mut flags = a.clone();
            flags.extend(extra.iter().map(|flag| flag.to_string()));
            portable_key(&args_a, &flags, b"guard", None, "compiler-v1", None)
        };
        let json = with(&["--json=artifacts,future-incompat"]);
        assert_ne!(key, json);
        assert_eq!(
            json,
            with(&[
                "--error-format=json",
                "--json=diagnostic-rendered-ansi,artifacts,future-incompat",
                "--diagnostic-width=211",
                "--color",
                "always",
            ])
        );
        assert_ne!(json, with(&["--json=artifacts"]));
    }

    #[cfg(unix)]
    #[test]
    fn donor_import_skips_busy_incomplete_and_incompatible_units() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let (_, donor) = fixture(&root.join("donor"));
        let (_, receiver) = fixture(&root.join("receiver"));
        let donor_lease = donor.try_immediate().unwrap();
        let _receiver_lease = receiver.try_immediate().unwrap();
        let graph = donor.rustc_dir.join("crate/s-123-finished");
        fs::create_dir_all(&graph).unwrap();
        fs::write(graph.join("dep-graph.bin"), b"complete").unwrap();
        let key = "a".repeat(64);
        publish(&donor, Some(&key), Some(&no_externs()));
        let profile = root.join("donor/target/debug");
        assert!(
            !copied(import(&receiver, &profile, &key, None)),
            "busy donor must not be copied"
        );
        assert!(donor_lease.finish(CompileOutcome::Success));
        let alias = root.join("donor-profile-alias");
        std::os::unix::fs::symlink(&profile, &alias).unwrap();
        assert!(!copied(import(&receiver, &alias, &key, None)));
        assert!(empty_directory(&receiver.rustc_dir));
        assert!(!copied(import(&receiver, &profile, &"b".repeat(64), None)));
        fs::create_dir(donor.rustc_dir.join("s-123-working")).unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        assert!(empty_directory(&receiver.rustc_dir));
        fs::remove_dir(donor.rustc_dir.join("s-123-working")).unwrap();
        assert!(copied(import(&receiver, &profile, &key, None)));
        let copied = receiver
            .rustc_dir
            .join("crate/s-123-finished/dep-graph.bin");
        assert_eq!(fs::read(&copied).unwrap(), b"complete");
        fs::write(&copied, b"changed").unwrap();
        assert_eq!(fs::read(graph.join("dep-graph.bin")).unwrap(), b"complete");
    }

    /// A donor's graph is copied whatever libraries it was made against:
    /// one made against other builds of some of them still serves the items
    /// those builds left alone, and the import says so. A record that names
    /// only libraries the receiver's or the donor's checkout compiled
    /// privately apart counts as the same libraries; a donor without a record
    /// tells nothing. Among donors, one that wrote to the receiver's
    /// `--out-dir` goes first, then one made against the receiver's
    /// libraries, newer or not.
    #[cfg(unix)]
    #[test]
    fn a_graph_made_against_other_libraries_is_imported_and_told_apart() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let key = "a".repeat(64);
        let record = |out_dir: &str, libraries: &[(&str, &str)]| ExternRecord {
            out_dir: out_dir.into(),
            externs: libraries
                .iter()
                .map(|(name, digest)| {
                    (
                        (*name).to_owned(),
                        ExternDigest {
                            digest: (*digest).to_owned(),
                            size: 1,
                            modified_ns: 1,
                        },
                    )
                })
                .collect(),
            private: Default::default(),
        };
        let donor = |name: &str, externs: Option<&ExternRecord>| {
            let (_, unit) = fixture(&root.join(name));
            let lease = unit.try_immediate().unwrap();
            fs::write(unit.rustc_dir.join("graph"), name).unwrap();
            publish(&unit, Some(&key), externs);
            assert!(lease.finish(CompileOutcome::Success));
            root.join(name).join("target/debug")
        };
        let receive = |name: &str, profiles: &[PathBuf], mine: &ExternRecord| {
            let (_, unit) = fixture(&root.join(name));
            let _lease = unit.try_immediate().unwrap();
            let outcome = import_from(&unit, profiles, &key, Some(mine));
            let graph = fs::read_to_string(unit.rustc_dir.join("graph")).ok();
            (outcome, graph)
        };
        let copied = |profile: &PathBuf, other_libraries: bool| Import::Copied {
            profile: profile.clone(),
            other_libraries,
        };
        let deps = "target/debug/deps";
        let core = "core=libcore-1.rmeta";
        let edit = "edit=libedit-2.rmeta";
        let mine = record(deps, &[(core, "store core"), (edit, "e1")]);

        // An older release's seed, without a record, is imported.
        let unrecorded = donor("unrecorded", None);
        assert_eq!(
            receive("receiver-1", std::slice::from_ref(&unrecorded), &mine),
            (copied(&unrecorded, false), Some("unrecorded".into()))
        );
        // A graph made against another build of `core`.
        let station = record(deps, &[(core, "station core"), (edit, "e1")]);
        let other = donor("other", Some(&station));
        assert_eq!(
            receive("receiver-2", std::slice::from_ref(&other), &mine),
            (copied(&other, true), Some("other".into()))
        );
        // `edit` is this checkout's own compile: the donor's may differ, as
        // a graph's own upstream does after an edit.
        let mut edited = record(deps, &[(core, "station core"), (edit, "e9")]);
        edited.private.insert(edit.into());
        assert_eq!(
            receive("receiver-3", std::slice::from_ref(&other), &edited),
            (copied(&other, false), Some("other".into()))
        );
        // A library the donor compiled privately is another build of the
        // same crate too (a folder kept for others compiles every crate of
        // its workspace so).
        let mut kept = record(deps, &[(core, "kept core"), (edit, "e1")]);
        kept.private.insert(core.into());
        let kept_folder = donor("kept", Some(&kept));
        assert_eq!(
            receive("receiver-4", std::slice::from_ref(&kept_folder), &mine),
            (copied(&kept_folder, false), Some("kept".into()))
        );
        // Of two donors, the one made against the receiver's libraries goes
        // first, though it is the older and listed last.
        let matching = donor("matching", Some(&mine));
        let newer = donor("newer", Some(&station));
        assert_eq!(
            receive("receiver-5", &[newer.clone(), matching.clone()], &mine),
            (copied(&matching, false), Some("matching".into()))
        );
        // And before that, the one that wrote to the receiver's `--out-dir`.
        let stack = donor(
            "stack-folder",
            Some(&record("target/integrate-stack/debug/deps", &[(core, "store core"), (edit, "e1")])),
        );
        assert_eq!(
            receive("receiver-6", &[stack, newer.clone()], &mine),
            (copied(&newer, true), Some("newer".into()))
        );
    }

    /// The record names what rustc reads: a library's `.rmeta` beside its
    /// `.rlib` (object code changes nothing a graph depends on), a library
    /// this checkout compiled privately as such, and the `--out-dir` as the
    /// portable arguments name it. A digest is computed again only for a
    /// file whose size or time changed.
    #[test]
    fn the_record_names_each_library_by_its_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = fs::canonicalize(temp.path()).unwrap();
        let deps = cwd.join("target/debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let rlib = deps.join("libcore-1.rlib");
        fs::write(&rlib, b"objects and metadata").unwrap();
        fs::write(deps.join("libcore-1.rmeta"), b"metadata").unwrap();
        let only_rmeta = deps.join("libedit-2.rmeta");
        fs::write(&only_rmeta, b"edited metadata").unwrap();
        let args = RustcArgs::parse(&[
            "/toolchain/bin/rustc".into(),
            "--crate-name".into(),
            "sample".into(),
            "src/lib.rs".into(),
            "--out-dir".into(),
            deps.display().to_string(),
            "--extern".into(),
            format!("core={}", rlib.display()),
            "--extern".into(),
            format!("edit={}", only_rmeta.display()),
            "--extern".into(),
            "std".into(),
        ])
        .unwrap();
        super::super::private_outputs::mark([only_rmeta.as_path()]);
        let record = ExternRecord::of(&args, &cwd, None).unwrap();
        assert_eq!(record.out_dir, "target/debug/deps");
        let digest = |bytes: &[u8]| blake3::hash(bytes).to_hex().to_string();
        assert_eq!(
            record.externs["core=libcore-1.rlib"].digest,
            digest(b"metadata")
        );
        assert_eq!(
            record.externs["edit=libedit-2.rmeta"].digest,
            digest(b"edited metadata")
        );
        assert_eq!(record.externs.len(), 2);
        assert_eq!(
            record.private.iter().collect::<Vec<_>>(),
            ["edit=libedit-2.rmeta"]
        );
        let written = ExternRecord::parse(&record.render()).unwrap();
        assert_eq!(written, record);

        // An unchanged file keeps its digest; a changed one is hashed again.
        let mut stale = written;
        stale.externs.get_mut("core=libcore-1.rlib").unwrap().digest = "remembered".into();
        stale.externs.get_mut("edit=libedit-2.rmeta").unwrap().digest = "remembered".into();
        let modified = std::time::SystemTime::now() + Duration::from_secs(5);
        File::options()
            .write(true)
            .open(&only_rmeta)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let again = ExternRecord::of(&args, &cwd, Some(&stale)).unwrap();
        assert_eq!(again.externs["core=libcore-1.rlib"].digest, "remembered");
        assert_eq!(
            again.externs["edit=libedit-2.rmeta"].digest,
            digest(b"edited metadata")
        );
    }

    /// A receiver finds a donor's unit through the donor's seed index, a
    /// folder kept for others before any other. An entry whose unit since
    /// published another key is passed over, and a donor without an index
    /// (an older release's) has its units read one by one.
    #[cfg(unix)]
    #[test]
    fn the_seed_index_names_the_donor_unit_and_a_kept_folder_goes_first() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let key = "a".repeat(64);
        let donor = |name: &str| {
            let (_, unit) = fixture(&root.join(name));
            let lease = unit.try_immediate().unwrap();
            fs::write(unit.rustc_dir.join("graph"), name).unwrap();
            publish(&unit, Some(&key), Some(&no_externs()));
            assert!(lease.finish(CompileOutcome::Success));
            (root.join(name).join("target/debug"), unit)
        };
        let receive = |name: &str, profiles: &[PathBuf]| {
            let (_, unit) = fixture(&root.join(name));
            let _lease = unit.try_immediate().unwrap();
            let Import::Copied { profile: donor, .. } = import_from(&unit, profiles, &key, None) else {
                return None;
            };
            assert_eq!(
                fs::read_to_string(unit.rustc_dir.join("graph")).unwrap(),
                donor.parent()?.parent()?.file_name()?.to_str()?
            );
            Some(donor)
        };
        let (older, _) = donor("older");
        let (kept, kept_unit) = donor("kept");
        let (newer, _) = donor("newer");
        assert_eq!(
            fs::read_to_string(kept.join(AUTO_ROOT).join(SEEDS).join(&key)).unwrap(),
            kept_unit.unit_key
        );
        mark_keep_all(&kept);
        assert!(keep_all_profile(&kept));

        let profiles = [newer.clone(), kept.clone()];
        assert_eq!(receive("first", &profiles), Some(kept.clone()));
        fs::write(kept_unit.unit_dir.join(MARKER), "b".repeat(64)).unwrap();
        assert_eq!(receive("second", &profiles), Some(newer));
        fs::remove_dir_all(older.join(AUTO_ROOT).join(SEEDS)).unwrap();
        assert_eq!(receive("third", &[kept, older.clone()]), Some(older));
    }

    /// The folders kept for others are listed while they keep that mark; one
    /// that is gone is forgotten.
    #[test]
    fn kept_donors_lists_marked_folders_and_forgets_missing_ones() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let profile = |name: &str| {
            let profile = temp.path().join(name).join("target/debug");
            fs::create_dir_all(profile.join(AUTO_ROOT)).unwrap();
            profile
        };
        let (kept, plain, gone) = (profile("kept"), profile("plain"), profile("gone"));
        mark_keep_all(&kept);
        mark_keep_all(&gone);
        for donor in [&kept, &plain, &gone] {
            register_kept_donor(&cache, donor);
        }
        register_kept_donor(&cache, &kept);
        fs::remove_dir_all(temp.path().join("gone")).unwrap();

        assert_eq!(kept_donors(&cache), vec![kept]);
        assert_eq!(fs::read_dir(cache.join(KEPT_DONORS)).unwrap().count(), 2);
    }

    /// The target folder is named alike in every folder, so a build
    /// machine's own (`target/integrate-stack`) seeds a checkout's `target`,
    /// and only a whole leading path component is replaced.
    #[test]
    fn the_target_folder_is_named_alike_in_every_key() {
        let stack = Some("target/integrate-stack");
        let plain = Some("target");
        for (argument, at_stack, at_plain) in [
            (
                "target/integrate-stack/debug/deps",
                stack,
                Some("target/debug/deps"),
            ),
            (
                "dependency=target/integrate-stack/debug/deps",
                stack,
                Some("dependency=target/debug/deps"),
            ),
        ] {
            assert_eq!(
                target_neutral(argument, at_stack),
                target_neutral(at_plain.unwrap(), plain)
            );
            assert_ne!(target_neutral(argument, at_stack), argument);
        }
        for unchanged in [
            "target_os=\"linux\"",
            "--cfg=feature=\"target\"",
            "mytarget/debug",
            "target-other/debug",
            "src/target/lib.rs",
        ] {
            assert_eq!(target_neutral(unchanged, plain), unchanged);
        }
        assert_eq!(target_neutral("target/debug", None), "target/debug");
    }

    #[test]
    fn old_shared_state_is_neither_reused_nor_imported() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let (args, donor) = fixture(&root.join("donor"));
        let (_, receiver) = fixture(&root.join("receiver"));
        let lease = donor.try_immediate().unwrap();
        let _receiver_lease = receiver.try_immediate().unwrap();
        fs::write(donor.rustc_dir.join("unit.pre-lto.bc"), b"old shared state").unwrap();
        let key = "a".repeat(64);
        publish(&donor, Some(&key), Some(&no_externs()));
        assert!(lease.finish(CompileOutcome::Success));

        let profile = root.join("donor/target/debug");
        let current = profile.join("incremental.kache-auto/v2");
        assert_eq!(donor.unit_dir.parent(), Some(current.as_path()));
        let legacy = profile.join("incremental.kache-auto/v1");
        fs::rename(&current, &legacy).unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        assert!(empty_directory(&receiver.rustc_dir));

        let fresh = AdaptiveUnit::eligible(&args, b"guard").unwrap();
        let lease = fresh.try_immediate().unwrap();
        assert!(empty_directory(&fresh.rustc_dir));
        fs::write(
            fresh.rustc_dir.join("unit.pre-lto.bc"),
            b"new private state",
        )
        .unwrap();
        assert!(lease.finish(CompileOutcome::Success));
        assert_eq!(
            fs::read(legacy.join(&donor.unit_key).join("rustc/unit.pre-lto.bc")).unwrap(),
            b"old shared state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn donor_import_rejects_linked_or_invalid_metadata_and_abandoned_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let (_, donor) = fixture(&root.join("donor"));
        let (_, receiver) = fixture(&root.join("receiver"));
        let lease = donor.try_immediate().unwrap();
        let _receiver_lease = receiver.try_immediate().unwrap();
        fs::write(donor.rustc_dir.join("graph"), b"complete").unwrap();
        let key = "a".repeat(64);
        publish(&donor, Some(&key), Some(&no_externs()));
        assert!(lease.finish(CompileOutcome::Success));
        let profile = root.join("donor/target/debug");

        let linked_profile = root.join("linked-profile");
        fs::create_dir(&linked_profile).unwrap();
        std::os::unix::fs::symlink(profile.join(AUTO_ROOT), linked_profile.join(AUTO_ROOT))
            .unwrap();
        assert!(!copied(import(&receiver, &linked_profile, &key, None)));

        let invalid = donor.unit_dir.with_file_name("not-a-unit-digest");
        fs::rename(&donor.unit_dir, &invalid).unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        fs::rename(&invalid, &donor.unit_dir).unwrap();

        let saved_unit = root.join("saved-unit");
        fs::rename(&donor.unit_dir, &saved_unit).unwrap();
        std::os::unix::fs::symlink(&saved_unit, &donor.unit_dir).unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        fs::remove_file(&donor.unit_dir).unwrap();
        fs::rename(&saved_unit, &donor.unit_dir).unwrap();

        let marker = donor.unit_dir.join(MARKER);
        let target_name = "x".repeat(64);
        let saved_marker = donor.unit_dir.join(&target_name);
        fs::rename(&marker, &saved_marker).unwrap();
        std::os::unix::fs::symlink(&target_name, &marker).unwrap();
        assert_eq!(fs::symlink_metadata(&marker).unwrap().len(), 64);
        assert!(!copied(import(&receiver, &profile, &key, None)));
        fs::remove_file(&marker).unwrap();
        fs::rename(&saved_marker, &marker).unwrap();

        let abandoned = donor.try_immediate().unwrap();
        publish(&donor, Some(&key), Some(&no_externs()));
        drop(abandoned);
        assert!(!copied(import(&receiver, &profile, &key, None)));
        crate::atomic::atomic_replace_deferrable(&donor.state_path, b"invalid state", false)
            .unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        fs::remove_file(&donor.state_path).unwrap();

        let saved_graph = donor.unit_dir.join("saved-graph");
        fs::rename(&donor.rustc_dir, &saved_graph).unwrap();
        std::os::unix::fs::symlink(&saved_graph, &donor.rustc_dir).unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        fs::remove_file(&donor.rustc_dir).unwrap();
        fs::rename(&saved_graph, &donor.rustc_dir).unwrap();

        let broken = donor.rustc_dir.join("broken-session");
        fs::create_dir(&broken).unwrap();
        std::os::unix::fs::symlink(donor.rustc_dir.join("graph"), broken.join("linked-graph"))
            .unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        assert!(empty_directory(&receiver.rustc_dir));
        fs::remove_dir_all(&broken).unwrap();

        let saved_file = donor.unit_dir.join("saved-file");
        fs::rename(donor.rustc_dir.join("graph"), &saved_file).unwrap();
        assert!(!copied(import(&receiver, &profile, &key, None)));
        fs::rename(&saved_file, donor.rustc_dir.join("graph")).unwrap();

        assert!(empty_directory(&receiver.rustc_dir));
        assert!(copied(import(&receiver, &profile, &key, None)));
        assert_eq!(
            fs::read(receiver.rustc_dir.join("graph")).unwrap(),
            b"complete"
        );
    }

    #[test]
    fn bootstrap_opt_in_never_overrides_an_existing_selection() {
        use std::ffi::OsStr;
        assert!(bootstrap_override_allowed(None, &[]));
        for value in ["", "-1", "0", "other_crate", "1"] {
            assert!(!bootstrap_override_allowed(Some(OsStr::new(value)), &[]));
        }
        for flag in ["remap-cwd-prefix=/src", "incremental-info"] {
            assert!(bootstrap_override_allowed(None, &[flag.into()]));
        }
        for flag in ["allow-features=test", "unstable-options", "future-option"] {
            assert!(!bootstrap_override_allowed(None, &[flag.into()]));
            assert!(!bootstrap_override_allowed(
                None,
                &["incremental-info".into(), flag.into()]
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_cfg_literal_cannot_authorize_a_mismatched_incremental_lease() {
        let _lock = crate::test_support::process_state_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let _cache = crate::config::tests::set_env_for_test(
            "KACHE_CACHE_DIR",
            Some(temp.path().as_os_str()),
        );
        let (mut args, _) = fixture(temp.path());
        let compiler = temp.path().join("rustc-mock");
        kache_fs::testutil::write_executable(
            &compiler,
            "#!/bin/sh\nprintf 'rustc test compiler\\n'\n",
        );
        args.rustc = compiler;
        args.all_args
            .extend(["--cfg".into(), "mode=\"incremental=literal\"".into()]);
        let unit = AdaptiveUnit::eligible(&args, b"guard").unwrap();
        let mut lease = unit.try_immediate().unwrap();
        args.all_args.push("-Copt-level=2".into());
        let profile = temp.path().join("target/debug");
        let rewritten = lease.worktree_compiler_args(&args, Some(&profile), true);
        assert_eq!(rewritten, strip_incremental(&args.all_args));
        assert!(lease.worktree_seed_key.is_none());
        assert!(!lease.worktree_bootstrap());
    }

    #[cfg(unix)]
    #[test]
    fn inner_wrappers_decline_but_adaptive_leases_prepare_portable_state() {
        let _lock = crate::test_support::process_state_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let _cache = crate::config::tests::set_env_for_test(
            "KACHE_CACHE_DIR",
            Some(temp.path().as_os_str()),
        );
        let compiler = temp.path().join("rustc-mock");
        kache_fs::testutil::write_executable(
            &compiler,
            "#!/bin/sh\nprintf 'rustc test compiler\\n'\n",
        );
        for (index, kind) in [LeaseKind::Immediate, LeaseKind::Active, LeaseKind::Seed]
            .into_iter()
            .enumerate()
        {
            let (mut args, _) = fixture(&temp.path().join(index.to_string()));
            args.rustc = compiler.clone();
            if kind == LeaseKind::Immediate {
                args.inner_rustc = Some(compiler.clone());
            }
            let unit = AdaptiveUnit::eligible(&args, b"guard").unwrap();
            let mut lease = unit.try_immediate().unwrap();
            lease.kind = kind;
            let isolated = lease.compiler_args(&args);
            let portable = lease.worktree_compiler_args(&args, None, true);
            if kind == LeaseKind::Immediate {
                assert_eq!(portable, isolated);
                assert!(!lease.worktree_portable());
            } else {
                assert_eq!(portable, isolated);
                assert!(lease.worktree_portable());
            }
            assert!(!lease.worktree_bootstrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_lease_names_the_checkout_it_imported_state_from() {
        let _lock = crate::test_support::process_state_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let _cache =
            crate::config::tests::set_env_for_test("KACHE_CACHE_DIR", Some(root.as_os_str()));
        let compiler = root.join("rustc-mock");
        kache_fs::testutil::write_executable(
            &compiler,
            "#!/bin/sh\nprintf 'rustc test compiler\\n'\n",
        );
        let (mut args, _) = fixture(&root.join("receiver"));
        args.rustc = compiler.clone();
        let receiver = AdaptiveUnit::eligible(&args, b"guard").unwrap();
        let mut lease = receiver.try_immediate().unwrap();
        lease.kind = LeaseKind::Seed;
        lease.worktree_compiler_args_from(&args, &[], true);
        let key = lease
            .worktree_seed_key
            .clone()
            .expect("a portable invocation");
        assert_eq!(lease.worktree_seed_donor(), None);

        let (mut donor_args, _) = fixture(&root.join("donor"));
        donor_args.rustc = compiler;
        let donor = AdaptiveUnit::eligible(&donor_args, b"guard").unwrap();
        let donor_lease = donor.try_immediate().unwrap();
        fs::write(donor.rustc_dir.join("graph"), b"complete").unwrap();
        publish(&donor, Some(&key), Some(&no_externs()));
        assert!(donor_lease.finish(CompileOutcome::Success));
        let profile = root.join("donor/target/debug");
        let missing = root.join("gone/target/debug");
        lease.worktree_compiler_args_from(&args, &[missing, profile.clone()], true);
        assert_eq!(lease.worktree_seed_donor(), Some(profile.as_path()));

        // Preparing the lease again imports nothing and keeps its donor.
        lease.worktree_compiler_args_from(&args, &[], true);
        assert_eq!(lease.worktree_seed_donor(), Some(profile.as_path()));
    }

    #[test]
    fn automatic_seed_requires_rustc_198_or_later() {
        for version in [
            "rustc 1.98.0",
            "rustc 1.98.0-beta.2",
            "rustc 1.99.0-nightly",
            "rustc 2.0.0",
        ] {
            assert!(compiler_supports_automatic_seed(version), "{version}");
        }
        for version in [
            "rustc 1.97.1",
            "rustc 1.97.0-nightly",
            "rustc unknown",
            "rustc",
            "clang 19.0.0",
            "rustc 1.unknown.0",
        ] {
            assert!(!compiler_supports_automatic_seed(version), "{version}");
        }
    }

    #[test]
    fn bootstrap_observation_disables_portable_state_for_later_attempts() {
        let temp = tempfile::tempdir().unwrap();
        let (args, unit) = fixture(temp.path());
        let mut lease = unit.try_immediate().unwrap();
        fs::write(unit.rustc_dir.join("graph.bin"), b"override").unwrap();
        lease.worktree_bootstrap = true;
        lease.worktree_seed_key = Some("a".repeat(64));
        assert!(lease.decline_worktree_bootstrap());
        assert!(empty_directory(&unit.rustc_dir));
        assert!(!lease.worktree_bootstrap());
        assert!(!lease.worktree_portable());
        assert!(!lease.needs_worktree_donor());
        let isolated = lease.compiler_args(&args);
        assert_eq!(lease.worktree_compiler_args(&args, None, true), isolated);
        assert!(lease.bootstrap_env_marker().is_file());
    }

    #[test]
    fn copy_tree_accepts_eight_directory_levels_and_rejects_a_ninth() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        let mut leaf = source.clone();
        let mut copied = dest.clone();
        for _ in 0..8 {
            leaf.push("level");
            copied.push("level");
        }
        fs::create_dir_all(&leaf).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(leaf.join("graph"), b"complete").unwrap();
        let mut budget = CopyBudget { files: 9, bytes: 8 };
        copy_tree(&source, &dest, &mut budget, 0).unwrap();
        assert_eq!(fs::read(copied.join("graph")).unwrap(), b"complete");
        assert_eq!(budget.files, 0);
        assert_eq!(budget.bytes, 0);

        fs::create_dir(leaf.join("ninth")).unwrap();
        let refused = temp.path().join("refused");
        fs::create_dir(&refused).unwrap();
        let mut budget = CopyBudget {
            files: 32,
            bytes: 32,
        };
        assert!(copy_tree(&source, &refused, &mut budget, 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn seed_files_never_share_writable_state_with_the_donor() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let cases = [
            "crate/s-123-456-abcdef/dep-graph.bin",
            "crate/s-123-456-abcdef/unit.o",
            "crate/s-123-456-abcdef/unit.obj",
            "crate/s-123-456-abcdef/unit.bc",
            "crate/s-123-456-abcdef/session.lock",
            "crate/s-123-456-abcdef/unknown.txt",
            "s-123-456-abcdef/dep-graph.bin",
            "crate/s-123-finished/dep-graph.bin",
            "crate/unknown/dep-graph.bin",
            "crate/s-123-456-bad_name/dep-graph.bin",
            "crate/s--456-abcdef/dep-graph.bin",
        ];
        let modified = filetime::FileTime::from_unix_time(100, 123_456_789);
        for name in cases {
            let input = source.join(name);
            fs::create_dir_all(input.parent().unwrap()).unwrap();
            fs::write(&input, b"original").unwrap();
            filetime::set_file_mtime(&input, modified).unwrap();
        }
        let mut budget = CopyBudget {
            files: 128,
            bytes: 4096,
        };
        copy_tree(&source, &dest, &mut budget, 0).unwrap();
        for name in cases {
            let input = source.join(name);
            let output = dest.join(name);
            assert_eq!(fs::read(&output).unwrap(), b"original");
            assert_ne!(
                fs::metadata(&input).unwrap().ino(),
                fs::metadata(&output).unwrap().ino(),
                "{name}"
            );
            assert_eq!(
                filetime::FileTime::from_last_modification_time(&fs::metadata(&output).unwrap()),
                filetime::FileTime::from_last_modification_time(&fs::metadata(&input).unwrap()),
                "{name}"
            );
            fs::write(&output, b"receiver").unwrap();
            assert_eq!(fs::read(&input).unwrap(), b"original", "{name}");
        }
        assert!(!finalized_session(Path::new("s-123-456-working"), 2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn seed_copy_fallback_keeps_the_donor_private() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let Ok(other) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        if fs::metadata(temp.path()).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
            return;
        }
        let source = temp.path().join("s-123-456-abcdef");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("dep-graph.bin"), b"original").unwrap();
        let mut budget = CopyBudget { files: 1, bytes: 8 };
        copy_tree(&source, other.path(), &mut budget, 2).unwrap();
        fs::write(other.path().join("dep-graph.bin"), b"receiver").unwrap();
        assert_eq!(fs::read(source.join("dep-graph.bin")).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn private_seed_copies_preserve_read_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("receiver");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        let input = source.join("session.lock");
        fs::write(&input, b"private").unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o400)).unwrap();
        let mut budget = CopyBudget { files: 1, bytes: 7 };
        copy_tree(&source, &dest, &mut budget, 0).unwrap();
        let output = dest.join("session.lock");
        assert_eq!(fs::read(&output).unwrap(), b"private");
        assert_eq!(
            fs::metadata(output).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }

    #[test]
    fn an_existing_stage_file_cannot_overwrite_a_linked_donor() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("s-123-456-abcdef");
        let dest = temp.path().join("receiver");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("dep-graph.bin"), b"receiver").unwrap();
        let donor = temp.path().join("donor.bin");
        fs::write(&donor, b"original").unwrap();
        fs::hard_link(&donor, dest.join("dep-graph.bin")).unwrap();
        let mut budget = CopyBudget { files: 1, bytes: 8 };
        assert_eq!(
            copy_tree(&source, &dest, &mut budget, 2)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(donor).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_copy_budget_fail_without_installing_partial_state() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(source.join("graph"), b"1234").unwrap();
        let mut budget = CopyBudget { files: 5, bytes: 3 };
        assert!(copy_tree(&source, &dest, &mut budget, 0).is_err());
        assert!(empty_directory(&dest));
        fs::remove_file(source.join("graph")).unwrap();
        std::os::unix::fs::symlink(temp.path(), source.join("link")).unwrap();
        let mut budget = CopyBudget {
            files: 5,
            bytes: 100,
        };
        assert!(copy_tree(&source, &dest, &mut budget, 0).is_err());
        assert!(empty_directory(&dest));
    }

    #[test]
    fn ordinary_or_failed_compile_cannot_leave_a_donor_marker() {
        let temp = tempfile::tempdir().unwrap();
        let (args, unit) = fixture(temp.path());
        let lease = unit.try_immediate().unwrap();
        publish(&unit, Some(&"a".repeat(64)), Some(&no_externs()));
        lease.compiler_args(&args);
        assert!(!unit.unit_dir.join(MARKER).exists());
        fs::write(unit.rustc_dir.join("graph"), b"graph").unwrap();
        assert!(lease.finish(CompileOutcome::Success));
        assert!(!unit.unit_dir.join(MARKER).exists());
        let mut lease = unit.try_immediate().unwrap();
        lease.worktree_seed_key = Some("b".repeat(64));
        assert!(lease.finish(CompileOutcome::CompileError));
        assert!(!unit.unit_dir.join(MARKER).exists());
        let mut lease = unit.try_immediate().unwrap();
        lease.worktree_seed_key = Some("c".repeat(64));
        assert!(lease.finish(CompileOutcome::Success));
        assert_eq!(
            fs::read_to_string(unit.unit_dir.join(MARKER)).unwrap(),
            "c".repeat(64)
        );
        assert!(unit.reset());
        assert!(!unit.unit_dir.join(MARKER).exists());
    }
}
