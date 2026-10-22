//! Tree digests shared by the units of one Cargo invocation.
//!
//! Every workspace unit's tree guard digests the whole workspace: one stat
//! walk of every entry, then the memoised digest for that stamp. A
//! fresh-worktree `cargo clippy --workspace --all-targets` runs about 240
//! such units, each walking the same ~5,000 entries: 1.2 M `statx` calls
//! for one answer. Here the first unit of an invocation walks and digests
//! the root as before and files the answer under the invocation; the other
//! units of the same invocation read it instead of walking.
//!
//! The invocation is the wrapper's parent process when that is Cargo, named
//! by its PID and its start time (so a reused PID is another invocation);
//! any other parent gets no session and walks every unit. A new Cargo
//! invocation walks again, so an edit between two builds is seen exactly as
//! before. What this gives up: an edit made while one Cargo invocation runs,
//! to a file outside a unit's closure (only a directory-scanning macro could
//! read one), is not seen by the units of that invocation that start after
//! it. The closure's own files are still hashed by every unit, so an edit to
//! a file rustc reads still changes the key. A record made after a compile
//! still re-walks the tree for real ([`crate::cache_key::TreeGuard::held`]),
//! so no record is filed under a stale digest.
//!
//! Only the workspace root is shared: an `OUT_DIR` is a few files and is
//! written by build scripts while the invocation runs.
//!
//! `KACHE_TREE_SESSION=off` turns it off; `KACHE_TREE_SESSION_ID` names the
//! invocation explicitly (tests, and wrappers that are not Cargo's child).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// What the first unit of an invocation learned about one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedRoot {
    /// The stamp's digest, as a [`crate::cache_key`] guard walk records it.
    pub(crate) stamp: String,
    /// Entries the walk spent.
    pub(crate) entries: usize,
    /// The root's content digest.
    pub(crate) digest: String,
}

/// Session directories older than this are removed when a new one is made.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

const VERSION: &[u8] = b"kache-session-tree-v1\n";

/// The invocation this process belongs to, if sharing is on and one can be
/// named.
pub(crate) fn session_id(env: &crate::invocation::InvocationEnv) -> Option<String> {
    // Tests share one process, and so one parent: a test names its session.
    #[cfg(test)]
    {
        let _ = env;
        TEST_SESSION.with(|session| session.borrow().clone())
    }
    #[cfg(not(test))]
    session_from_env(env)
}

#[cfg(test)]
thread_local! {
    static TEST_SESSION: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Name the session the units of this test thread belong to, `None` for none.
#[cfg(test)]
pub(crate) fn set_test_session(session: Option<&str>) {
    TEST_SESSION.with(|slot| *slot.borrow_mut() = session.map(str::to_string));
}

#[cfg_attr(test, allow(dead_code))]
fn session_from_env(env: &crate::invocation::InvocationEnv) -> Option<String> {
    if env
        .var_os("KACHE_TREE_SESSION")
        .is_some_and(|value| value == "off" || value == "0")
    {
        return None;
    }
    if let Some(id) = env.var_os("KACHE_TREE_SESSION_ID") {
        let id = id.to_str()?;
        return (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            .then(|| id.to_string());
    }
    parent_session()
}

#[cfg(target_os = "linux")]
fn parent_session() -> Option<String> {
    let ppid = std::os::unix::process::parent_id();
    if ppid <= 1 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{ppid}/stat")).ok()?;
    cargo_session(ppid, &stat)
}

/// The session of a parent process from its `/proc/<pid>/stat`, only when
/// that parent is Cargo. Anything else that runs the wrapper itself (a test
/// harness, a build tool's long-lived process) would put every compile it
/// ever makes into one session and so never see an edit: it gets none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cargo_session(ppid: u32, stat: &str) -> Option<String> {
    // The command name sits in parentheses and may hold spaces and
    // parentheses itself; the fields after it never do.
    let (head, after) = stat.rsplit_once(')')?;
    let command = head.split_once('(')?.1;
    if command != "cargo" {
        return None;
    }
    // `after` starts with field 3 (state); start time is field 22.
    let start = after.split_whitespace().nth(19)?;
    (!start.is_empty() && start.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{ppid}-{start}"))
}

#[cfg(not(target_os = "linux"))]
fn parent_session() -> Option<String> {
    None
}

/// The file one root's answer is filed under in `session`.
fn entry_path(memo_dir: &Path, session: &str, key: &[u8]) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    hasher.update(VERSION);
    hasher.update(key);
    memo_dir
        .join("session-trees")
        .join(session)
        .join(&hasher.finalize().to_hex()[..32])
}

fn parse(text: &str) -> Option<SharedRoot> {
    let mut lines = text.lines();
    let stamp = lines.next()?.to_string();
    let entries = lines.next()?.parse().ok()?;
    let digest = lines.next()?.to_string();
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(&stamp) && hex(&digest) && lines.next() == Some("end")).then_some(SharedRoot {
        stamp,
        entries,
        digest,
    })
}

fn read(path: &Path) -> Option<SharedRoot> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    parse(&text)
}

/// The answer for the root named by `key` in this invocation: the one filed
/// by an earlier unit, or `compute`'s, filed for the later ones. Units of one
/// invocation that start together wait for the first one's walk instead of
/// all walking. Without a session, or when the session cannot be written,
/// `compute` alone answers.
pub(crate) fn shared_or(
    env: &crate::invocation::InvocationEnv,
    memo_dir: &Path,
    key: &[u8],
    compute: impl FnOnce() -> Option<SharedRoot>,
) -> Option<SharedRoot> {
    let Some(session) = session_id(env) else {
        return compute();
    };
    shared_in(memo_dir, &session, key, compute)
}

pub(crate) fn shared_in(
    memo_dir: &Path,
    session: &str,
    key: &[u8],
    compute: impl FnOnce() -> Option<SharedRoot>,
) -> Option<SharedRoot> {
    let path = entry_path(memo_dir, session, key);
    if let Some(found) = read(&path) {
        crate::phase_trace::decision("session_tree", "shared");
        return Some(found);
    }
    let Some(dir) = path.parent() else {
        return compute();
    };
    if !dir.is_dir() {
        if std::fs::create_dir_all(dir).is_err() {
            return compute();
        }
        if let Some(sessions) = dir.parent() {
            sweep(sessions, dir);
        }
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"));
    let _held = match lock {
        Ok(file) => file.lock().ok().map(|()| file),
        Err(_) => None,
    };
    if let Some(found) = read(&path) {
        crate::phase_trace::decision("session_tree", "shared");
        return Some(found);
    }
    crate::phase_trace::decision("session_tree", "walked");
    let computed = compute()?;
    let staging = path.with_extension(format!("tmp-{}", std::process::id()));
    let written = std::fs::File::create(&staging).and_then(|mut file| {
        write!(
            file,
            "{}\n{}\n{}\nend\n",
            computed.stamp, computed.entries, computed.digest
        )
    });
    if written.is_ok() {
        let _ = std::fs::rename(&staging, &path);
    } else {
        let _ = std::fs::remove_file(&staging);
    }
    Some(computed)
}

/// Remove session directories under `sessions` older than [`SESSION_TTL`],
/// other than `current`.
fn sweep(sessions: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age > SESSION_TTL)
            });
        if old {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(n: u8) -> SharedRoot {
        SharedRoot {
            stamp: format!("{n:064x}"),
            entries: n as usize,
            digest: format!("{:064x}", n as u32 + 1000),
        }
    }

    #[test]
    fn second_unit_of_a_session_reads_the_first_ones_answer() {
        let dir = tempfile::tempdir().unwrap();
        let first = shared_in(dir.path(), "s1", b"root", || Some(answer(1)));
        assert_eq!(first, Some(answer(1)));
        let second = shared_in(dir.path(), "s1", b"root", || panic!("must not walk again"));
        assert_eq!(second, Some(answer(1)));
    }

    #[test]
    fn another_session_or_root_computes_again() {
        let dir = tempfile::tempdir().unwrap();
        shared_in(dir.path(), "s1", b"root", || Some(answer(1)));
        assert_eq!(
            shared_in(dir.path(), "s2", b"root", || Some(answer(2))),
            Some(answer(2))
        );
        assert_eq!(
            shared_in(dir.path(), "s1", b"other", || Some(answer(3))),
            Some(answer(3))
        );
    }

    #[test]
    fn a_failed_walk_is_not_filed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(shared_in(dir.path(), "s1", b"root", || None), None);
        assert_eq!(
            shared_in(dir.path(), "s1", b"root", || Some(answer(4))),
            Some(answer(4))
        );
    }

    #[test]
    fn a_torn_entry_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = entry_path(dir.path(), "s1", b"root");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}\n12\n", answer(1).stamp)).unwrap();
        assert_eq!(
            shared_in(dir.path(), "s1", b"root", || Some(answer(5))),
            Some(answer(5))
        );
    }

    #[test]
    fn the_session_is_cargo_and_its_start_time() {
        let stat = |command: &str| {
            format!(
                "4242 ({command}) S 1 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0"
            )
        };
        assert_eq!(
            cargo_session(4242, &stat("cargo")).as_deref(),
            Some("4242-987654")
        );
        for other in ["cargo-nextest", "kache_cli_comm", "a (cargo)", "sh"] {
            assert_eq!(cargo_session(4242, &stat(other)), None, "{other}");
        }
        assert_eq!(cargo_session(4242, "4242 (cargo) S 1"), None, "torn");
    }
}
