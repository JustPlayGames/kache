//! Target-local metadata translated before a seeded unit is published.

use std::borrow::Cow;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{Layout, Unit, build_script_out_dir};

// Metadata beyond this bound costs a normal Cargo rebuild, rather than an
// unbounded allocation on the target-info probe's seeding path.
const MAX_METADATA_BYTES: u64 = 16 << 20;

pub(super) struct Relocation {
    from: PathBuf,
    to: PathBuf,
    depinfo_dirs: Vec<PathBuf>,
    script: Option<Script>,
}

struct Script {
    stdout: PathBuf,
    root_output: PathBuf,
    /// The run's `OUT_DIR` as the donor holds it and as its record names it,
    /// and as this target has it. Only Unix copies symbolic links.
    #[cfg_attr(not(unix), allow(dead_code))]
    out_dirs: [PathBuf; 2],
    #[cfg_attr(not(unix), allow(dead_code))]
    current_out_dir: PathBuf,
    translated_stdout: String,
    translated_root: String,
}

impl Relocation {
    /// `checkout`, the donor's checkout and this one, is given for a run of
    /// another checkout that [`super::workspace`] verified: its directives
    /// that name the donor's checkout then name this one
    /// ([`rebase_checkout`]).
    pub(super) fn for_unit(
        from: &Path,
        to: &Path,
        layout: Layout,
        unit: &Unit,
        checkout: Option<(&Path, &Path)>,
        deadline: Instant,
    ) -> io::Result<Self> {
        let out_dir = build_script_out_dir(from, layout, unit);
        let stdout = crate::cargo_layout::build_script_stdout(&out_dir)
            .ok_or_else(|| io::Error::other("unknown seeded build-script layout"))?;
        let root_output = stdout.with_file_name("root-output");
        let root = read_text(&root_output, deadline)?;
        let output = read_text(&stdout, deadline)?;
        // Generated OUT_DIR files are script-owned data, even when their
        // names end in .d. Only rustc's direct output locations are dep-info.
        let depinfo_dirs = match layout {
            Layout::Shared => vec![
                from.join("deps"),
                from.join("examples"),
                from.join("build")
                    .join(format!("{}-{}", unit.package, unit.hash)),
            ],
            Layout::PerUnit if output.is_none() => vec![out_dir.clone()],
            Layout::PerUnit => Vec::new(),
        };
        let script = match (root, output) {
            (Some(root), Some(output)) => {
                let recorded = root.trim();
                let named =
                    crate::cargo_layout::out_dir_unit_name(Path::new(recorded), &unit.package);
                let expected = format!("{}-{}", unit.package, unit.hash);
                if named.as_deref() != Some(expected.as_str()) {
                    return Err(io::Error::other("unrecognized seeded root-output"));
                }
                let current = build_script_out_dir(to, layout, unit);
                let current = current
                    .to_str()
                    .ok_or_else(|| io::Error::other("seeded OUT_DIR is not UTF-8"))?;
                let mut translated_stdout = rebase_stdout(&output, recorded, current);
                if let Some((theirs, ours)) = checkout {
                    translated_stdout = rebase_checkout(&translated_stdout, theirs, ours)
                        .ok_or_else(|| io::Error::other("checkout is not UTF-8"))?;
                }
                Some(Script {
                    stdout,
                    root_output,
                    out_dirs: [out_dir.clone(), PathBuf::from(recorded)],
                    current_out_dir: PathBuf::from(current),
                    translated_stdout,
                    translated_root: current.to_owned(),
                })
            }
            (None, None) => None,
            _ => return Err(io::Error::other("unpaired seeded build-script records")),
        };
        Ok(Self {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            depinfo_dirs,
            script,
        })
    }

    /// Where a symbolic link of the run's `OUT_DIR` points in this target,
    /// when the donor's points by an absolute path into that very
    /// `OUT_DIR` (cxx's headers name their `.rs.h` beside them so): the
    /// same entry of this target's `OUT_DIR`, as the script writes it here.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn out_dir_link(&self, target: &Path) -> Option<PathBuf> {
        let script = self.script.as_ref()?;
        script.out_dirs.iter().find_map(|out_dir| {
            let rest = target.strip_prefix(out_dir).ok()?;
            // An entry inside it: a link to the directory itself is a
            // hermetic run's sealed `OUT_DIR`, which has a way of its own.
            (rest.components().next().is_some()
                && rest
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))))
            .then(|| script.current_out_dir.join(rest))
        })
    }

    pub(super) fn contents(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> io::Result<Option<Cow<'_, [u8]>>> {
        if let Some(script) = &self.script {
            if path == script.stdout {
                return Ok(Some(Cow::Borrowed(script.translated_stdout.as_bytes())));
            }
            if path == script.root_output {
                return Ok(Some(Cow::Borrowed(script.translated_root.as_bytes())));
            }
        }
        if path.extension() != Some(std::ffi::OsStr::new("d"))
            || !self
                .depinfo_dirs
                .iter()
                .any(|dir| path.parent() == Some(dir.as_path()))
        {
            return Ok(None);
        }
        let original = read_text(path, deadline)?
            .ok_or_else(|| io::Error::other("seeded dep-info disappeared"))?;
        let translated = kache_store::link::rebase_depinfo_content(&original, &self.from, &self.to)
            .ok_or_else(|| io::Error::other("seeded dep-info does not belong to this donor"))?;
        Ok((translated != original).then(|| Cow::Owned(translated.into_bytes())))
    }
}

fn read_text(path: &Path, deadline: Instant) -> io::Result<Option<String>> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > MAX_METADATA_BYTES {
        return Err(io::Error::other("unsupported seeded metadata"));
    }
    read_bounded_text(std::fs::File::open(path)?).map(Some)
}

fn read_bounded_text(reader: impl Read) -> io::Result<String> {
    let mut text = String::new();
    reader
        .take(MAX_METADATA_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_METADATA_BYTES {
        return Err(io::Error::other("seeded metadata grew beyond its limit"));
    }
    Ok(text)
}

/// Cargo rewrites the recorded OUT_DIR in directive values before parsing them
/// (cargo/src/compiler/custom_build.rs, BuildOutput::parse). Non-directive
/// stdout is opaque and does not participate in its replay.
fn rebase_stdout(output: &str, recorded: &str, current: &str) -> String {
    output
        .split_inclusive('\n')
        .map(|line| {
            if line.trim_start().starts_with("cargo:") {
                line.replace(recorded, current)
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// The directives whose path may name a checkout of the workspace when a
/// run of another checkout is copied: Cargo watches the first for the run's
/// freshness, and the linker searches the second when it links a unit above
/// the run. Any other directive naming the donor's checkout (an environment
/// variable or `cfg` a compiler reads, a link argument, metadata another
/// script reads) keeps the run out.
const CHECKOUT_DIRECTIVES: [&str; 2] = ["rerun-if-changed", "rustc-link-search"];

/// The kinds `rustc-link-search` takes before a `=` (Cargo's
/// `PathKind`); a value without one is a plain path.
const LINK_SEARCH_KINDS: [&str; 5] = ["native", "crate", "dependency", "framework", "all"];

/// The path of a directive line of [`CHECKOUT_DIRECTIVES`], in either
/// syntax (`cargo:` or `cargo::`), as the range of `line` it takes; the
/// line's ending is not part of it.
pub(super) fn checkout_directive(line: &str) -> Option<std::ops::Range<usize>> {
    let content = line.trim_end_matches(['\n', '\r']);
    let trimmed = content.trim_start();
    let body = trimmed
        .strip_prefix("cargo::")
        .or_else(|| trimmed.strip_prefix("cargo:"))?;
    let (key, value) = body.split_once('=')?;
    if !CHECKOUT_DIRECTIVES.contains(&key) {
        return None;
    }
    let path = match value.split_once('=') {
        Some((kind, path)) if key == "rustc-link-search" && LINK_SEARCH_KINDS.contains(&kind) => {
            path
        }
        _ => value,
    };
    // `path` is a suffix of `content`.
    Some(content.len() - path.len()..content.len())
}

/// The part of `path` after `root`, when `path` is `root` or a path below
/// it: `/w/a` is below `/w`, `/w2/a` is not.
pub(super) fn below_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(root)?;
    (rest.is_empty() || rest.starts_with(['/', '\\'])).then_some(rest)
}

/// `output` with the donor's checkout `theirs` replaced by `ours` at the
/// start of each [`checkout_directive`]'s path. [`super::workspace`] keeps
/// out a run whose record names the donor's checkout anywhere else, so this
/// is the output the run would have recorded here, as Cargo reads it: a
/// `rerun-if-changed` path inside the package becomes the same path from
/// the package in Cargo's fingerprint, and the linker searches this
/// checkout's directory (whose files were found the same). `None` when a
/// root is not UTF-8.
pub(super) fn rebase_checkout(output: &str, theirs: &Path, ours: &Path) -> Option<String> {
    let (theirs, ours) = (theirs.to_str()?, ours.to_str()?);
    Some(
        output
            .split_inclusive('\n')
            .map(|line| {
                let Some(span) = checkout_directive(line) else {
                    return line.to_owned();
                };
                match below_root(&line[span.clone()], theirs) {
                    Some(rest) => {
                        format!("{}{ours}{rest}{}", &line[..span.start], &line[span.end..])
                    }
                    None => line.to_owned(),
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_metadata_file_at_the_limit_is_read_but_growth_is_bounded() {
        let exact = vec![b'a'; 16 << 20];
        assert_eq!(
            read_bounded_text(exact.as_slice()).unwrap().as_bytes(),
            exact
        );
        assert!(
            read_bounded_text(std::io::repeat(b'a')).is_err(),
            "even a growing source is capped"
        );
        assert!(
            read_bounded_text(&[0xff][..]).is_err(),
            "binary metadata cannot be relocated"
        );
    }

    #[test]
    fn reading_metadata_distinguishes_missing_unsupported_and_expired() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        assert_eq!(read_text(&missing, deadline).unwrap(), None);
        assert!(read_text(dir.path(), deadline).is_err());
        let exact = dir.path().join("exact");
        std::fs::File::create(&exact)
            .unwrap()
            .set_len(16 << 20)
            .unwrap();
        assert_eq!(
            read_text(&exact, deadline).unwrap().unwrap().len(),
            16 << 20
        );
        assert_eq!(
            read_text(&missing, Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[cfg(unix)]
    #[test]
    fn reading_metadata_refuses_a_leaf_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("stdout");
        let linked = dir.path().join("linked-stdout");
        let text = "cargo:rustc-cfg=seeded\n";
        std::fs::write(&regular, text).unwrap();
        std::os::unix::fs::symlink(&regular, &linked).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        assert_eq!(
            read_text(&regular, deadline).unwrap().as_deref(),
            Some(text)
        );
        assert!(
            read_text(&linked, deadline).is_err(),
            "seed metadata must not follow a leaf symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reading_metadata_propagates_non_missing_lookup_errors() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("stdout");
        std::fs::write(&anchor, "cargo:rustc-cfg=seeded\n").unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);

        assert_eq!(
            read_text(&dir.path().join("missing"), deadline).unwrap(),
            None
        );
        assert_eq!(
            read_text(&anchor.join("child"), deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn stdout_relocation_matches_cargos_directive_value_replay() {
        let original = "opaque /old/out\ncargo:warning=/old/out/value\n cargo::rustc-env=PATH=/old/out/native\ncargo::rerun-if-changed=/old/out/source\n";
        assert_eq!(
            rebase_stdout(original, "/old/out", "/new/out"),
            "opaque /old/out\ncargo:warning=/new/out/value\n cargo::rustc-env=PATH=/new/out/native\ncargo::rerun-if-changed=/new/out/source\n"
        );
        assert_eq!(rebase_stdout("", "/old/out", "/new/out"), "");
    }

    #[test]
    fn a_link_into_the_runs_own_out_dir_points_into_this_targets() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (
            dir.path().join("donor/debug"),
            dir.path().join("ours/debug"),
        );
        let unit = Unit {
            package: "dep".into(),
            hash: "0123456789abcdef".into(),
        };
        let out = build_script_out_dir(&from, Layout::Shared, &unit);
        let recorded = dir
            .path()
            .join("moved/debug/build/dep-0123456789abcdef/out");
        let stdout = crate::cargo_layout::build_script_stdout(&out).unwrap();
        std::fs::create_dir_all(stdout.parent().unwrap()).unwrap();
        std::fs::write(&stdout, "cargo:rustc-cfg=x\n").unwrap();
        std::fs::write(
            stdout.with_file_name("root-output"),
            recorded.to_str().unwrap(),
        )
        .unwrap();
        let relocation = Relocation::for_unit(
            &from,
            &to,
            Layout::Shared,
            &unit,
            None,
            Instant::now() + std::time::Duration::from_secs(60),
        )
        .unwrap();
        let current = build_script_out_dir(&to, Layout::Shared, &unit);
        for base in [&out, &recorded] {
            assert_eq!(
                relocation.out_dir_link(&base.join("include/a.rs.h")),
                Some(current.join("include/a.rs.h"))
            );
        }
        for elsewhere in [
            out.clone(),
            recorded.clone(),
            out.join("../output"),
            out.join("include/../../output"),
            PathBuf::from("/usr/include/a.h"),
            from.join("deps/libx.rlib"),
        ] {
            assert_eq!(
                relocation.out_dir_link(&elsewhere),
                None,
                "{}",
                elsewhere.display()
            );
        }
    }

    #[test]
    fn a_verified_runs_checkout_directives_name_this_checkout() {
        let output = concat!(
            "cargo:rerun-if-changed=/w/a/crates/p/../../vendor/lib.a\n",
            "cargo::rustc-link-search=native=/w/a/vendor\n",
            "cargo:rustc-link-search=/w/a\r\n",
            "cargo:rerun-if-changed=/w/ab/other\n",
            "cargo:rustc-env=DIR=/w/a/x\n",
            "/w/a/opaque\n",
            "cargo:rerun-if-changed=build.rs",
        );
        assert_eq!(
            rebase_checkout(output, Path::new("/w/a"), Path::new("/n/b")).unwrap(),
            concat!(
                "cargo:rerun-if-changed=/n/b/crates/p/../../vendor/lib.a\n",
                "cargo::rustc-link-search=native=/n/b/vendor\n",
                "cargo:rustc-link-search=/n/b\r\n",
                "cargo:rerun-if-changed=/w/ab/other\n",
                "cargo:rustc-env=DIR=/w/a/x\n",
                "/w/a/opaque\n",
                "cargo:rerun-if-changed=build.rs",
            ),
            "only the two directives' paths, and only below the checkout"
        );
    }

    #[test]
    fn a_script_recorded_before_the_donor_moves_is_rebased_from_its_recorded_root() {
        for layout in [Layout::PerUnit, Layout::Shared] {
            let dir = tempfile::tempdir().unwrap();
            let from = dir.path().join("donor/debug");
            let to = dir.path().join("consumer/debug");
            let unit = Unit {
                package: "dep".into(),
                hash: "0123456789abcdef".into(),
            };
            let old = build_script_out_dir(&dir.path().join("removed/debug"), layout, &unit);
            let stdout = crate::cargo_layout::build_script_stdout(&build_script_out_dir(
                &from, layout, &unit,
            ))
            .unwrap();
            std::fs::create_dir_all(stdout.parent().unwrap()).unwrap();
            std::fs::write(
                &stdout,
                format!("cargo:rerun-if-changed={}/input\n", old.display()),
            )
            .unwrap();
            let root = stdout.with_file_name("root-output");
            std::fs::write(&root, old.to_str().unwrap()).unwrap();
            let relocation = Relocation::for_unit(
                &from,
                &to,
                layout,
                &unit,
                None,
                Instant::now() + std::time::Duration::from_secs(60),
            )
            .unwrap();
            let current = build_script_out_dir(&to, layout, &unit);
            let generated_data = build_script_out_dir(&from, layout, &unit).join("config.d");
            std::fs::create_dir_all(generated_data.parent().unwrap()).unwrap();
            let data = format!("value: {}/opaque\n", from.display());
            std::fs::write(&generated_data, &data).unwrap();
            assert!(
                relocation
                    .contents(&generated_data, Instant::now())
                    .unwrap()
                    .is_none(),
                "script-generated .d data is not compiler dep-info"
            );
            assert_eq!(
                relocation
                    .contents(&root, Instant::now())
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                current.to_str().unwrap().as_bytes()
            );
            assert_eq!(
                relocation
                    .contents(&stdout, Instant::now())
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                format!("cargo:rerun-if-changed={}/input\n", current.display()).as_bytes()
            );
        }
    }
}
