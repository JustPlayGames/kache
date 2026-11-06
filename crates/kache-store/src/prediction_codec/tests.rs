use super::*;
use crate::file_hash::FileHashCache;
use rusqlite::params;

/// A closure as a unit's row holds it: long enough that zstd pays.
fn closure_json(files: usize) -> String {
    let sources: Vec<String> = (0..files)
        .map(|i| format!("\"/home/dev/.cargo/registry/src/index/serde-1.0.{i}/src/de/mod_{i}.rs\""))
        .collect();
    format!(
        "{{\"schema\":2,\"sources\":[{}],\"env_deps\":[[\"CARGO_PKG_NAME\",\"serde\"]]}}",
        sources.join(",")
    )
}

fn open_cache(dir: &tempfile::TempDir) -> FileHashCache<'static> {
    FileHashCache::open(&dir.path().join("index.db")).unwrap()
}

/// How the row is kept: its stored schema, SQLite's type of the payload and
/// the payload's length in bytes.
fn stored(cache: &FileHashCache<'_>, identity: &str) -> (u32, String, i64) {
    cache
        .db()
        .query_row(
            "SELECT schema, typeof(prediction_json), length(CAST(prediction_json AS BLOB))
             FROM input_predictions WHERE identity = ?1",
            params![identity],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

#[test]
fn a_compressed_row_reads_back_as_the_writers_schema_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(&dir);
    let json = closure_json(40);
    for (identity, schema) in [("unit", 2), ("portable", 102)] {
        cache
            .put_input_prediction_as(Codec::Zstd, identity, schema, Some("serde"), &json)
            .unwrap();
        let (stored_schema, kind, length) = stored(&cache, identity);
        assert_eq!(
            stored_schema,
            schema + 65_536,
            "the codec is tagged on the stored schema"
        );
        assert_eq!(kind, "blob");
        assert!(
            length * 3 < json.len() as i64,
            "{length} bytes for {} of JSON",
            json.len()
        );
        assert_eq!(
            cache.get_input_prediction(identity).unwrap(),
            Some((schema, json.clone()))
        );
    }
}

#[test]
fn a_plain_row_is_kept_and_read_as_every_release_writes_it() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(&dir);
    let json = closure_json(40);
    cache
        .put_input_prediction_as(Codec::Plain, "unit", 2, None, &json)
        .unwrap();
    assert_eq!(
        stored(&cache, "unit"),
        (2, "text".into(), json.len() as i64)
    );
    assert_eq!(cache.get_input_prediction("unit").unwrap(), Some((2, json)));
}

/// The rollback case: a release that only writes plain rows replaces a
/// compressed one, and a compressing release replaces that again.
#[test]
fn either_form_replaces_the_other_under_one_name() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(&dir);
    let (old, new) = (closure_json(30), closure_json(31));
    cache
        .put_input_prediction_as(Codec::Zstd, "unit", 2, None, &old)
        .unwrap();
    cache
        .put_input_prediction_as(Codec::Plain, "unit", 2, None, &new)
        .unwrap();
    assert_eq!(stored(&cache, "unit").1, "text");
    assert_eq!(cache.get_input_prediction("unit").unwrap(), Some((2, new)));
    cache
        .put_input_prediction_as(Codec::Zstd, "unit", 2, None, &old)
        .unwrap();
    assert_eq!(stored(&cache, "unit").1, "blob");
    assert_eq!(cache.get_input_prediction("unit").unwrap(), Some((2, old)));
}

/// What a release from before compression runs on a row: its read of the
/// payload as text fails, which its callers take as a miss (or, for a build
/// script, an error, which is why those rows are never compressed).
#[test]
fn a_release_before_compression_cannot_take_a_compressed_row_for_a_plain_one() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(&dir);
    cache
        .put_input_prediction_as(Codec::Zstd, "unit", 2, None, &closure_json(40))
        .unwrap();
    let (schema, text): (u32, rusqlite::Result<String>) = cache
        .db()
        .query_row(
            "SELECT schema, prediction_json FROM input_predictions WHERE identity = 'unit'",
            [],
            |row| Ok((row.get(0)?, row.get(1))),
        )
        .unwrap();
    assert_ne!(schema, 2);
    assert_ne!(schema, 102);
    assert!(text.is_err(), "an older release would decode {text:?}");
}

#[test]
fn only_a_compressing_cache_compresses_and_never_a_build_scripts_row() {
    assert_eq!(
        Codec::for_write(true, "kache-input-prediction:abc"),
        Codec::Zstd
    );
    assert_eq!(
        Codec::for_write(false, "kache-input-prediction:abc"),
        Codec::Plain
    );
    assert_eq!(Codec::for_write(true, "build-script:abc"), Codec::Plain);
    assert_eq!(Codec::for_write(true, "unsettled:abc"), Codec::Plain);

    let dir = tempfile::tempdir().unwrap();
    let owned = open_cache(&dir);
    let json = closure_json(40);
    let compressing = FileHashCache::Compressing(owned.db());
    compressing
        .put_input_prediction("unit", 2, None, &json)
        .unwrap();
    compressing
        .put_input_prediction("build-script:unit", 2, None, &json)
        .unwrap();
    compressing
        .put_input_prediction_trimming("tree:", "tree:a", 102, None, &json, 2)
        .unwrap();
    FileHashCache::Borrowed(owned.db())
        .put_input_prediction("plain", 2, None, &json)
        .unwrap();
    assert_eq!(stored(&owned, "unit").1, "blob");
    assert_eq!(stored(&owned, "tree:a").1, "blob");
    assert_eq!(stored(&owned, "build-script:unit").1, "text");
    assert_eq!(stored(&owned, "plain").1, "text");
    for identity in ["unit", "build-script:unit", "plain"] {
        assert_eq!(
            owned.get_input_prediction(identity).unwrap(),
            Some((2, json.clone()))
        );
    }
    assert_eq!(
        owned.get_input_prediction("tree:a").unwrap(),
        Some((102, json))
    );
}

#[test]
fn a_frame_no_smaller_than_the_json_and_a_tagged_schema_stay_plain() {
    let (schema, payload) = encode(Codec::Zstd, 2, "{}");
    assert_eq!(schema, 2);
    assert_eq!(payload, ToSqlOutput::Borrowed(ValueRef::Text(b"{}")));
    let json = closure_json(40);
    let (schema, payload) = encode(Codec::Zstd, 2 | ZSTD_SCHEMA_TAG, &json);
    assert_eq!(schema, 2 | ZSTD_SCHEMA_TAG);
    assert_eq!(
        payload,
        ToSqlOutput::Borrowed(ValueRef::Text(json.as_bytes()))
    );
}

#[test]
fn a_row_this_release_cannot_read_is_absent_not_an_error() {
    let json = closure_json(40);
    let frame = zstd::bulk::compress(json.as_bytes(), ZSTD_LEVEL).unwrap();
    let tagged = 2 | ZSTD_SCHEMA_TAG;
    assert_eq!(
        decode(tagged, ValueRef::Blob(&frame)),
        Some((2, json.clone()))
    );
    // The tag and the type disagree.
    assert_eq!(decode(2, ValueRef::Blob(&frame)), None);
    assert_eq!(decode(tagged, ValueRef::Text(json.as_bytes())), None);
    // A damaged frame, one cut short, and one that does not say its size.
    let mut damaged = frame.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0xff;
    assert_eq!(decode(tagged, ValueRef::Blob(&damaged)), None);
    assert_eq!(
        decode(tagged, ValueRef::Blob(&frame[..frame.len() - 4])),
        None
    );
    let mut unsized_frame = Vec::new();
    {
        let mut encoder = zstd::stream::Encoder::new(&mut unsized_frame, ZSTD_LEVEL).unwrap();
        encoder.include_contentsize(false).unwrap();
        std::io::Write::write_all(&mut encoder, json.as_bytes()).unwrap();
        encoder.finish().unwrap();
    }
    assert_eq!(decode(tagged, ValueRef::Blob(&unsized_frame)), None);
    // Bytes that are not UTF-8, in either form.
    let latin1 = zstd::bulk::compress(&[0xe9; 64], ZSTD_LEVEL).unwrap();
    assert_eq!(decode(tagged, ValueRef::Blob(&latin1)), None);
    assert_eq!(decode(2, ValueRef::Text(&[0xe9])), None);
    // Neither form at all.
    assert_eq!(decode(2, ValueRef::Null), None);
    assert_eq!(decode(tagged, ValueRef::Integer(7)), None);
}

#[test]
fn a_frame_that_claims_more_than_the_bound_is_not_decoded() {
    // 64 MiB and one byte of zeros: a frame of a few KB.
    let big = vec![b'0'; (64 << 20) + 1];
    let frame = zstd::bulk::compress(&big, 1).unwrap();
    assert_eq!(decode(2 | ZSTD_SCHEMA_TAG, ValueRef::Blob(&frame)), None);
    let at_bound = zstd::bulk::compress(&big[1..], 1).unwrap();
    assert_eq!(
        decode(2 | ZSTD_SCHEMA_TAG, ValueRef::Blob(&at_bound)).map(|(_, json)| json.len()),
        Some(64 << 20)
    );
}

#[test]
fn an_unreadable_stored_row_reads_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_cache(&dir);
    cache
        .db()
        .execute(
            "INSERT INTO input_predictions (identity, schema, prediction_json) VALUES ('unit', ?1, x'00')",
            params![2 | ZSTD_SCHEMA_TAG],
        )
        .unwrap();
    assert_eq!(cache.get_input_prediction("unit").unwrap(), None);
}
