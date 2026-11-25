//! Units of the tree the checkouts on a machine share.
//!
//! A target with incremental history compiles what it rebuilds on a private
//! lane: fast for an edit, and never stored. After a pull or a rebase the
//! units that moved are no edits. Every other checkout builds the same
//! sources, and one of them, or the store, may hold them already; compiled
//! privately they are compiled once in each checkout, and everything built on
//! them with them, since a unit that takes a privately compiled dependency
//! has a key no store holds.
//!
//! `[cache] shared_ref` names the ref the checkouts share. A unit is of the
//! shared tree when its package folder, and what its last dep-info names
//! elsewhere in the workspace, hold what the merge base of `HEAD` and that
//! ref holds. Such a unit keeps to the cache in a target with history too
//! (the wrapper's `keeps_to_the_cache`): it is looked up and restored, or
//! compiled whole under a flight and stored, as in a new target.
//!
//! A unit the shared tree does not hold is the checkout's own: its package
//! differs from the merge base, or Git does not track it. With
//! `[cache] keep_incremental` at its default such a unit compiles on the kept
//! lane (`crate::incremental_policy`), from its own state or a donor's.
//!
//! Git's answer picks the lane, never a key. A wrong "shared" compiles a unit
//! whole and stores it; a wrong "changed" compiles it privately, as
//! before. Git missing, failing or slow reads as neither ([`Tree::Unknown`]).

use crate::args::RustcArgs;
use crate::invocation::InvocationEnv;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How long Git may take over one unit before the unit keeps its lane.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_POLL: Duration = Duration::from_millis(1);
/// How long a Git past its deadline has to remove its lock files after
/// SIGTERM before it is killed.
#[cfg(unix)]
const GIT_GRACE: Duration = Duration::from_millis(200);
/// Sources outside its package one unit may have Git compare. A unit that
/// reads more keeps its lane.
const MAX_OUTSIDE_SOURCES: usize = 64;

/// What Git says of a unit's sources against the shared tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Tree {
    /// The merge base of `HEAD` and the shared ref holds them.
    Shared,
    /// The checkout changed them, or Git does not track the package.
    Changed,
    /// No shared tree covers them: no package folder, no repository, no such
    /// ref, or more sources outside the package than are compared.
    Outside,
    /// Git gave no answer: it did not start, was stopped or took too long.
    Unknown,
}

/// Whether the unit `args` compiles is of the shared tree at `reference`.
/// `sources` is what its last dep-info named, when one is at hand.
pub(crate) fn holds(reference: &str, args: &RustcArgs, sources: Option<&[PathBuf]>) -> bool {
    classify(reference, args, sources) == Tree::Shared
}

/// What Git says of the unit `args` compiles against the shared tree at
/// `reference` ([`Tree`]). `sources` is what its last dep-info named, when
/// one is at hand.
pub(crate) fn classify(reference: &str, args: &RustcArgs, sources: Option<&[PathBuf]>) -> Tree {
    let env = args.invocation_env();
    let Some(package) = env
        .var_os("CARGO_MANIFEST_DIR")
        .and_then(|dir| env.resolve(Path::new(&dir)))
    else {
        return Tree::Outside;
    };
    let Some(outside) = outside_sources(
        &package,
        args.path_normalization_root(),
        args.target_dir().as_deref(),
        sources.unwrap_or_default(),
    ) else {
        return Tree::Outside;
    };
    package_against_merge_base(env, reference, &package, &outside)
}

/// The sources Git must compare beside the package folder: those elsewhere in
/// the workspace, such as a file another folder's `include_str!` names. A
/// registry crate or the standard library is the same in every checkout, and
/// what a build script wrote under the target is not Git's. `None` past
/// [`MAX_OUTSIDE_SOURCES`].
fn outside_sources(
    package: &Path,
    workspace: Option<&Path>,
    target: Option<&Path>,
    sources: &[PathBuf],
) -> Option<Vec<PathBuf>> {
    let package = lexical(package);
    let mut outside = Vec::new();
    for source in sources {
        let source = lexical(source);
        let in_workspace = workspace.is_some_and(|root| source.starts_with(lexical(root)));
        let in_target = target.is_some_and(|target| source.starts_with(lexical(target)));
        if source.starts_with(&package) || !in_workspace || in_target || outside.contains(&source)
        {
            continue;
        }
        if outside.len() == MAX_OUTSIDE_SOURCES {
            return None;
        }
        outside.push(source);
    }
    Some(outside)
}

/// `path` with its `.` and `..` components resolved by name: dep-info spells
/// a file reached through `../` as written, inside the folder it leaves.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// Whether `package` and the `outside` files hold what the merge base of
/// `HEAD` and `reference` holds, in a repository that tracks the package.
#[cfg(test)]
fn package_at_merge_base(
    env: &InvocationEnv,
    reference: &str,
    package: &Path,
    outside: &[PathBuf],
) -> bool {
    package_against_merge_base(env, reference, package, outside) == Tree::Shared
}

/// What `package` and the `outside` files hold against the merge base of
/// `HEAD` and `reference` ([`Tree`]).
fn package_against_merge_base(
    env: &InvocationEnv,
    reference: &str,
    package: &Path,
    outside: &[PathBuf],
) -> Tree {
    let deadline = Instant::now() + GIT_TIMEOUT;
    // The comparison first: an edited package ends here, in one process.
    let mut diff: Vec<OsString> = ["diff", "--quiet", "--merge-base", reference, "--", "."]
        .into_iter()
        .map(OsString::from)
        .collect();
    diff.extend(outside.iter().map(|path| path.clone().into_os_string()));
    match git_status(env, package, &diff, deadline) {
        Some(0) => {}
        // `--quiet`: 1 for a difference, anything else for an error: no
        // repository, or no such ref or merge base.
        Some(1) => return Tree::Changed,
        Some(_) => return Tree::Outside,
        None => return Tree::Unknown,
    }
    // Nothing differs in a folder Git does not track either, and a package
    // there (a scratch crate in an ignored folder) is nobody else's.
    match git_status(
        env,
        package,
        &["ls-files", "--error-unmatch", "--", "Cargo.toml"],
        deadline,
    ) {
        Some(0) => Tree::Shared,
        Some(1) => Tree::Changed,
        Some(_) => Tree::Outside,
        None => Tree::Unknown,
    }
}

/// The exit code of `git -C dir <args>`, when it exits before `deadline`,
/// run as the invocation would run it. The repository's filesystem monitor
/// stays off (a configured one is a program Git would start), and Git takes
/// no optional lock. `git diff` still refreshes the index under `index.lock`
/// when stats moved, so a Git past `deadline` is stopped with [`stop`].
fn git_status<S: AsRef<OsStr>>(
    env: &InvocationEnv,
    dir: &Path,
    args: &[S],
    deadline: Instant,
) -> Option<i32> {
    let Ok(mut command) = env.command("git") else {
        return None;
    };
    command
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = crate::platform::host_spawn(&mut command) else {
        return None;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(GIT_POLL),
            Ok(None) | Err(_) => {
                stop(&mut child);
                return None;
            }
        }
    }
}

/// Stops a Git that ran past its deadline. SIGTERM first: Git's signal
/// handler removes the lock files it holds (`index.lock` while `git diff`
/// refreshes the index), which a SIGKILL would leave behind for the user's
/// next Git command in that checkout to fail on. Killed after [`GIT_GRACE`].
fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `pid` is this process's own child, not yet waited for, so
        // the id still names it and no other process.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let grace = Instant::now() + GIT_GRACE;
        while Instant::now() < grace {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(GIT_POLL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=Kache Test",
                "-c",
                "user.email=kache@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_missing() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }

    /// A repository with two packages and a file both could include, all
    /// committed, and the branch `shared` at that commit.
    fn repository(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        for (file, contents) in [
            ("low/Cargo.toml", "[package]\nname = \"low\"\n"),
            ("low/src/lib.rs", "pub fn low() {}\n"),
            ("top/Cargo.toml", "[package]\nname = \"top\"\n"),
            ("top/src/lib.rs", "pub fn top() {}\n"),
            ("assets/words.txt", "one\n"),
            (".gitignore", "/scratch\n"),
        ] {
            let path = repo.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        git(&repo, &["branch", "shared"]);
        repo
    }

    /// `git diff` refreshes the index under `index.lock` even with
    /// `GIT_OPTIONAL_LOCKS=0`, and Git removes its lock files when it is
    /// asked to stop. A Git past its deadline is asked first: one killed
    /// outright leaves `index.lock` behind, and the user's next Git command in
    /// that checkout fails on it.
    #[cfg(unix)]
    #[test]
    fn a_git_past_its_deadline_is_asked_to_stop_before_it_is_killed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let started = dir.path().join("started");
        let lock = dir.path().join("index.lock");
        // A Git that takes its lock, removes it on SIGTERM as Git's own
        // handler does, and never finishes.
        let script = bin.join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{lock}'\ntrap 'rm -f \"{lock}\"; exit 143' TERM\ntouch '{started}'\nwhile :; do sleep 0.02; done\n",
                lock = lock.display(),
                started = started.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = InvocationEnv::capture();
        let mut path = bin.clone().into_os_string();
        if let Some(rest) = env.var_os("PATH") {
            path.push(":");
            path.push(rest);
        }
        env.set("PATH", path);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(git_status(&env, dir.path(), &["diff"], deadline), None);
        assert!(started.exists(), "the stand-in Git ran and took its lock");
        assert!(!lock.exists(), "a Git stopped at its deadline left index.lock");
    }

    fn at_merge_base(repo: &Path, package: &str, outside: &[PathBuf]) -> bool {
        let mut env = InvocationEnv::capture();
        // A Git hook's own variables must not lead Git to another repository.
        env.set("GIT_DIR", repo.join("elsewhere"));
        package_at_merge_base(&env, "shared", &repo.join(package), outside)
    }

    #[test]
    fn a_package_is_shared_until_the_branch_changes_it() {
        if git_missing() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repository(dir.path());
        assert!(at_merge_base(&repo, "low", &[]));
        assert!(at_merge_base(&repo, "top", &[]));

        // An edit in the working tree, then committed: the branch's own.
        std::fs::write(repo.join("low/src/lib.rs"), "pub fn low() -> u8 { 1 }\n").unwrap();
        assert!(!at_merge_base(&repo, "low", &[]));
        assert!(at_merge_base(&repo, "top", &[]), "another package's edit");
        git(&repo, &["commit", "-q", "-a", "-m", "edit"]);
        assert!(!at_merge_base(&repo, "low", &[]));

        // Once the shared ref has the commit, the package is everyone's.
        git(&repo, &["branch", "-f", "shared", "HEAD"]);
        assert!(at_merge_base(&repo, "low", &[]));

        // A checkout behind the shared ref builds what the ref once held.
        git(&repo, &["checkout", "-q", "--detach", "HEAD~1"]);
        assert!(at_merge_base(&repo, "low", &[]));
    }

    #[test]
    fn a_source_outside_the_package_counts() {
        if git_missing() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repository(dir.path());
        let words = repo.join("assets/words.txt");
        assert!(at_merge_base(&repo, "top", std::slice::from_ref(&words)));
        std::fs::write(&words, "two\n").unwrap();
        assert!(!at_merge_base(&repo, "top", &[words]));
        assert!(at_merge_base(&repo, "top", &[]));
    }

    /// An untracked package is the checkout's own; an unknown ref or no
    /// repository at all is no answer, which keeps a unit on the lanes it
    /// had without the shared ref.
    #[test]
    fn an_untracked_package_is_changed_and_an_unknown_ref_or_no_repository_outside() {
        if git_missing() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = repository(dir.path());
        let scratch = repo.join("scratch/Cargo.toml");
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        std::fs::write(scratch, "[package]\nname = \"scratch\"\n").unwrap();
        assert!(!at_merge_base(&repo, "scratch", &[]), "ignored folder");

        let env = InvocationEnv::capture();
        assert_eq!(
            package_against_merge_base(&env, "shared", &repo.join("scratch"), &[]),
            Tree::Changed
        );
        assert_eq!(
            package_against_merge_base(&env, "shared", &repo.join("low"), &[]),
            Tree::Shared
        );
        std::fs::write(repo.join("low/src/lib.rs"), "pub fn low() -> u8 { 1 }\n").unwrap();
        assert_eq!(
            package_against_merge_base(&env, "shared", &repo.join("low"), &[]),
            Tree::Changed
        );
        assert_eq!(
            package_against_merge_base(&env, "origin/missing", &repo.join("low"), &[]),
            Tree::Outside
        );
        let loose = dir.path().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::write(loose.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            package_against_merge_base(&env, "shared", &loose, &[]),
            Tree::Outside
        );
    }

    #[test]
    fn only_workspace_sources_outside_the_package_are_compared() {
        let workspace = Path::new("/work/repo");
        let package = workspace.join("crates/top");
        let target = workspace.join("target");
        let sources = [
            package.join("src/lib.rs"),
            // Spelled through the package, as dep-info writes an include.
            package.join("src/../../../assets/words.txt"),
            workspace.join("assets/words.txt"),
            target.join("debug/build/top-1/out/generated.rs"),
            PathBuf::from("/home/user/.cargo/registry/src/index/dep-1.0.0/src/lib.rs"),
        ];
        assert_eq!(
            outside_sources(&package, Some(workspace), Some(&target), &sources),
            Some(vec![workspace.join("assets/words.txt")])
        );
        assert_eq!(
            outside_sources(&package, None, Some(&target), &sources),
            Some(Vec::new()),
            "no workspace root, nothing to name"
        );
        let many: Vec<PathBuf> = (0..=MAX_OUTSIDE_SOURCES)
            .map(|index| workspace.join(format!("assets/{index}.txt")))
            .collect();
        assert_eq!(
            outside_sources(&package, Some(workspace), Some(&target), &many),
            None
        );
    }
}
