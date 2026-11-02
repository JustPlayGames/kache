use super::*;

/// A lookup that finds an adoptable key held by another process waits for
/// it, and adopts the entry itself when the holder lets go uncommitted, as
/// a wrapper does between its verification and the daemon's claim of the
/// registration (lane KACHE-W23). Before, the lookup missed at once and its
/// miss path, finding the key free and uncommitted, compiled the unit.
#[test]
fn a_held_adoptable_key_is_waited_for_and_adopted_once_let_go() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    let store = crate::store::Store::open(&config).unwrap();
    let old = kache_format::CACHE_KEY_VERSION - 1;
    let legacy = format!("{:064x}", 0xb1);
    let key = format!("{:064x}", 0xb2);
    let source = dir.path().join("lib.rlib");
    std::fs::write(&source, vec![7u8; 4096]).unwrap();
    store
        .put(
            &legacy,
            "testcrate",
            &["lib".into()],
            &[],
            "host",
            "dev",
            &[(source, "libtestcrate.rlib".into())],
            "",
            "",
        )
        .unwrap();
    store.set_key_schema_for_test(&legacy, old);
    remember(&key, old, &legacy, "testcrate");

    let held = store.try_lock(&key).unwrap().expect("free");
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
    });
    let started = std::time::Instant::now();
    let meta = adopt_on_miss(&store, &key).expect("adopted once the holder let go");
    holder.join().unwrap();

    assert!(started.elapsed() >= std::time::Duration::from_millis(250));
    assert_eq!(meta.cache_key, key);
    assert_eq!(meta.files, store.get(&legacy).unwrap().unwrap().files);
}
