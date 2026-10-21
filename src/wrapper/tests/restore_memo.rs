//! The file-hash memo rows a hit's restore writes, when the index is busy.

use super::*;
use std::sync::Mutex;

/// Above the memo's persistence floor.
const RESTORED_BYTES: usize = 128 * 1024;

/// A store with one blob, and the arguments of a unit whose entry restores
/// that blob as `libfoo.rlib`.
struct Fixture {
    dir: tempfile::TempDir,
    config: Config,
    store: Store,
    hash: String,
    /// What the store handed to its sink, as the daemon receives it.
    handed: Arc<Mutex<Vec<crate::store::MemoRow>>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("cache"));
        let mut store = Store::open(&config).unwrap();
        let handed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&handed);
        store.send_busy_memo_rows_to(Box::new(move |rows| {
            sink.lock().unwrap().extend_from_slice(rows);
            true
        }));
        let content = vec![b'r'; RESTORED_BYTES];
        let source = dir.path().join("source.rlib");
        std::fs::write(&source, &content).unwrap();
        let hash = crate::cache_key::hash_file(&source).unwrap();
        create_blob(&store, &hash, &content);
        Self {
            dir,
            config,
            store,
            hash,
            handed,
        }
    }

    /// Write what the store handed over, as the daemon does once the index
    /// is free; returns how many rows were written.
    fn daemon_writes_handed_rows(&self) -> usize {
        let rows = std::mem::take(&mut *self.handed.lock().unwrap());
        self.store.write_handed_memo_rows(&rows).unwrap()
    }

    fn out_dir(&self) -> PathBuf {
        self.dir.path().join("target/debug/deps")
    }

    fn restored(&self) -> PathBuf {
        self.out_dir().join("libfoo.rlib")
    }

    /// Restore the entry while another connection holds the index's write
    /// lock, as a miss storing its outputs does.
    fn restore_while_the_index_is_busy(&self) {
        let out_dir = self.out_dir();
        let args = rustc_args(&[
            "rustc",
            "src/lib.rs",
            "--crate-name",
            "foo",
            "--emit",
            "link",
            "--out-dir",
            &out_dir.to_string_lossy(),
        ]);
        let meta = entry_meta(
            "busy-key",
            vec![cached_file("libfoo.rlib", &self.hash)],
            &[],
        );
        let writer = rusqlite::Connection::open(self.config.cache_dir.join("index.db")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        restore_from_cache(
            &self.config,
            &RustcCompiler::new(),
            &self.store,
            &args,
            &meta,
            None,
        )
        .unwrap();
        writer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            self.handed.lock().unwrap().len(),
            1,
            "the busy index's rows go to the sink"
        );
    }

    /// Hash the restored file as the next unit's key would, through a fresh
    /// hasher: its digest and the bytes read for it.
    fn next_key_hash(&self) -> (String, u64) {
        let hasher = self.store.file_hasher();
        let hash = hasher.hash(&self.restored()).unwrap();
        (hash, hasher.stats().bytes_hashed)
    }
}

/// A restore that finds the index busy used to drop its memo rows after a
/// 100 ms wait, so every dependent read the restored file again.
#[test]
fn a_restore_under_a_busy_index_still_memoises_the_restored_file() {
    let fixture = Fixture::new();

    fixture.restore_while_the_index_is_busy();
    assert_eq!(fixture.daemon_writes_handed_rows(), 1);

    let (hash, bytes) = fixture.next_key_hash();
    assert_eq!(hash, fixture.hash);
    assert_eq!(
        bytes, 0,
        "the next key must take the restored file's hash from the memo"
    );
}

/// Same size, then the old mtime set back: only the ctime shows the write.
/// Unix only: Windows fingerprints carry the creation time instead.
#[cfg(unix)]
fn rewrite_in_place_keeping_mtime(path: &Path) {
    let before = std::fs::metadata(path).unwrap();
    // A later tick for the ctime than the restore's.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let mut permissions = before.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).unwrap();
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.write_all(&vec![b'w'; RESTORED_BYTES]).unwrap();
    drop(file);
    filetime::set_file_mtime(
        path,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .unwrap();
    let after = crate::cache_key::FileFingerprint::from_path(path).unwrap();
    let was = crate::cache_key::FileFingerprint::from_metadata(path, &before);
    assert_eq!(
        (after.size, after.mtime_ns, after.inode),
        (was.size, was.mtime_ns, was.inode)
    );
    assert_ne!(
        after.ctime_ns, was.ctime_ns,
        "the write must show in the ctime"
    );
}

/// A new inode at the path, with the old size and mtime.
fn replace_keeping_size_and_mtime(path: &Path) {
    let before = std::fs::metadata(path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let staged = path.with_extension("staged");
    std::fs::write(&staged, vec![b'n'; RESTORED_BYTES]).unwrap();
    filetime::set_file_mtime(
        &staged,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .unwrap();
    std::fs::rename(&staged, path).unwrap();
}

#[cfg(unix)]
#[test]
fn a_file_rewritten_in_place_after_its_row_landed_is_hashed_again() {
    let fixture = Fixture::new();
    fixture.restore_while_the_index_is_busy();
    assert_eq!(fixture.daemon_writes_handed_rows(), 1);

    rewrite_in_place_keeping_mtime(&fixture.restored());

    let (hash, bytes) = fixture.next_key_hash();
    assert_ne!(
        hash, fixture.hash,
        "the memo must not serve the old bytes' hash"
    );
    assert_eq!(
        hash,
        crate::cache_key::hash_file(&fixture.restored()).unwrap()
    );
    assert_eq!(bytes, RESTORED_BYTES as u64);
}

#[cfg(unix)]
#[test]
fn a_file_rewritten_in_place_before_its_row_landed_drops_the_row() {
    let fixture = Fixture::new();
    fixture.restore_while_the_index_is_busy();

    rewrite_in_place_keeping_mtime(&fixture.restored());

    assert_eq!(
        fixture.daemon_writes_handed_rows(),
        0,
        "a row that no longer describes its file is not written"
    );
    let (hash, bytes) = fixture.next_key_hash();
    assert_ne!(hash, fixture.hash);
    assert_eq!(bytes, RESTORED_BYTES as u64);
}

#[test]
fn a_replaced_file_is_hashed_again() {
    let fixture = Fixture::new();
    fixture.restore_while_the_index_is_busy();
    assert_eq!(fixture.daemon_writes_handed_rows(), 1);

    replace_keeping_size_and_mtime(&fixture.restored());

    let (hash, bytes) = fixture.next_key_hash();
    assert_ne!(hash, fixture.hash, "a new inode must not inherit the row");
    assert_eq!(
        hash,
        crate::cache_key::hash_file(&fixture.restored()).unwrap()
    );
    assert_eq!(bytes, RESTORED_BYTES as u64);
}

/// Without a daemon to take them, the rows are dropped as before, after
/// the longer wait: the restore itself still succeeds.
#[test]
fn a_restore_with_nowhere_to_hand_its_rows_still_restores() {
    let mut fixture = Fixture::new();
    fixture
        .store
        .send_busy_memo_rows_to(Box::new(|_: &[crate::store::MemoRow]| false));
    let out_dir = fixture.out_dir();
    let args = rustc_args(&[
        "rustc",
        "src/lib.rs",
        "--crate-name",
        "foo",
        "--emit",
        "link",
        "--out-dir",
        &out_dir.to_string_lossy(),
    ]);
    let meta = entry_meta(
        "busy-key",
        vec![cached_file("libfoo.rlib", &fixture.hash)],
        &[],
    );
    let writer = rusqlite::Connection::open(fixture.config.cache_dir.join("index.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    restore_from_cache(
        &fixture.config,
        &RustcCompiler::new(),
        &fixture.store,
        &args,
        &meta,
        None,
    )
    .unwrap();
    writer.execute_batch("ROLLBACK").unwrap();

    assert_eq!(
        std::fs::read(fixture.restored()).unwrap(),
        vec![b'r'; RESTORED_BYTES]
    );
    let (_, bytes) = fixture.next_key_hash();
    assert_eq!(
        bytes, RESTORED_BYTES as u64,
        "the dropped row is not served"
    );
}
