//! The file-hash memo rows a unit's key learns, and the unit's other
//! bookkeeping writes, when another process holds the index's write lock.

use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Above the memo's persistence floor.
const INPUT_BYTES: usize = 64 * 1024;

/// A store that hands memo rows a busy index refuses to a sink, as the
/// wrapper's store hands them to the daemon, and one input file.
struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    store: Store,
    input: PathBuf,
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
        let input = dir.path().join("input.rlib");
        std::fs::write(&input, vec![b'a'; INPUT_BYTES]).unwrap();
        Self {
            _dir: dir,
            config,
            store,
            input,
            handed,
        }
    }

    /// Another connection holding the index's write lock, as a miss storing
    /// its outputs or a stalled lock holder does, until it is dropped.
    fn hold_the_write_lock(&self) -> rusqlite::Connection {
        let writer = rusqlite::Connection::open(self.config.index_db_path()).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
    }

    /// Write what the store handed over, as the daemon does once the index
    /// is free; returns how many rows were written.
    fn daemon_writes_handed_rows(&self) -> usize {
        let rows = std::mem::take(&mut *self.handed.lock().unwrap());
        self.store.write_handed_memo_rows(&rows).unwrap()
    }

    /// Hash the input as the next unit's key would, through a fresh hasher:
    /// its digest and the bytes read for it.
    fn next_key_hash(&self) -> (String, u64) {
        let hasher = self.store.file_hasher();
        let hash = hasher.hash(&self.input).unwrap();
        (hash, hasher.stats().bytes_hashed)
    }
}

/// A key's memo rows waited 100 ms for a busy index before the compile and
/// were then dropped, so the build's next unit read the file again. They now
/// go to the daemon after a short wait, and land once the index lets them.
#[test]
fn a_busy_index_hands_a_keys_memo_rows_over_instead_of_waiting() {
    let fixture = Fixture::new();
    let hasher = fixture.store.file_hasher();
    let hash = hasher.hash(&fixture.input).unwrap();

    let writer = fixture.hold_the_write_lock();
    let started = Instant::now();
    hasher.flush_memo_as_if_settled();
    let waited = started.elapsed();
    drop(writer);
    assert!(
        waited < Duration::from_millis(50),
        "the unit waited {waited:?} for its memo rows"
    );
    {
        let handed = fixture.handed.lock().unwrap();
        assert_eq!(handed.len(), 1, "the busy index's row goes to the sink");
        assert_eq!(
            handed[0].fingerprint,
            crate::cache_key::FileFingerprint::from_path(&fixture.input).unwrap(),
            "the row carries the stamp the bytes were hashed at"
        );
        assert_eq!(handed[0].hash, hash);
        assert!(!handed[0].redated);
    }

    assert_eq!(fixture.daemon_writes_handed_rows(), 1);
    assert_eq!(
        fixture.next_key_hash(),
        (hash, 0),
        "the next key takes the input's hash from the memo"
    );
}

/// A handed row that reaches the index after its file changed is not
/// written: the next key reads the new bytes, never the old hash.
#[test]
fn a_late_memo_row_for_a_file_changed_since_is_not_written() {
    let fixture = Fixture::new();
    let hasher = fixture.store.file_hasher();
    let old = hasher.hash(&fixture.input).unwrap();
    let writer = fixture.hold_the_write_lock();
    hasher.flush_memo_as_if_settled();
    drop(writer);
    assert_eq!(fixture.handed.lock().unwrap().len(), 1);

    // Same size, new bytes, and the stamp's times put back: only the change
    // time and the content tell the two apart.
    let before = std::fs::metadata(&fixture.input).unwrap();
    std::fs::write(&fixture.input, vec![b'b'; INPUT_BYTES]).unwrap();
    filetime::set_file_mtime(
        &fixture.input,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .unwrap();

    assert_eq!(fixture.daemon_writes_handed_rows(), 0);
    let (hash, bytes) = fixture.next_key_hash();
    assert_ne!(hash, old);
    assert_eq!(hash, crate::cache_key::hash_file(&fixture.input).unwrap());
    assert_eq!(bytes, INPUT_BYTES as u64, "the changed file is read again");
}

/// Without anywhere to hand them, a busy index still drops the rows after
/// the old wait: the next key reads the file again, nothing else.
#[test]
fn without_a_daemon_a_busy_index_drops_a_keys_memo_rows() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path().join("cache"));
    let store = Store::open(&config).unwrap();
    let input = dir.path().join("input.rlib");
    std::fs::write(&input, vec![b'a'; INPUT_BYTES]).unwrap();
    let hasher = store.file_hasher();
    let hash = hasher.hash(&input).unwrap();

    let writer = rusqlite::Connection::open(config.index_db_path()).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    hasher.flush_memo_as_if_settled();
    drop(writer);

    let next = store.file_hasher();
    assert_eq!(next.hash(&input).unwrap(), hash);
    assert_eq!(next.stats().bytes_hashed, INPUT_BYTES as u64);
}

/// A unit's bookkeeping writes on a busy index (its key's memo rows before
/// the compile, its target root and prediction rows after it) cost one short
/// wait together, not one each: once the daemon took rows, the later writes
/// only try the lock. Before, they waited 5 + 25 + 25 ms (100 + 25 + 25 ms
/// before the memo rows had somewhere to go). The rows still all reach the
/// daemon.
#[test]
fn a_units_bookkeeping_on_a_busy_index_pays_one_short_wait() {
    let fixture = Fixture::new();
    let scratch = fixture.input.parent().unwrap();
    let workspace = scratch.join("ws");
    let target = cargo_target_in(&workspace);
    let source = scratch.join("lib.rs");
    std::fs::write(&source, "pub fn f() {}\n").unwrap();
    let closure = crate::cache_key::DepInfo {
        source_files: vec![source],
        env_deps: Vec::new(),
    };

    let writer = fixture.hold_the_write_lock();
    let started = Instant::now();
    {
        let hasher = fixture.store.file_hasher();
        hasher.hash(&fixture.input).unwrap();
        hasher.flush_memo_as_if_settled();
    }
    register_target_root(
        &fixture.config,
        &fixture.store,
        &target,
        &workspace,
        Some("rustc 1"),
    );
    let predictions = fixture.store.file_hasher().with_input_predictions(true);
    predictions.record_input_prediction("unit-a", Some("a"), &closure, None);
    let waited = started.elapsed();
    drop(writer);
    assert!(
        waited < Duration::from_millis(40),
        "the unit waited {waited:?} for its bookkeeping"
    );
    assert_eq!(fixture.handed.lock().unwrap().len(), 1);
    assert_eq!(
        predictions
            .take_busy_predictions()
            .iter()
            .map(|row| row.identity.as_str())
            .collect::<Vec<_>>(),
        ["unit-a"],
        "the prediction row is kept for the daemon"
    );
}
