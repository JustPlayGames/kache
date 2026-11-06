//! How an `input_predictions` row keeps its payload: the JSON as TEXT, as
//! every release has written it, or a zstd frame of that JSON as a BLOB in
//! the same column.
//!
//! A compressed row's stored `schema` carries [`ZSTD_SCHEMA_TAG`] on top of
//! the schema its writer gave. No release before this one knows such a
//! number, and each of them checks the schema before decoding: a release
//! from before compression reads the row as absent (its `String` read of the
//! BLOB fails, or the schema does not match) and runs the dep-info pre-pass,
//! then writes the row back as plain JSON. Readers here get the writer's
//! schema and the JSON back, so nothing above the store sees the codec, and
//! no cache key or prediction identity changes with it.
//!
//! A build script's rows ([`crate::file_hash::BUILD_SCRIPT_ROW_PREFIXES`])
//! are never compressed: an older release's build-script reader returns the
//! failed read as an error, not a miss, and the rows are small.
//!
//! Writing compressed rows is off unless the store was opened with
//! `compress_predictions` (`[cache] input_prediction_compression`), which
//! hands out a [`crate::file_hash::FileHashCache::Compressing`]; reading both
//! forms is always on,
//! so a release that writes them can be rolled back to one that only reads
//! them without a single extra pre-pass.

use rusqlite::types::{ToSqlOutput, Value, ValueRef};
use std::cell::RefCell;
use zstd::bulk::{Compressor, Decompressor};

/// Set on the stored schema of a row whose payload is a zstd frame. Far above
/// every prediction schema (2, 102 and the older 1, 101), as
/// [`crate::file_hash::FILE_HASH_RULE_REDATED`] marks a rule.
pub const ZSTD_SCHEMA_TAG: u32 = 1 << 16;

/// The zstd level rows are written at: 7x on the build machine's 12 GB of
/// predictions, about 11 us a row.
pub const ZSTD_LEVEL: i32 = 3;

/// The most a compressed row may expand to. The largest rows (a crate that
/// embeds thousands of files) hold about 300 KB of JSON; a frame that claims
/// more, or does not say, is not decoded.
pub const MAX_DECODED_BYTES: u64 = 64 << 20;

thread_local! {
    // A context per thread, not per row: setting one up costs more than
    // compressing a typical 4 KB row.
    static COMPRESSOR: RefCell<Option<Compressor<'static>>> = const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<Decompressor<'static>>> = const { RefCell::new(None) };
}

fn compress(json: &str) -> Option<Vec<u8>> {
    COMPRESSOR.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = Some(Compressor::new(ZSTD_LEVEL).ok()?);
        }
        cell.as_mut()?.compress(json.as_bytes()).ok()
    })
}

fn decompress(frame: &[u8], size: usize) -> Option<Vec<u8>> {
    DECOMPRESSOR.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = Some(Decompressor::new().ok()?);
        }
        cell.as_mut()?.decompress(frame, size).ok()
    })
}

/// How a row is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// The JSON as TEXT, readable by every release.
    Plain,
    /// A zstd frame as a BLOB under a tagged schema, readable from the
    /// release that introduced [`ZSTD_SCHEMA_TAG`] on.
    Zstd,
}

impl Codec {
    /// The codec a row named `identity` is written with, when new rows are
    /// compressed (`compress`) or not.
    pub fn for_write(compress: bool, identity: &str) -> Self {
        if compress && !is_build_script_row(identity) {
            Self::Zstd
        } else {
            Self::Plain
        }
    }
}

fn is_build_script_row(identity: &str) -> bool {
    crate::file_hash::BUILD_SCRIPT_ROW_PREFIXES
        .iter()
        .any(|prefix| identity.starts_with(prefix))
}

/// The stored schema and payload of a row written with `codec`. A frame no
/// smaller than the JSON, and a schema that already carries the tag, are
/// written plain.
pub fn encode(codec: Codec, schema: u32, json: &str) -> (u32, ToSqlOutput<'_>) {
    if codec == Codec::Zstd
        && schema & ZSTD_SCHEMA_TAG == 0
        && let Some(frame) = compress(json)
        && frame.len() < json.len()
    {
        return (
            schema | ZSTD_SCHEMA_TAG,
            ToSqlOutput::Owned(Value::Blob(frame)),
        );
    }
    (
        schema,
        ToSqlOutput::Borrowed(ValueRef::Text(json.as_bytes())),
    )
}

/// The writer's schema and the JSON of a stored row, or `None` for a row
/// this release cannot read: a BLOB without the tag, a tagged TEXT, a frame
/// that does not decode or claims more than [`MAX_DECODED_BYTES`], or bytes
/// that are not UTF-8. The caller treats `None` as an absent row.
pub fn decode(schema: u32, payload: ValueRef<'_>) -> Option<(u32, String)> {
    let tagged = schema & ZSTD_SCHEMA_TAG != 0;
    match payload {
        ValueRef::Text(text) if !tagged => String::from_utf8(text.to_vec())
            .ok()
            .map(|json| (schema, json)),
        ValueRef::Blob(frame) if tagged => {
            let size = zstd::zstd_safe::get_frame_content_size(frame).ok()??;
            if size > MAX_DECODED_BYTES {
                return None;
            }
            let bytes = decompress(frame, usize::try_from(size).ok()?)?;
            if bytes.len() as u64 != size {
                return None;
            }
            String::from_utf8(bytes)
                .ok()
                .map(|json| (schema & !ZSTD_SCHEMA_TAG, json))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
