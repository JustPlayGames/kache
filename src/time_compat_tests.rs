//! Times an installed chrono-era kache wrote, read by this tree and written
//! again: the same bytes, so a daemon of either version reads the other's
//! files the same way on a shared store.
//!
//! The tests run only with `KACHE_COMPAT_COPIES` naming a folder of copies
//! of files that kache wrote (never the live files: the tests copy what they
//! open into a temporary folder, as readers take locks beside a log):
//!
//! - `run/events.jsonl`, `run/events.jsonl.archive/*.jsonl.zst`: the event
//!   log and the segments a rotation archived (`ts`, serde's `Z` text)
//! - `store/gc_stats.json` (`last_run`, `+00:00` text), `store/savings.json`
//!   (`since`, serde's `Z` text)
//! - `store/index-sample.db`: rows of the index's `entries` and
//!   `eviction_tombstones` (SQLite's `datetime('now')` text)
//! - `log/seed-notes.log`: the seeding notes kache appends to its log
//!
//! `KACHE_COMPAT_SINCE` (RFC 3339) is when that kache started: the event
//! read must return every line from then on.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use kache_protocol::time::{parse_rfc3339, rfc3339, rfc3339_z};

fn copies() -> PathBuf {
    PathBuf::from(std::env::var_os("KACHE_COMPAT_COPIES").expect("KACHE_COMPAT_COPIES"))
}

fn since() -> jiff::Timestamp {
    let text = std::env::var("KACHE_COMPAT_SINCE").expect("KACHE_COMPAT_SINCE");
    parse_rfc3339(&text).expect("KACHE_COMPAT_SINCE is RFC 3339")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The archived segments' lines and the log's, as written.
fn event_lines(run: &Path) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut segments: Vec<PathBuf> = std::fs::read_dir(run.join("events.jsonl.archive"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    segments.sort();
    for segment in segments {
        let decoder =
            zstd::stream::read::Decoder::new(std::fs::File::open(&segment).unwrap()).unwrap();
        for line in std::io::BufReader::new(decoder).split(b'\n') {
            lines.push(line.unwrap());
        }
    }
    let log = std::fs::File::open(run.join("events.jsonl")).unwrap();
    for line in std::io::BufReader::new(log).split(b'\n') {
        lines.push(line.unwrap());
    }
    lines.retain(|line| !line.is_empty());
    lines
}

fn ts_text(value: &serde_json::Value) -> &str {
    value["ts"].as_str().expect("an event line has a ts")
}

#[test]
#[ignore = "needs KACHE_COMPAT_COPIES"]
fn event_times_read_back_and_write_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join("run");
    copy_tree(&copies().join("run"), &run);
    let lines = event_lines(&run);
    assert!(lines.len() > 1_000, "only {} event lines", lines.len());

    let mut builds = 0;
    let mut heartbeats = 0;
    for line in &lines {
        let raw: serde_json::Value = serde_json::from_slice(line).unwrap();
        let text = ts_text(&raw);
        let at = parse_rfc3339(text).unwrap_or_else(|| panic!("{text} does not parse"));
        assert_eq!(rfc3339_z(at), text);
        // The field's own serde, through the struct the line is.
        let again = if raw.get("event").is_some() {
            heartbeats += 1;
            let event: crate::events::HeartbeatEvent = serde_json::from_slice(line).unwrap();
            serde_json::to_value(&event).unwrap()
        } else {
            builds += 1;
            let event: crate::events::BuildEvent = serde_json::from_slice(line)
                .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(line)));
            assert_eq!(event.ts, at);
            serde_json::to_value(&event).unwrap()
        };
        assert_eq!(ts_text(&again), text);
    }

    let since = since();
    let expected = lines
        .iter()
        .filter(|line| !line.windows(8).any(|w| w == b"\"event\":"))
        .filter(|line| {
            let raw: serde_json::Value = serde_json::from_slice(line).unwrap();
            parse_rfc3339(ts_text(&raw)).unwrap() >= since
        })
        .count();
    let read = crate::events::read_events_since(&run.join("events.jsonl"), since).unwrap();
    assert_eq!(read.len(), expected);
    eprintln!(
        "{} lines ({builds} builds, {heartbeats} heartbeats), {} read since {since}",
        lines.len(),
        read.len()
    );
}

#[test]
#[ignore = "needs KACHE_COMPAT_COPIES"]
fn store_ledger_times_read_back_and_write_the_same_bytes() {
    let store = copies().join("store");

    // gc_stats.json: written with `rfc3339`, read with `parse_rfc3339` by
    // the daemon's auto-GC, `kache stats`, the report and the OTel export.
    let gc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(store.join("gc_stats.json")).unwrap())
            .unwrap();
    let last_run = gc["last_run"].as_str().unwrap();
    let at = parse_rfc3339(last_run).unwrap();
    assert_eq!(rfc3339(at), last_run);

    // savings.json: through the ledger's own reader and its serde.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(store.join("savings.json"), dir.path().join("savings.json")).unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("savings.json")).unwrap())
            .unwrap();
    let totals = crate::savings::read(dir.path()).expect("the ledger reads");
    assert_eq!(totals.hits, raw["hits"].as_u64().unwrap());
    let again = serde_json::to_value(&totals).unwrap();
    assert_eq!(again["since"], raw["since"]);
    eprintln!("gc last_run {last_run}, savings since {}", raw["since"]);
}

#[test]
#[ignore = "needs KACHE_COMPAT_COPIES"]
fn index_times_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index-sample.db");
    std::fs::copy(copies().join("store/index-sample.db"), &db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mut texts: Vec<String> = Vec::new();
    for sql in [
        "SELECT created_at FROM entries",
        "SELECT last_accessed FROM entries",
        "SELECT evicted_at FROM eviction_tombstones",
        "SELECT demanded_at FROM eviction_tombstones WHERE demanded_at IS NOT NULL",
    ] {
        let mut statement = conn.prepare(sql).unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        texts.extend(rows.map(Result::unwrap));
    }
    assert!(texts.len() > 1_000, "only {} index times", texts.len());
    for text in &texts {
        // `YYYY-MM-DD HH:MM:SS`, read here by hand, not by the reader tested.
        let field = |range: std::ops::Range<usize>| text[range].parse::<i8>().unwrap();
        let year: i16 = text[0..4].parse().unwrap();
        let at = jiff::civil::date(year, field(5..7), field(8..10))
            .at(field(11..13), field(14..16), field(17..19), 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp();
        assert_eq!(
            crate::term::local_store_time(text),
            crate::term::local_datetime(at)
        );
    }
    eprintln!("{} index times", texts.len());
}

#[test]
#[ignore = "needs KACHE_COMPAT_COPIES"]
fn seed_note_times_write_the_same_bytes() {
    let notes = std::fs::read_to_string(copies().join("log/seed-notes.log")).unwrap();
    let mut count = 0;
    for line in notes.lines() {
        let (time, rest) = line.split_once("  INFO kache::target_seed: ").unwrap();
        assert!(!rest.is_empty());
        let at = parse_rfc3339(time).unwrap();
        assert_eq!(crate::target_seed::note_time(at), time);
        count += 1;
    }
    assert!(count > 0);
    eprintln!("{count} seed notes");
}
