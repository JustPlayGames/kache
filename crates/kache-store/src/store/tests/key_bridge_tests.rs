//! Key bridges (lane KACHE-W20 prototype): `adopt_legacy_entry` re-indexes an
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
            .unwrap();

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
