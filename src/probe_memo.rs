//! On-disk memo for host-toolchain probes that spawn a tool.
//!
//! Every kache process is one rustc invocation, so without a file memo each
//! linked output pays every probe again. A memo is keyed by a digest of
//! length-prefixed [`Material`] fields, the file carries that full digest on
//! its first line and a reader refuses a file whose digest line differs, so
//! the short digest prefix in the file name can never alias two inputs.
//!
//! Writes go through a uniquely named temporary file and a rename: many
//! wrappers run in parallel under cargo, and a reader must only ever see a
//! whole file. Nothing here is required for a miss to be correct; every
//! failure reads as "not cached" and the caller runs the probe.

use std::path::{Path, PathBuf};

/// Length-prefixed key material. Prefixing every field with its byte length
/// keeps `("a\nb", "c")` and `("a", "b\nc")` distinct.
pub(crate) struct Material(Vec<u8>);

impl Material {
    /// Start material for one probe kind. Bump the tag whenever what the
    /// probe means changes, so an older kache's memo is never reused.
    pub(crate) fn new(tag: &str) -> Self {
        let mut material = Self(Vec::new());
        material.push(tag.as_bytes());
        material
    }

    pub(crate) fn push(&mut self, field: &[u8]) -> &mut Self {
        self.0
            .extend_from_slice(&(field.len() as u64).to_le_bytes());
        self.0.extend_from_slice(field);
        self
    }

    /// The full hex digest of everything pushed so far.
    pub(crate) fn digest(&self) -> String {
        blake3::hash(&self.0).to_hex().to_string()
    }
}

/// Hex digest of a file's bytes, or `None` when it cannot be read.
pub(crate) fn file_digest(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// `<dir>/<prefix>-<digest prefix>.<extension>`.
pub(crate) fn memo_path(dir: &Path, prefix: &str, extension: &str, digest: &str) -> PathBuf {
    dir.join(format!("{prefix}-{}.{extension}", &digest[..16]))
}

/// The memo body when the file's first line is exactly `digest`; `None` for
/// a missing, torn or foreign file.
pub(crate) fn read_verified(path: &Path, digest: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let (header, body) = contents.split_once('\n')?;
    (header == digest && !body.is_empty()).then(|| body.to_string())
}

/// Persist `body` under `digest`. See [`write_atomic`].
pub(crate) fn write_verified(path: &Path, digest: &str, body: &str) {
    write_atomic(path, &format!("{digest}\n{body}"));
}

/// Best-effort atomic write; a failure only means the next process re-probes.
pub(crate) fn write_atomic(path: &Path, contents: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(mut staging) = tempfile::NamedTempFile::new_in(dir) else {
        return;
    };
    if std::io::Write::write_all(&mut staging, contents.as_bytes()).is_err() {
        return; // the NamedTempFile drop removes the partial file
    }
    if staging.persist(path).is_ok() {
        // Temp files are 0600: a store shared through a group shares memos.
        kache_fs::share_in_group_store(path);
    }
}

/// How long a memo may go unread before GC removes it. Memos are keyed by a
/// tool's path, its file's identity, the environment and the working
/// directory, so every toolchain update, new checkout and new environment
/// leaves a set behind that nothing reads again, and nothing else removes
/// them: a shared build machine's default folder held 1.7 million after a
/// few days. Removing one that is still wanted costs one probe.
pub(crate) const MEMO_IDLE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// How long a tree digest (`probes/tree-digests`) may go unread. A build
/// writes one per walked root and set of skipped names, so new checkouts and
/// worktrees leave most of the memo files and bytes there (145k files and
/// 575 MB of 226k and 1.2 GB on a shared build machine); one that is still
/// wanted costs one walk of its tree.
pub(crate) const TREE_DIGEST_IDLE: std::time::Duration =
    std::time::Duration::from_secs(2 * 24 * 3600);

/// The subdirectory of `probes/` that holds the tree digests.
pub(crate) const TREE_DIGESTS_DIR: &str = "tree-digests";

/// Most memos one GC sweep removes, across every directory it looks in. A
/// removal is a stat and an unlink, so a full batch takes a few seconds in
/// the daemon's below-normal-priority sweep, and a backlog of a million
/// converges in about ten sweeps.
pub(crate) const MEMO_SWEEP_CAP: usize = 100_000;

/// The subdirectory of a cache directory that holds probe memos.
const PROBES_DIR: &str = "probes";

/// Per-build tree answers under `probes/`, which [`crate::session_tree`]
/// sweeps itself, a directory per Cargo invocation.
const SESSION_TREES_DIR: &str = "session-trees";

/// Memos an older kache wrote in the cache directory itself, before they
/// moved under `probes/`. Nothing reads them there any more.
const LEGACY_ROOT_MEMOS: &[&str] = &[
    "rustc-ver-",
    "rustc-sysroot-",
    "rustc-cwd-",
    "linker-ver-",
    "clippy-ver-",
    "msvc-banner-",
];

/// What [`sweep_idle_memos`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemoSweepStats {
    pub(crate) removed: usize,
    /// Memo files left: ones read or written within [`MEMO_IDLE`], and
    /// whatever the per-sweep cap deferred.
    pub(crate) remaining: usize,
}

/// The cache directories whose memos a GC of the store at `cache_dir`
/// sweeps: that store's own (tree memos and compiler probe records follow
/// the configured store) when it may write there, and the directory the
/// environment selects for the per-user memos ([`crate::config::probe_memo_dir`]:
/// `KACHE_CACHE_DIR`, else the default folder, `~/.cache/kache` or
/// `%LOCALAPPDATA%\kache`), which a store set in `config.toml` does not
/// move.
pub(crate) fn memo_sweep_roots(cache_dir: &Path, store_writable: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if store_writable {
        roots.push(cache_dir.to_path_buf());
    }
    // This crate's own tests run GC against stores in temporary
    // directories: never against the memos of the account running them.
    if !cfg!(test)
        && let Some(selected) = crate::config::probe_memo_dir().parent()
    {
        roots.push(selected.to_path_buf());
    }
    roots
}

/// Remove the memos under each of `roots` that nothing has read or written
/// for [`MEMO_IDLE`] ([`TREE_DIGEST_IDLE`] for the tree digests), up to [`MEMO_SWEEP_CAP`] per sweep: the files of
/// `<root>/probes` and of its subdirectories but the session trees, and the
/// old-layout memos in `<root>` itself. Use is the later of a file's
/// modification and access times; where a volume records no access times a
/// memo in use is removed once per [`MEMO_IDLE`] and probed again. A reader
/// that loses the race to a removal reads "not cached" and probes, as for
/// any missing memo.
pub(crate) fn sweep_idle_memos(roots: &[PathBuf]) -> MemoSweepStats {
    sweep_idle_memos_at(
        roots,
        MEMO_IDLE,
        TREE_DIGEST_IDLE,
        MEMO_SWEEP_CAP,
        std::time::SystemTime::now(),
    )
}

fn sweep_idle_memos_at(
    roots: &[PathBuf],
    idle: std::time::Duration,
    tree_idle: std::time::Duration,
    cap: usize,
    now: std::time::SystemTime,
) -> MemoSweepStats {
    let mut sweep = MemoSweep {
        cap,
        now,
        seen: 0,
        removed: 0,
    };
    let mut swept: Vec<PathBuf> = Vec::new();
    for root in roots {
        let identity = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if swept.contains(&identity) {
            continue;
        }
        swept.push(identity);
        sweep.dir(root, idle, is_legacy_root_memo, None);
        let mut subdirs = Vec::new();
        sweep.dir(&root.join(PROBES_DIR), idle, |_| true, Some(&mut subdirs));
        for subdir in subdirs {
            let idle = if subdir.file_name() == Some(TREE_DIGESTS_DIR.as_ref()) {
                tree_idle
            } else {
                idle
            };
            sweep.dir(&subdir, idle, |_| true, None);
        }
    }
    MemoSweepStats {
        removed: sweep.removed,
        remaining: sweep.seen - sweep.removed,
    }
}

fn is_legacy_root_memo(name: &str) -> bool {
    name.ends_with(".txt")
        && LEGACY_ROOT_MEMOS
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

struct MemoSweep {
    cap: usize,
    now: std::time::SystemTime,
    seen: usize,
    removed: usize,
}

impl MemoSweep {
    /// Sweep the regular files of `dir` that `accept` names and nobody used
    /// for `idle`, and collect its subdirectories but the session trees into
    /// `subdirs`.
    fn dir(
        &mut self,
        dir: &Path,
        idle: std::time::Duration,
        accept: impl Fn(&str) -> bool,
        mut subdirs: Option<&mut Vec<PathBuf>>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            // The directory entry's type costs no stat where the filesystem
            // reports it (Linux, Windows).
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if let Some(subdirs) = subdirs.as_deref_mut()
                    && entry.file_name() != SESSION_TREES_DIR
                {
                    subdirs.push(entry.path());
                }
                continue;
            }
            if !kind.is_file() || !entry.file_name().to_str().is_some_and(&accept) {
                continue;
            }
            self.seen += 1;
            // Past the cap the walk only counts, which costs no stat.
            if self.removed >= self.cap {
                continue;
            }
            let path = entry.path();
            if std::fs::symlink_metadata(&path).is_ok_and(|meta| self.is_idle(&meta, idle))
                && std::fs::remove_file(&path).is_ok()
            {
                self.removed += 1;
            }
        }
    }

    fn is_idle(&self, meta: &std::fs::Metadata, idle: std::time::Duration) -> bool {
        let last_use = [meta.modified(), meta.accessed()]
            .into_iter()
            .filter_map(Result::ok)
            .max();
        last_use.is_some_and(|used| {
            self.now
                .duration_since(used)
                .is_ok_and(|age| age >= idle)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_is_length_prefixed_so_field_boundaries_are_part_of_the_key() {
        let mut joined = Material::new("t");
        joined.push(b"a\nb").push(b"c");
        let mut split = Material::new("t");
        split.push(b"a").push(b"b\nc");
        assert_ne!(joined.digest(), split.digest());

        let mut same = Material::new("t");
        same.push(b"a\nb").push(b"c");
        assert_eq!(joined.digest(), same.digest());
        assert_ne!(Material::new("t").digest(), Material::new("u").digest());
        assert_eq!(joined.digest().len(), 64, "the full blake3 hex digest");
        assert_eq!(
            joined.0[..9],
            [1, 0, 0, 0, 0, 0, 0, 0, b't'],
            "little-endian u64 length, then the bytes"
        );
    }

    #[test]
    fn file_digest_follows_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool.exe");
        assert_eq!(file_digest(&file), None);
        std::fs::write(&file, b"v1").unwrap();
        let first = file_digest(&file).unwrap();
        assert_eq!(first, blake3::hash(b"v1").to_hex().to_string());
        std::fs::write(&file, b"v2").unwrap();
        assert_ne!(
            file_digest(&file).unwrap(),
            first,
            "same length, different bytes"
        );
    }

    #[test]
    fn memo_path_folds_prefix_digest_and_extension() {
        let dir = Path::new("/cache");
        let one = Material::new("x").digest();
        let two = Material::new("y").digest();
        let a = memo_path(dir, "msvc-banner", "txt", &one);
        assert_ne!(a, memo_path(dir, "msvc-banner", "txt", &two));
        assert_ne!(a, memo_path(dir, "other", "txt", &one));
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("msvc-banner-{}.txt", &one[..16]));
        assert_eq!(a.parent(), Some(dir));
    }

    #[test]
    fn verified_read_requires_the_matching_digest_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("memo.txt");
        let digest = Material::new("x").digest();
        assert_eq!(read_verified(&path, &digest), None);

        write_verified(&path, &digest, "banner\n");
        assert_eq!(read_verified(&path, &digest).as_deref(), Some("banner\n"));
        assert_eq!(
            read_verified(&path, &Material::new("y").digest()),
            None,
            "a file written for other inputs is not served"
        );

        write_verified(&path, &digest, "replaced\n");
        assert_eq!(read_verified(&path, &digest).as_deref(), Some("replaced\n"));

        std::fs::write(&path, &digest).unwrap();
        assert_eq!(read_verified(&path, &digest), None, "no body line");
        std::fs::write(&path, format!("{digest}\n")).unwrap();
        assert_eq!(read_verified(&path, &digest), None, "empty body");
        std::fs::write(&path, format!("{digest}x\nbanner\n")).unwrap();
        assert_eq!(
            read_verified(&path, &digest),
            None,
            "digest must match exactly"
        );

        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .count();
        assert_eq!(leftovers, 1, "no staging files linger beside the memo");
    }

    #[test]
    fn gc_removes_memos_nobody_used_for_the_idle_time() {
        let idle = std::time::Duration::from_secs(3600);
        let now = std::time::SystemTime::now();
        let stale = now - 2 * idle;
        let file = |path: PathBuf, modified, accessed| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"memo").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(modified)
                        .set_accessed(accessed),
                )
                .unwrap();
            path
        };
        let store = tempfile::tempdir().unwrap();
        let default = tempfile::tempdir().unwrap();
        let (a, b) = (store.path(), default.path());
        let idle_memos = [
            file(a.join("probes/rustc-ver-1.txt"), stale, stale),
            file(a.join("probes/tree-digests/0f0f"), stale, stale),
            file(a.join("rustc-ver-1.txt"), stale, stale),
            file(b.join("probes/0f0f.json"), stale, stale),
            file(b.join("clippy-ver-2.txt"), stale, stale),
        ];
        let kept = [
            // Read lately: the access time is the last use.
            file(a.join("probes/linker-placement-1.json"), stale, now),
            file(b.join("probes/root-manifest/v1-x"), now, stale),
            // Another sweep owns the session trees.
            file(b.join("probes/session-trees/1-2/0f0f"), stale, stale),
            // Not a memo of the old layout.
            file(a.join("rustc-ver.lock"), stale, stale),
            file(a.join("index.db"), stale, stale),
        ];

        // A root named twice is swept once.
        let roots = [a.to_path_buf(), b.to_path_buf(), a.join(".")];
        assert_eq!(
            sweep_idle_memos_at(&roots, idle, idle, 100, now),
            MemoSweepStats {
                removed: 5,
                remaining: 2
            }
        );
        assert!(idle_memos.iter().all(|path| !path.exists()));
        assert!(kept.iter().all(|path| path.exists()));

        // The cap bounds one sweep; the next one takes the rest.
        let more = [
            file(b.join("probes/1.json"), stale, stale),
            file(b.join("probes/2.json"), stale, stale),
            file(b.join("probes/3.json"), stale, stale),
        ];
        assert_eq!(
            sweep_idle_memos_at(&roots, idle, idle, 2, now),
            MemoSweepStats {
                removed: 2,
                remaining: 3
            }
        );
        assert_eq!(more.iter().filter(|path| path.exists()).count(), 1);
        assert_eq!(sweep_idle_memos_at(&roots, idle, idle, 2, now).removed, 1);
    }

    #[test]
    fn tree_digests_go_after_two_idle_days_other_memos_after_seven() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let day = std::time::Duration::from_secs(24 * 3600);
        let now = std::time::SystemTime::now();
        let memo = |path: PathBuf, idle_days: u32| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"memo").unwrap();
            let used = now - day * idle_days;
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(used)
                        .set_accessed(used),
                )
                .unwrap();
            path
        };
        let tree_idle = memo(root.join("probes/tree-digests/0f0f"), 3);
        let tree_in_use = memo(root.join("probes/tree-digests/1f1f"), 1);
        let probe_idle_3_days = memo(root.join("probes/rustc-ver-1.txt"), 3);
        let probe_idle_8_days = memo(root.join("probes/rustc-ver-2.txt"), 8);

        let stats = sweep_idle_memos(&[root.to_path_buf()]);

        assert!(!tree_idle.exists(), "a tree digest unused for 3 days goes");
        assert!(tree_in_use.exists(), "one used yesterday stays");
        assert!(probe_idle_3_days.exists(), "other memos keep 7 days");
        assert!(!probe_idle_8_days.exists());
        assert_eq!(
            stats,
            MemoSweepStats {
                removed: 2,
                remaining: 2
            }
        );
    }

    #[test]
    fn atomic_write_to_an_unwritable_location_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        // The parent "directory" is a regular file: create_dir_all fails.
        write_atomic(&blocker.join("memo.txt"), "ignored");
        assert_eq!(std::fs::read(&blocker).unwrap(), b"x");
    }
}
