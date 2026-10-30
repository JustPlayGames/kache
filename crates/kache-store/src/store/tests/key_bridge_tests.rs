//! Key bridges (lanes KACHE-W20..W22): `adopt_legacy_entry` re-indexes an
//! earlier key version's entry only when everything it can check agrees.

use super::*;

/// `put_test_entry`'s entry, made to look stored by key version `version`.
fn legacy_entry(store: &Store, dir: &Path, key: &str, version: u32) -> EntryMeta {
    put_test_entry(store, dir, key, 4096);
    store
        .db
        .execute(
            "UPDATE entries SET key_schema = ?2 WHERE cache_key = ?1",
            params![key, version],
        )
        .unwrap();
    let mut meta = store.stored_meta(key).unwrap();
    meta.key_schema = version;
    std::fs::write(
        store.entry_dir(key).join("meta.json"),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .unwrap();
    meta
}

fn refcount(store: &Store, hash: &str) -> i64 {
    store
        .db
        .query_row("SELECT refcount FROM blobs WHERE hash = ?1", params![hash], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_legacy_entry_is_adopted_only_when_key_version_crate_and_bytes_agree() {
    let old = kache_format::CACHE_KEY_VERSION - 1;
    // (case, legacy version stored, crate asked for, tamper with the blob, adopted)
    let cases = [
        ("adopted", old, "legacy_crate", false, true),
        ("another version's entry", old - 1, "legacy_crate", false, false),
        ("another crate's entry", old, "other_crate", false, false),
        ("a blob that lost its bytes", old, "legacy_crate", true, false),
    ];
    for (case, stored_as, crate_name, tamper, adopted) in cases {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(test_config(dir.path())).unwrap();
        let meta = legacy_entry(&store, dir.path(), "legacy", stored_as);
        let hash = &meta.files[0].hash;
        if tamper {
            let blob = store.blob_path(hash);
            let mut perms = std::fs::metadata(&blob).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(&blob, perms).unwrap();
            std::fs::write(&blob, vec![b'x'; 4096]).unwrap();
        }
        let before = refcount(&store, hash);

        let got = store
            .adopt_legacy_entry("legacy", old, "current", crate_name)
            .unwrap()
            .is_some();

        assert_eq!(got, adopted, "{case}");
        assert_eq!(store.contains("current"), adopted, "{case}");
        assert!(store.contains("legacy"), "{case}: the legacy row stays");
        if adopted {
            let current = store.get("current").unwrap().unwrap();
            assert_eq!(current.cache_key, "current");
            assert_eq!(current.key_schema, kache_format::CACHE_KEY_VERSION);
            assert_eq!(current.files, meta.files, "same blobs, no copy");
            assert_eq!(refcount(&store, hash), before + 1, "{case}");
        } else {
            assert_eq!(refcount(&store, hash), before, "{case}");
        }
    }
}

/// Eviction ranks a row of an earlier key version as one no key of this
/// release reaches, until a hit says otherwise: also a hit inside
/// [`HIT_STAMP_INTERVAL`] (a row stamped a second ago, then hit by the new
/// release, kept ranking first). A newer release's row is not stale to an
/// older one (a rollback keeps its rows ranked by idleness).
#[test]
fn a_hit_on_an_earlier_key_versions_row_stops_it_ranking_stale_at_once() {
    let current = kache_format::CACHE_KEY_VERSION;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(test_config(dir.path())).unwrap();
    legacy_entry(&store, dir.path(), "earlier", current - 1);
    legacy_entry(&store, dir.path(), "newer", current + 1);
    // Components whose own key version a rustc bump left alone (KACHE-W22):
    // their rows are what this release's keys reach. One a version before
    // its own component's is stale like any other.
    for (key, crate_name, crate_type, version) in [
        ("cc", "foo.c", "", kache_format::CC_KEY_VERSION),
        ("cc_earlier", "bar.c", "", kache_format::CC_KEY_VERSION - 1),
        (
            "build_script",
            kache_format::BUILD_SCRIPT_RUN_CRATE_NAME,
            "build-script",
            kache_format::BUILD_SCRIPT_KEY_VERSION,
        ),
    ] {
        legacy_entry(&store, dir.path(), key, version);
        store
            .db
            .execute(
                "UPDATE entries SET crate_name = ?2, crate_type = ?3 WHERE cache_key = ?1",
                params![key, crate_name, crate_type],
            )
            .unwrap();
    }
    let idle = |key: &str| {
        store
            .eviction_candidates()
            .unwrap()
            .into_iter()
            .find(|entry| entry.key == key)
            .unwrap()
            .idle_hours
    };
    assert!(idle("earlier") >= STALE_KEY_SCHEMA_IDLE_HOURS, "not hit yet");
    assert!(idle("newer") < 1.0, "a newer version is not an earlier one");
    assert!(idle("cc") < 1.0, "a C compile at its own key version");
    assert!(
        idle("build_script") < 1.0,
        "a build-script run at its own key version"
    );
    assert!(idle("cc_earlier") >= STALE_KEY_SCHEMA_IDLE_HOURS);

    // Just stored, so well inside the stamp interval.
    store.get("earlier").unwrap().expect("a hit");

    let schema: u32 = store
        .db
        .query_row(
            "SELECT key_schema FROM entries WHERE cache_key = 'earlier'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema, current);
    assert!(idle("earlier") < 1.0);
}

/// The key's build lock comes first (KACHE-W22): a process that finds it
/// held leaves the key to its holder (whose commit its claim waits for)
/// without re-hashing anything; once the holder adopted it, the next one
/// finds the entry under its key.
#[test]
fn an_adoption_leaves_a_key_another_process_holds_to_it() {
    let old = kache_format::CACHE_KEY_VERSION - 1;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(test_config(dir.path())).unwrap();
    legacy_entry(&store, dir.path(), "legacy", old);

    let held = store.try_lock("current").unwrap().expect("free");
    let got = store
        .adopt_legacy_entry("legacy", old, "current", "legacy_crate")
        .unwrap()
        .expect("an outcome");
    assert_eq!(got.outcome, AdoptionOutcome::Contended);
    assert_eq!(got.bytes, 0, "nothing re-hashed");
    assert!(!store.contains("current"));
    drop(held);

    let first = store
        .adopt_legacy_entry("legacy", old, "current", "legacy_crate")
        .unwrap()
        .unwrap();
    assert_eq!(first.outcome, AdoptionOutcome::Adopted);
    let again = store
        .adopt_legacy_entry("legacy", old, "current", "legacy_crate")
        .unwrap()
        .unwrap();
    assert_eq!(again.outcome, AdoptionOutcome::ByPeer);
    assert_eq!(again.bytes, 0);
}

/// One transaction registers many adoptions, each on its own: one whose
/// legacy row went is refused without undoing the others, and a key already
/// committed is left as it is.
#[test]
fn a_batch_of_adoptions_registers_each_on_its_own() {
    let old = kache_format::CACHE_KEY_VERSION - 1;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(test_config(dir.path())).unwrap();
    let meta = legacy_entry(&store, dir.path(), "a", old);
    legacy_entry(&store, dir.path(), "b", old);
    put_test_entry(&store, dir.path(), "already", 4096);
    let before = refcount(&store, &meta.files[0].hash);
    let request = |legacy: &str, key: &str| AdoptionRequest {
        legacy_key: legacy.to_string(),
        legacy_version: old,
        cache_key: key.to_string(),
        crate_name: format!("{legacy}_crate"),
    };

    let results = store
        .register_adoptions(&[
            request("a", "new_a"),
            request("gone", "new_gone"),
            request("b", "already"),
        ])
        .unwrap();

    assert_eq!(
        results,
        [Registered::Adopted, Registered::Refused, Registered::ByPeer]
    );
    assert_eq!(store.get("new_a").unwrap().unwrap().files, meta.files);
    assert_eq!(refcount(&store, &meta.files[0].hash), before + 1);
    assert!(!store.contains("new_gone"));
    assert_eq!(
        store.get("already").unwrap().unwrap().crate_name,
        "already_crate"
    );
}

/// A registration handed to a registrar (KACHE-W23): the caller gets the
/// verified entry under the new key to restore from at once, nothing is
/// written yet, and the key's lock is the registrar's to take: the closure
/// had it and let it go.
#[test]
fn a_deferred_adoption_hands_back_the_verified_entry_unwritten() {
    let old = kache_format::CACHE_KEY_VERSION - 1;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(test_config(dir.path())).unwrap();
    let meta = legacy_entry(&store, dir.path(), "legacy", old);
    let before = refcount(&store, &meta.files[0].hash);

    let got = store
        .adopt_legacy_entry_with("legacy", old, "current", "legacy_crate", |request, lock| {
            assert_eq!(request.cache_key, "current");
            assert!(store.store_in_flight("current"), "held and marked storing");
            drop(lock);
            Ok(Registered::Deferred)
        })
        .unwrap()
        .expect("an outcome");

    assert_eq!(got.outcome, AdoptionOutcome::Deferred);
    let restored = got.meta.expect("the verified entry");
    assert_eq!(restored.cache_key, "current");
    assert_eq!(restored.key_schema, kache_format::CACHE_KEY_VERSION);
    assert_eq!(restored.files, meta.files);
    assert!(!store.contains("current"), "not written");
    assert_eq!(refcount(&store, &meta.files[0].hash), before);
    assert!(store.try_lock("current").unwrap().is_some());
}
