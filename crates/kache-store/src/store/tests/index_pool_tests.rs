use super::*;
use crate::index_pool::{burst_on_this_thread, pool_on_this_thread};

/// Mark the connection a store's next statement takes with its page cache
/// size, which belongs to the connection: a store that reads `tag` back took
/// that very one.
fn mark(store: &Store, tag: i64) {
    store.db().pragma_update(None, "cache_size", -tag).unwrap();
}

/// The tag on the connection a store's next statement takes; an unmarked
/// connection has SQLite's default, 2000.
fn tag(store: &Store) -> i64 {
    -store
        .db()
        .query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))
        .unwrap()
}

const UNMARKED: i64 = 2000;

#[test]
fn a_pooled_store_holds_no_connection_between_statements() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let first = Store::open(&config).unwrap();
    assert!(!first.db.holds(), "opening takes none for later");
    mark(&first, 1);
    assert!(!first.db.holds());

    let second = Store::open(&config).unwrap();
    assert_eq!(
        tag(&second),
        1,
        "the next statement took the one given back"
    );
    assert!(
        !first.contains(&"0".repeat(64)),
        "and the first store still reads"
    );
}

#[test]
fn a_connection_stays_with_its_store_while_a_statement_uses_it() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let store = Store::open(&config).unwrap();
    let outer = store.db();
    mark(&store, 2);
    assert!(store.db.holds(), "the outer statement still uses it");
    assert_eq!(tag(&store), 2);
    drop(outer);
    assert!(!store.db.holds());
}

#[test]
fn a_connection_inside_a_transaction_stays_with_its_store() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let store = Store::open(&config).unwrap();
    store.db().execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.db.holds());
    mark(&store, 3);
    store.db().execute_batch("COMMIT").unwrap();
    assert!(!store.db.holds(), "the commit gave it back");
    assert_eq!(tag(&Store::open(&config).unwrap()), 3);
}

#[test]
fn a_connection_waiting_with_another_timeout_stays_until_it_is_given_back() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let store = Store::open(&config).unwrap();
    store.db().busy_timeout(Duration::ZERO).unwrap();
    assert!(store.db.holds());
    store.give_back_busy_timeout();
    assert!(!store.db.holds());
}

#[test]
fn connections_given_back_past_the_idle_cap_are_closed() {
    pool_on_this_thread(1);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let (a, b) = (Store::open(&config).unwrap(), Store::open(&config).unwrap());
    let (held_a, held_b) = (a.db(), b.db());
    held_a.pragma_update(None, "cache_size", -4).unwrap();
    held_b.pragma_update(None, "cache_size", -5).unwrap();
    drop(held_a);
    drop(held_b);
    let (held_c, held_d) = (a.db(), b.db());
    let size = |db: &Connection| db.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0));
    assert_eq!(size(&held_c).unwrap(), -4, "the one idle connection");
    assert_eq!(size(&held_d).unwrap(), -UNMARKED, "the second was closed");
}

#[test]
fn a_burst_keeps_the_connections_it_opened_up_to_its_ceiling() {
    pool_on_this_thread(1);
    burst_on_this_thread(2);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let (a, b, c) = (
        Store::open(&config).unwrap(),
        Store::open(&config).unwrap(),
        Store::open(&config).unwrap(),
    );
    // Three statements at once: two connections are opened for want of
    // an idle one, and three are given back.
    let held = [a.db(), b.db(), c.db()];
    for (tag, db) in (4..).zip(&held) {
        db.pragma_update(None, "cache_size", -tag).unwrap();
    }
    drop(held);
    let again = [a.db(), b.db(), c.db()];
    let mut tags: Vec<i64> = again
        .iter()
        .map(|db| {
            -db.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0))
                .unwrap()
        })
        .collect();
    tags.sort_unstable();
    assert_eq!(
        tags,
        [4, 5, UNMARKED],
        "two kept (the ceiling), the third closed and opened afresh"
    );
}

#[test]
#[cfg(unix)]
fn a_replaced_index_never_gets_a_connection_to_the_old_file() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let old = Store::open(&config).unwrap();
    mark(&old, 6);
    let index = config.index_db_path();
    for suffix in ["", "-wal", "-shm"] {
        let mut side = index.clone().into_os_string();
        side.push(suffix);
        let _ = fs::rename(&side, dir.path().join(format!("aside{suffix}")));
    }
    // Another process recreates it.
    drop(open_index_db(&index).unwrap());

    let fresh = Store::open(&config).unwrap();
    assert_eq!(tag(&fresh), UNMARKED, "the store opened the new index");
    assert_eq!(tag(&old), UNMARKED, "and so did the old one");
}

#[test]
fn without_pooling_a_store_keeps_its_own_connection() {
    pool_on_this_thread(0);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let store = Store::open(&config).unwrap();
    mark(&store, 7);
    assert!(store.db.holds());
    drop(store);
    assert_eq!(tag(&Store::open(&config).unwrap()), UNMARKED);
}

#[test]
fn read_only_and_read_write_stores_never_share_a_connection() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let writer = Store::open(&config).unwrap();
    mark(&writer, 8);
    let reader = Store::open_read_only(&config).unwrap();
    assert_eq!(tag(&reader), UNMARKED);
    mark(&reader, 9);
    assert_eq!(tag(&writer), 8);
    assert_eq!(tag(&reader), 9);
}

#[test]
fn a_file_hash_cache_lends_the_connection_per_statement() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let store = Store::open(&config).unwrap();
    let memo = store.file_hash_cache();
    {
        let statement = memo.db();
        statement.pragma_update(None, "cache_size", -10).unwrap();
        assert!(store.db.holds(), "a statement still uses it");
    }
    assert!(!store.db.holds(), "between statements the cache lets it go");
    assert_eq!(tag(&Store::open(&config).unwrap()), 10);
}

#[test]
fn a_store_dropped_inside_a_transaction_closes_its_connection() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let store = Store::open(&config).unwrap();
    store.db().execute_batch("BEGIN IMMEDIATE").unwrap();
    mark(&store, 11);
    drop(store);
    let next = Store::open(&config).unwrap();
    assert_eq!(tag(&next), UNMARKED, "no one inherits the open transaction");
    next.db().execute_batch("BEGIN IMMEDIATE; COMMIT").unwrap();
}

#[test]
fn closing_the_idle_connections_leaves_the_ones_in_use() {
    pool_on_this_thread(4);
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());
    let (idle, busy) = (Store::open(&config).unwrap(), Store::open(&config).unwrap());
    let held = busy.db();
    held.pragma_update(None, "cache_size", -13).unwrap();
    mark(&idle, 12);
    let ours = |key: &crate::index_pool::PoolKey| key.path.starts_with(dir.path());
    assert_eq!(
        crate::index_pool::close_idle_where(ours),
        1,
        "the one given back"
    );
    assert_eq!(tag(&idle), UNMARKED, "the next statement opened a new one");
    assert_eq!(
        held.query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        -13
    );
}
