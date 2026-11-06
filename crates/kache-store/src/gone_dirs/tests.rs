use super::*;
use crate::file_hash::ensure_file_hash_cache_schema;
use rusqlite::Connection;

fn memo() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    ensure_file_hash_cache_schema(&db).unwrap();
    db
}

/// A row for `path`, written at Unix second `at`.
fn row(db: &Connection, path: &str, at: i64) {
    db.execute(
        "INSERT INTO file_hashes (path, size, mtime_ns, hash, updated_at)
         VALUES (?1, 1, 1, 'h', datetime(?2, 'unixepoch'))",
        params![path, at],
    )
    .unwrap();
}

fn paths(db: &Connection) -> Vec<String> {
    db.prepare("SELECT path FROM file_hashes ORDER BY path")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Lookups from a table: a directory not named is there.
fn looks(table: &'static [(&'static str, DirState)]) -> impl Fn(&str) -> DirState {
    move |dir| {
        table
            .iter()
            .find(|(name, _)| *name == dir)
            .map_or(DirState::Present, |(_, state)| *state)
    }
}

const SEEN: i64 = 1_800_000_000;

#[test]
fn a_gone_directory_loses_its_rows_and_everything_else_stays() {
    let db = memo();
    for path in [
        "/wt/gone/src/lib.rs",
        "/wt/gone/target/debug/deps/liba.rmeta",
        "/wt/live/src/lib.rs",
        // Siblings whose names start with the gone one's, on both sides of
        // `/` in byte order.
        "/wt/gone-2/src/lib.rs",
        "/wt/gone0/lib.rs",
        "/wt/gone5/lib.rs",
        "/wt/live/target/gone/old.rmeta",
        "relative/path.rs",
    ] {
        row(&db, path, SEEN - 60);
    }
    let cache = FileHashCache::Borrowed(&db);
    let look = looks(&[
        ("/wt/gone", DirState::Gone(SEEN)),
        ("/wt/live/target/gone", DirState::Gone(SEEN)),
    ]);
    let done = cache
        .prune_file_hashes_under_gone_dirs_with(Duration::from_secs(60), 2, &look)
        .unwrap();
    assert_eq!(
        paths(&db),
        [
            "/wt/gone-2/src/lib.rs",
            "/wt/gone0/lib.rs",
            "/wt/gone5/lib.rs",
            "/wt/live/src/lib.rs",
            "relative/path.rs",
        ]
    );
    assert_eq!((done.rows, done.dirs, done.cut_short), (3, 2, false));
}

#[test]
fn rows_written_after_the_directory_was_seen_gone_stay() {
    // A pool worktree removed and made again under the same name: the
    // build filling the new one writes rows after the lookup.
    let db = memo();
    row(&db, "/pool/w01/old.rs", SEEN - 1);
    row(&db, "/pool/w01/new.rs", SEEN);
    row(&db, "/pool/w01/newer.rs", SEEN + 30);
    let cache = FileHashCache::Borrowed(&db);
    let look = looks(&[("/pool/w01", DirState::Gone(SEEN))]);
    cache
        .prune_file_hashes_under_gone_dirs_with(Duration::from_secs(60), 100, &look)
        .unwrap();
    assert_eq!(paths(&db), ["/pool/w01/new.rs", "/pool/w01/newer.rs"]);
}

#[test]
fn a_directory_the_lookup_is_unsure_of_keeps_its_rows() {
    let db = memo();
    row(&db, "/home/other/wt/a.rs", SEEN - 60);
    row(&db, "/home/other/wt/b.rs", SEEN - 60);
    row(&db, "/home/zgone/a.rs", SEEN - 60);
    let cache = FileHashCache::Borrowed(&db);
    let look = looks(&[
        ("/home/other", DirState::Unknown),
        ("/home/zgone", DirState::Gone(SEEN)),
    ]);
    let done = cache
        .prune_file_hashes_under_gone_dirs_with(Duration::from_secs(60), 100, &look)
        .unwrap();
    assert_eq!(paths(&db), ["/home/other/wt/a.rs", "/home/other/wt/b.rs"]);
    assert_eq!(done.rows, 1, "the walk went past the unsure directory");
}

#[test]
fn the_prune_stops_at_its_budget_and_the_next_one_goes_on() {
    let db = memo();
    for n in 0..5 {
        row(&db, &format!("/gone/f{n}.rs"), SEEN - 60);
    }
    let cache = FileHashCache::Borrowed(&db);
    let look = looks(&[("/gone", DirState::Gone(SEEN))]);
    let none = cache
        .prune_file_hashes_under_gone_dirs_with(Duration::ZERO, 2, &look)
        .unwrap();
    assert_eq!((none.rows, none.batches, none.cut_short), (0, 0, true));
    assert_eq!(paths(&db).len(), 5);
    let all = cache
        .prune_file_hashes_under_gone_dirs_with(Duration::from_secs(60), 2, &look)
        .unwrap();
    assert_eq!((all.rows, all.batches, all.cut_short), (5, 3, false));
    assert!(paths(&db).is_empty());
}

#[test]
fn a_writer_holding_the_index_cuts_the_prune_short_without_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    ensure_file_hash_cache_schema(&db).unwrap();
    row(&db, "/gone/a.rs", SEEN - 60);
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    db.busy_timeout(Duration::ZERO).unwrap();
    let cache = FileHashCache::Borrowed(&db);
    let look = looks(&[("/gone", DirState::Gone(SEEN))]);
    let done = cache
        .prune_file_hashes_under_gone_dirs_with(Duration::from_secs(60), 100, &look)
        .unwrap();
    assert_eq!((done.rows, done.cut_short), (0, true));
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(paths(&db), ["/gone/a.rs"]);
}

#[cfg(unix)]
#[test]
fn on_disk_a_removed_directory_goes_and_an_unsearchable_one_stays() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let base = root.path().to_str().unwrap().to_owned();
    for dir in ["live", "removed", "locked/inner"] {
        std::fs::create_dir_all(format!("{base}/{dir}")).unwrap();
    }
    std::fs::remove_dir(format!("{base}/removed")).unwrap();
    let locked = format!("{base}/locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root searches every directory: then the lookup is sure, and only the
    // removed directory's rows are in question.
    let searchable = std::fs::symlink_metadata(format!("{locked}/inner")).is_ok();
    let db = memo();
    let an_hour_ago = unix_now() - 3600;
    for dir in ["live", "removed", "locked/inner"] {
        row(&db, &format!("{base}/{dir}/a.rs"), an_hour_ago);
    }
    let cache = FileHashCache::Borrowed(&db);
    let done = cache
        .prune_file_hashes_under_gone_dirs(Duration::from_secs(60))
        .unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        paths(&db),
        [
            format!("{base}/live/a.rs"),
            format!("{base}/locked/inner/a.rs"),
        ],
        "searchable as this user: {searchable}"
    );
    assert_eq!((done.rows, done.dirs), (1, 1));
}
