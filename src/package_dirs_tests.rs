// Symlinks: a pool member's home is a Unix layout.
#![cfg(unix)]

use super::*;
use tempfile::TempDir;

/// Only a path under a home's linked `registry` or `git` changes spelling:
/// not one under a home's own downloads, nor one outside them.
#[test]
fn only_paths_under_a_linked_registry_or_git_are_respelled() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let shared = root.join("shared");
    let member = root.join("member.cargo");
    for path in [
        shared.join("registry/src/idx/dep-1.0.0"),
        shared.join("git/checkouts/repo/abc"),
        member.clone(),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::os::unix::fs::symlink(shared.join("registry"), member.join("registry")).unwrap();
    std::os::unix::fs::symlink(shared.join("git"), member.join("git")).unwrap();

    let cases = [
        (
            member.join("registry/src/idx/dep-1.0.0"),
            Some(shared.join("registry/src/idx/dep-1.0.0")),
        ),
        (
            member.join("git/checkouts/repo/abc/Cargo.toml"),
            Some(shared.join("git/checkouts/repo/abc/Cargo.toml")),
        ),
        (member.join("registry-other/dep"), None),
        (root.join("project"), None),
    ];
    for (path, expected) in cases {
        assert_eq!(
            through_linked_home(&member, &path),
            expected,
            "{}",
            path.display()
        );
    }
    // The shared home's own downloads are already spelled one way.
    assert_eq!(
        through_linked_home(&shared, &shared.join("registry/src/idx/dep-1.0.0")),
        None
    );

    let mut env = InvocationEnv::from_parts(
        [
            ("CARGO_HOME", member.as_os_str().to_owned()),
            (
                "CARGO_MANIFEST_DIR",
                member.join("registry/src/idx/dep-1.0.0").into_os_string(),
            ),
            (
                "CARGO_MANIFEST_PATH",
                member
                    .join("registry/src/idx/dep-1.0.0/Cargo.toml")
                    .into_os_string(),
            ),
        ],
        Some(root.clone()),
        Some(root.join("home")),
        root.join("tmp"),
    );
    respell_for(&mut env);
    assert_eq!(
        env.var_os("CARGO_MANIFEST_DIR"),
        Some(shared.join("registry/src/idx/dep-1.0.0").into_os_string())
    );
    assert_eq!(
        env.var_os("CARGO_MANIFEST_PATH"),
        Some(
            shared
                .join("registry/src/idx/dep-1.0.0/Cargo.toml")
                .into_os_string()
        )
    );
}
