use super::*;
use std::os::unix::fs::MetadataExt;

/// A tracked target under `dir` with `files` hardlinked from `blob`, plus a
/// file of its own and a symlink to the blob, last used `idle_days` ago.
fn target(store: &Store, dir: &Path, name: &str, blob: &Path, idle_days: i64) -> PathBuf {
    let workspace = dir.join("worktrees").join(name);
    let target = dir.join("targets").join(name);
    let deps = target.join("debug/deps");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&deps).unwrap();
    std::fs::write(
        target.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    std::fs::hard_link(blob, deps.join("libdep.rlib")).unwrap();
    std::fs::write(deps.join("own.o"), b"not the store's").unwrap();
    std::os::unix::fs::symlink(blob, deps.join("link.rlib")).unwrap();
    store.remember_target_root(&target, &workspace).unwrap();
    store
        .file_hash_cache()
        .db()
        .execute(
            "UPDATE target_roots SET last_seen = last_seen - ?2 WHERE path = ?1",
            rusqlite::params![target.to_string_lossy().into_owned(), idle_days * 86_400],
        )
        .unwrap();
    target
}

#[test]
fn stale_targets_lists_store_links_and_unlinks_them_only_when_applied() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let config = crate::test_support::test_config(dir.join("cache"));
    let store = Store::open(&config).unwrap();
    let output = dir.join("out.rlib");
    std::fs::write(&output, vec![b'r'; 4096]).unwrap();
    store
        .put(
            "key",
            "dep",
            &["lib".into()],
            &[],
            "host",
            "dev",
            &[(output.clone(), "libdep.rlib".into())],
            "",
            "",
        )
        .unwrap();
    store.remove_clone_for_test(&output);
    let blob = store.blob_path(&store.get("key").unwrap().unwrap().files[0].hash);
    let stale = target(&store, &dir, "stale", &blob, 20);
    let fresh = target(&store, &dir, "fresh", &blob, 1);
    let link = |target: &Path| target.join("debug/deps/libdep.rlib");

    let listed = run(&config, 14, false).unwrap();
    assert_eq!(listed.targets.len(), 1, "{listed:?}");
    assert_eq!(listed.targets[0].path, stale);
    assert_eq!(listed.targets[0].files, [link(&stale)]);
    assert_eq!((listed.files, listed.bytes, listed.unlinked), (1, 4096, 0));
    assert!(link(&stale).exists(), "a dry run removes nothing");

    let applied = run(&config, 14, true).unwrap();
    assert_eq!((applied.unlinked, applied.left), (1, 0));
    assert!(!link(&stale).exists());
    assert!(stale.join("debug/deps/own.o").exists());
    assert!(
        stale.join("debug/deps/link.rlib").exists(),
        "symlinks are not followed"
    );
    assert!(
        link(&fresh).exists(),
        "a target used within the window is left alone"
    );
    assert_eq!(
        std::fs::metadata(&blob).unwrap().nlink(),
        2,
        "the store's and fresh's"
    );
    assert!(store.contains("key"), "entries are the sweep's to evict");
}
