use super::imp::Queue;
use super::{hold_index_place, set_index_busy_wait};
use rusqlite::{Connection, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn index(dir: &Path) -> PathBuf {
    let path = dir.join("index.db");
    let db = crate::open_index_db(&path).unwrap();
    db.execute_batch("CREATE TABLE IF NOT EXISTS t (who INTEGER)")
        .unwrap();
    path
}

/// A connection holding the index's write lock until dropped.
fn holder(path: &Path) -> Connection {
    let db = crate::open_index_db(path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    db
}

/// Wait until `n` tickets were taken from the index's turns.
fn tickets_taken(path: &Path, n: u64) {
    let queue = Queue::of(path).expect("the turns are on in tests");
    let deadline = Instant::now() + Duration::from_secs(10);
    while queue.counters().0 < n {
        assert!(Instant::now() < deadline, "a waiter never took its ticket");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A writer on a thread of its own: waits `wait` for the lock, writes its
/// id, commits; `got` lists who got the lock, in order.
fn writer(
    path: &Path,
    id: i64,
    wait: Duration,
    got: &Arc<Mutex<Vec<i64>>>,
) -> std::thread::JoinHandle<bool> {
    let (path, got) = (path.to_path_buf(), Arc::clone(got));
    std::thread::spawn(move || {
        let db = crate::open_index_db(&path).unwrap();
        set_index_busy_wait(&db, wait).unwrap();
        let Ok(tx) = rusqlite::Transaction::new_unchecked(&db, TransactionBehavior::Immediate)
        else {
            return false;
        };
        got.lock().unwrap().push(id);
        tx.execute("INSERT INTO t (who) VALUES (?1)", [id]).unwrap();
        tx.commit().unwrap();
        true
    })
}

/// Writers that found the lock held get it in the order they asked, once
/// it frees: SQLite's own busy handler hands it to whoever polls first,
/// which favours the latest arrivals (they poll every few ms, the oldest
/// every 100 ms).
#[test]
fn waiters_get_the_lock_in_the_order_they_asked() {
    let dir = tempfile::tempdir().unwrap();
    let path = index(dir.path());
    let held = holder(&path);
    let got = Arc::new(Mutex::new(Vec::new()));
    let mut writers = Vec::new();
    for id in 0..12 {
        writers.push(writer(&path, id, Duration::from_secs(20), &got));
        tickets_taken(&path, id as u64 + 1);
        // The early ones settle into SQLite's long sleeps.
        std::thread::sleep(Duration::from_millis(if id < 4 { 120 } else { 5 }));
    }
    drop(held);
    for writer in writers {
        assert!(
            writer.join().unwrap(),
            "every writer gets the lock in its wait"
        );
    }
    assert_eq!(*got.lock().unwrap(), (0..12).collect::<Vec<_>>());
    let (next, serving) = Queue::of(&path).unwrap().counters();
    assert_eq!(next, serving, "a line that emptied serves no one");
}

/// A waiter that gives up hands its turn on at once (the one behind it is
/// served as it leaves), not after the stale-head wait: the quitter asked
/// every millisecond until it left, so only its leaving moved the line.
#[test]
fn a_waiter_that_gives_up_hands_its_turn_on_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = index(dir.path());
    let held = holder(&path);
    let got = Arc::new(Mutex::new(Vec::new()));
    let quitter = writer(&path, 1, Duration::from_millis(150), &got);
    tickets_taken(&path, 1);
    let patient = writer(&path, 2, Duration::from_secs(20), &got);
    tickets_taken(&path, 2);
    assert!(
        !quitter.join().unwrap(),
        "the short wait gives up while the lock is held"
    );
    assert_eq!(
        Queue::of(&path).unwrap().counters(),
        (2, 1),
        "the quitter's leaving serves the patient writer's ticket"
    );
    drop(held);
    assert!(patient.join().unwrap());
    assert_eq!(*got.lock().unwrap(), vec![2]);
}

/// A writer that asks in short tries (the daemon's batch) keeps its place
/// between them while it holds one, instead of going to the end of the
/// line at every try.
#[test]
fn a_held_place_survives_short_tries() {
    let dir = tempfile::tempdir().unwrap();
    let path = index(dir.path());
    let held = holder(&path);
    let got = Arc::new(Mutex::new(Vec::new()));
    let batch = {
        let (path, got) = (path.clone(), Arc::clone(&got));
        std::thread::spawn(move || {
            let db = crate::open_index_db(&path).unwrap();
            let _place = hold_index_place(&db);
            let started = Instant::now();
            loop {
                set_index_busy_wait(&db, Duration::from_millis(20)).unwrap();
                match rusqlite::Transaction::new_unchecked(&db, TransactionBehavior::Immediate) {
                    Ok(tx) => {
                        got.lock().unwrap().push(0);
                        tx.commit().unwrap();
                        return;
                    }
                    Err(_) => assert!(started.elapsed() < Duration::from_secs(20)),
                }
            }
        })
    };
    tickets_taken(&path, 1);
    std::thread::sleep(Duration::from_millis(60));
    let other = writer(&path, 1, Duration::from_secs(20), &got);
    tickets_taken(&path, 2);
    std::thread::sleep(Duration::from_millis(60));
    drop(held);
    batch.join().unwrap();
    assert!(other.join().unwrap());
    assert_eq!(*got.lock().unwrap(), vec![0, 1], "the batch asked first");
    let (next, serving) = Queue::of(&path).unwrap().counters();
    assert_eq!(next, 2, "the batch kept one ticket through its tries");
    assert_eq!(serving, 2);
}

/// A turn whose writer went silent (gone, or holding a ticket it never
/// gives back) is passed over after a short while, not waited on forever.
#[test]
fn a_silent_head_is_passed_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = index(dir.path());
    let held = holder(&path);
    // A connection that waits once, gives up keeping its place, and never
    // asks again nor lets its place go.
    let silent = crate::open_index_db(&path).unwrap();
    let place = hold_index_place(&silent);
    set_index_busy_wait(&silent, Duration::from_millis(10)).unwrap();
    assert!(silent.execute_batch("BEGIN IMMEDIATE").is_err());
    let got = Arc::new(Mutex::new(Vec::new()));
    let patient = writer(&path, 1, Duration::from_secs(20), &got);
    tickets_taken(&path, 2);
    let freed = Instant::now();
    drop(held);
    assert!(
        patient.join().unwrap(),
        "the writer behind a silent head still gets the lock"
    );
    assert!(freed.elapsed() < Duration::from_secs(2));
    drop(place);
}

/// A turns file of another layout turns the turns off for that index; its
/// connections still open, write and wait as SQLite's busy timeout does.
#[test]
fn an_unknown_turns_file_leaves_sqlite_waiting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.db-turns"), [0xEEu8; 64]).unwrap();
    let path = index(dir.path());
    assert!(Queue::of(&path).is_none());
    let db = crate::open_index_db(&path).unwrap();
    set_index_busy_wait(&db, Duration::from_millis(300)).unwrap();
    assert_eq!(super::busy_wait(&db), Some(Duration::from_millis(300)));
    let held = holder(&path);
    let asked = Instant::now();
    assert!(db.execute_batch("BEGIN IMMEDIATE").is_err());
    assert!(
        asked.elapsed() >= Duration::from_millis(250),
        "it still waited"
    );
    drop(held);
    db.execute("INSERT INTO t (who) VALUES (1)", []).unwrap();
}
