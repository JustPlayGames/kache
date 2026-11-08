//! A downloaded package's directory in one spelling, whatever Cargo home
//! links to it.
//!
//! A worktree pool gives each member a Cargo home of its own whose `registry`
//! and `git` link into a shared home (`~/.cargo`), so that the members share
//! one copy of every download. Cargo spells a package's directory through the
//! home it runs with and hands that spelling to rustc as `CARGO_MANIFEST_DIR`
//! (and `CARGO_MANIFEST_PATH`). A crate that reads one with `env!` bakes it,
//! and its key keeps it as given (kunobi-ninja/kache#167): the same package
//! keyed apart in every member, and every crate built on it with it.
//!
//! It is one directory through either home. Spelled through the link's
//! target, the compile reads the same files, every member builds the same
//! bytes under one key, and the path a restored artifact bakes is valid in
//! every member. Cargo leaves both variables out of its own freshness checks,
//! since it sets them itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kache_fs::paths::strip_verbatim_prefix;

use crate::invocation::InvocationEnv;
use crate::path_normalizer::{LINKED_CARGO_HOME_DIRS, linked_cargo_home_dir};

/// The variables Cargo sets to a path inside the package.
const PACKAGE_PATH_VARS: [&str; 2] = ["CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH"];

/// Spell this compile's package paths through the shared home when they sit
/// in a Cargo home's linked `registry` or `git`.
pub(crate) fn respell_for(env: &mut InvocationEnv) {
    let Some(home) = cargo_home(env) else {
        return;
    };
    for var in PACKAGE_PATH_VARS {
        let respelled = env
            .var_os(var)
            .and_then(|value| through_linked_home(&home, Path::new(&value)));
        if let Some(respelled) = respelled {
            tracing::debug!(var, value = %respelled.display(), "package path spelled through the shared Cargo home");
            env.set(var, OsString::from(respelled));
        }
    }
}

/// The Cargo home this compile's Cargo runs with, as Cargo spells it.
fn cargo_home(env: &InvocationEnv) -> Option<PathBuf> {
    env.var_os("CARGO_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| env.home_dir().map(|home| home.join(".cargo")))
        .and_then(|home| env.resolve(&home))
}

/// `path` through the directory that `home`'s `registry` or `git` links to,
/// when it lies under one that links into another home; `None` otherwise.
fn through_linked_home(home: &Path, path: &Path) -> Option<PathBuf> {
    LINKED_CARGO_HOME_DIRS.into_iter().find_map(|sub| {
        let below = path.strip_prefix(home.join(sub)).ok()?;
        let target = linked_cargo_home_dir(home, sub)?;
        let target = PathBuf::from(strip_verbatim_prefix(target.to_str()?).as_ref());
        Some(target.join(below))
    })
}

#[cfg(test)]
#[path = "package_dirs_tests.rs"]
mod tests;
