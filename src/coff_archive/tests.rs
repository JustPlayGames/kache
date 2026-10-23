use super::*;

// `gen`: one C function built by the `cc` crate with `cl.exe` and archived
// with `lib.exe` (MSVC 2022, 2026-10-18), in two target directories whose
// paths differ in length; the `z7` pair with `CFLAGS=-Z7` (debug records
// with relocations, `.debug$T`).
const SHORT: &[u8] = include_bytes!("fixtures/gen-short.lib");
const LONG: &[u8] = include_bytes!("fixtures/gen-long.lib");
const Z7_SHORT: &[u8] = include_bytes!("fixtures/gen-z7-short.lib");
const Z7_LONG: &[u8] = include_bytes!("fixtures/gen-z7-long.lib");
const SHORT_DIR: &str = r"C:\Users\User\ko2\tA\debug\build\gen-2f3778baca245d4f\out";
const LONG_DIR: &str =
    r"C:\Users\User\ko2\longer-folder-name\tC\debug\build\gen-2f3778baca245d4f\out";
const Z7_SHORT_DIR: &str = r"C:\Users\User\ko2\tF\debug\build\gen-2f3778baca245d4f\out";
const Z7_LONG_DIR: &str =
    r"C:\Users\User\ko2\much-longer-folder-name\tG\debug\build\gen-2f3778baca245d4f\out";

fn digest(bytes: &[u8], dir: &str) -> String {
    path_free_digest(bytes, dir.as_bytes()).expect("a COFF archive this reads")
}

/// Replace the first occurrence of `from` in `bytes` with `to` (same length).
fn patched(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let at = bytes
        .windows(from.len())
        .position(|window| window == from)
        .expect("pattern present");
    let mut out = bytes.to_vec();
    out[at..at + to.len()].copy_from_slice(to);
    out
}

#[test]
fn the_fixtures_differ_only_in_paths_and_times() {
    assert_ne!(SHORT, LONG);
    assert_ne!(Z7_SHORT, Z7_LONG);
    assert_ne!(SHORT.len(), LONG.len(), "the paths differ in length");
}

#[test]
fn archives_from_two_target_directories_hash_alike() {
    assert_eq!(digest(SHORT, SHORT_DIR), digest(LONG, LONG_DIR));
}

#[test]
fn debug_records_and_their_relocations_hash_alike_across_directories() {
    assert_eq!(digest(Z7_SHORT, Z7_SHORT_DIR), digest(Z7_LONG, Z7_LONG_DIR));
    assert_ne!(
        digest(Z7_SHORT, Z7_SHORT_DIR),
        digest(SHORT, SHORT_DIR),
        "debug info is part of the identity"
    );
}

#[test]
fn a_path_other_than_the_archives_directory_still_counts() {
    // Told the wrong directory, nothing is cut out and the paths differ.
    assert_ne!(digest(SHORT, SHORT_DIR), digest(LONG, SHORT_DIR));
    // The directory spelled differently is not cut out either.
    assert_ne!(
        digest(SHORT, SHORT_DIR),
        digest(SHORT, &SHORT_DIR.to_lowercase())
    );
}

/// Cargo spelled a Cranelift build's `OUT_DIR` with both separators
/// (`<checkout>\target/cranelift\debug\build\ring/<hash>\out`, a relative
/// `build.target-dir` and the per-unit build directories); `cl.exe` recorded
/// it with backslashes. Matched only as spelled, nothing was cut out, and
/// every `-sys` crate missed in each new checkout. A `/` and a `\` now
/// match each other.
#[test]
fn the_directory_is_cut_out_whichever_separators_spell_it() {
    let mixed = |dir: &str| {
        let mut spelled = dir.replacen(r"\debug", "/debug", 1);
        spelled = spelled.replacen(r"\gen-", "/gen-", 1);
        assert_ne!(spelled, dir);
        spelled
    };
    assert_eq!(digest(SHORT, &mixed(SHORT_DIR)), digest(SHORT, SHORT_DIR));
    assert_eq!(
        digest(SHORT, &mixed(SHORT_DIR)),
        digest(LONG, &mixed(LONG_DIR))
    );
    assert_eq!(
        digest(Z7_SHORT, &mixed(Z7_SHORT_DIR)),
        digest(Z7_LONG, &Z7_LONG_DIR.replace('\\', "/"))
    );
    assert!(same_path(br"a/b\c", br"a\b/c"));
    assert!(!same_path(br"a/b", br"a/c"));
    assert!(!same_path(b"a/", b"a/b"));
    assert_eq!(pieces(br"x C:\o\p y C:/o/p z", br"C:\o/p"), [&b"x "[..], b" y ", b" z"]);
}

#[test]
fn a_changed_object_name_changes_the_digest() {
    let renamed = patched(SHORT, b"db3b6bfb95261072-f.o", b"db3b6bfb95261072-g.o");
    assert_ne!(digest(SHORT, SHORT_DIR), digest(&renamed, SHORT_DIR));
}

#[test]
fn changed_code_changes_the_digest() {
    // `.text$mn` holds `mov eax, 1; ret` (b8 01 00 00 00 c3).
    let changed = patched(
        SHORT,
        &[0xb8, 0x01, 0x00, 0x00, 0x00, 0xc3],
        &[0xb8, 0x02, 0x00, 0x00, 0x00, 0xc3],
    );
    assert_ne!(digest(SHORT, SHORT_DIR), digest(&changed, SHORT_DIR));
}

#[test]
fn changed_debug_records_change_the_digest() {
    // The source path `cl.exe` recorded, outside the target directory.
    let changed = patched(Z7_SHORT, br"proj\gen\f.c", br"proj\gen\g.c");
    assert_ne!(
        digest(Z7_SHORT, Z7_SHORT_DIR),
        digest(&changed, Z7_SHORT_DIR)
    );
}

#[test]
fn member_dates_do_not_count() {
    let header_date = &SHORT[8 + 16..8 + 28];
    let redated = patched(SHORT, header_date, b"1000000000  ");
    assert_eq!(digest(SHORT, SHORT_DIR), digest(&redated, SHORT_DIR));
}

#[test]
fn what_it_does_not_read_is_left_to_the_caller() {
    assert!(path_free_digest(b"not an archive", SHORT_DIR.as_bytes()).is_none());
    assert!(path_free_digest(&SHORT[..SHORT.len() - 7], SHORT_DIR.as_bytes()).is_none());
    assert!(path_free_digest(b"!<arch>\n", SHORT_DIR.as_bytes()).is_none());
    // An ELF object is not COFF.
    let mut elf =
        b"!<arch>\nobj.o/          0           0     0     644     4         `\n".to_vec();
    elf.extend_from_slice(b"\x7fELF");
    assert!(path_free_digest(&elf, b"/x").is_none());
    // An unknown debug section.
    let unknown = patched(Z7_SHORT, b".debug$T", b".debug$P");
    assert!(path_free_digest(&unknown, Z7_SHORT_DIR.as_bytes()).is_none());
}

#[test]
fn pieces_split_at_each_occurrence() {
    assert_eq!(pieces(b"aXXbXXc", b"XX"), [&b"a"[..], b"b", b"c"]);
    assert_eq!(pieces(b"XX", b"XX"), [&b""[..], b""]);
    assert_eq!(pieces(b"abc", b""), [&b"abc"[..]]);
    assert_eq!(offset_without(b"aXXbc", b"XX", 0), Some((0, 0)));
    assert_eq!(offset_without(b"aXXbc", b"XX", 3), Some((1, 1)));
    assert_eq!(offset_without(b"aXXbc", b"XX", 4), Some((1, 2)));
    assert_eq!(offset_without(b"aXXbc", b"XX", 1), None);
}
