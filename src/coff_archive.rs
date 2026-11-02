//! A digest of an archive of COFF objects that leaves out the build directory
//! the objects record.
//!
//! The `cc` crate builds a `-sys` crate's C code for `*-windows-msvc` with
//! `cl.exe` and archives it with `lib.exe`, both in the build script's
//! `OUT_DIR`. The archive's bytes then name that directory several times:
//!
//! - `lib.exe` names each member by the path it was given, the object's
//!   absolute path in `OUT_DIR`, and dates it with the object's modification
//!   time;
//! - `cl.exe` writes the object's path into its `.debug$S` section (the
//!   `S_OBJNAME` record), and with `-Z7` the directory, the source and the
//!   command line into `.debug$T` (`LF_BUILDINFO`); `/Brepro` makes the COFF
//!   header's timestamp a hash of the object, and `.chks64` holds checksums of
//!   its sections, so both change with any of those paths.
//!
//! rustc bundles the archive into the crate's rlib, so its digest is part of
//! the crate's cache key, and a new target directory (every new checkout,
//! `target/integrate-clippy` beside `target/`) missed the crate and every
//! crate above it (ring and everything that uses TLS, measured on Windows on
//! 2026-10-18), while the code in the archive was the same.
//!
//! [`path_free_digest`] hashes what the archive holds with each occurrence of
//! the archive's own directory cut out, and leaves out what only records
//! where or when it was built:
//!
//! - member names and every string in the objects, as the pieces around each
//!   occurrence of the directory, length-framed, so a name that differs
//!   anywhere else still differs;
//! - member order, and each symbol of the linker members by the member that
//!   defines it (their offsets follow from the members' sizes, which follow
//!   from the path's length);
//! - each object's machine, section count, symbol count and characteristics;
//!   each section's name, characteristics, data and relocations; each
//!   symbol, with its auxiliary records. `.debug$S` and `.debug$T` are read
//!   record by record, so the record and subsection lengths the path's length
//!   sets are framed by this hash instead, and a relocation into `.debug$S`
//!   is placed by its record and its offset in the record without the
//!   directory;
//! - not hashed: member dates, user, group and mode, the COFF timestamp, the
//!   `.chks64` section, and the length and checksum of a debug section's
//!   section symbol, all derived from what is hashed or from the time.
//!
//! Anything else returns `None` and the caller keeps its digest of the whole
//! archive: big-object COFF, a `/SYM64/` table, an object with an optional
//! header, a debug section other than `$S` and `$T`, a relocation outside a
//! record, malformed sizes or offsets.
//!
//! What a hit then restores is the rlib another target directory built, with
//! that directory's path in its member names and debug records: debuggers and
//! linker map files name the other directory, as for the build-script paths
//! kept out of keys before. The code is the same.

use std::collections::HashMap;

const DOMAIN: &[u8] = b"kache.native-ar.coff-path-free.v2\0";
const HEADER: usize = 60;
const IMAGE_SCN_LNK_NRELOC_OVFL: u32 = 0x0100_0000;
const IMAGE_SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
const IMAGE_SYM_CLASS_STATIC: u8 = 3;
const CV_SIGNATURE_C13: u32 = 4;
const DEBUG_S_SYMBOLS: u32 = 0xF1;

/// The digest of `bytes`, an archive of COFF objects, with each occurrence of
/// `dir` (the archive's directory, as the build spelled it) left out. `None`
/// when the archive is anything this does not read in full.
pub(crate) fn path_free_digest(bytes: &[u8], dir: &[u8]) -> Option<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    archive(&mut Hash(&mut hasher, dir, false), bytes)?;
    Some(hasher.finalize().to_hex().to_string())
}

/// The directory to cut out of an archive found in `own_dir`: the one its
/// members are named under when `lib.exe` named them by absolute paths, else
/// `own_dir`. The same where the archive was built (`cc` gives `lib.exe` the
/// objects in `OUT_DIR`, so they agree), and the directory of the target
/// that built it once kache restored it into another one, so a crate that
/// bundles or links it keys the same in both.
pub(crate) fn digest_dir<'a>(bytes: &'a [u8], own_dir: &'a [u8]) -> &'a [u8] {
    recorded_dir(bytes).unwrap_or(own_dir)
}

/// The one absolute directory every member of the archive is named under,
/// as `lib.exe` names each by the path it was given. `None` when a member is
/// named by a bare name (`llvm-ar`, an import member) or two directories.
pub(crate) fn recorded_dir(bytes: &[u8]) -> Option<&[u8]> {
    let (_, members) = parse(bytes)?;
    let mut dir: Option<&[u8]> = None;
    for member in &members {
        let cut = member.name.iter().rposition(|&b| b == b'/' || b == b'\\')?;
        let parent = &member.name[..cut];
        if !is_absolute(parent) {
            return None;
        }
        match dir {
            None => dir = Some(parent),
            Some(dir) if same_path(dir, parent) => {}
            Some(_) => return None,
        }
    }
    dir
}

/// A Unix path below `/`, a drive path below its root, or a UNC path.
fn is_absolute(path: &[u8]) -> bool {
    let separator = |byte: u8| byte == b'/' || byte == b'\\';
    match path {
        [b'/', rest @ ..] => !rest.is_empty(),
        [drive, b':', sep, rest @ ..] => {
            drive.is_ascii_alphabetic() && separator(*sep) && !rest.is_empty()
        }
        [b'\\', b'\\', rest @ ..] => !rest.is_empty(),
        _ => false,
    }
}

/// Whether `bytes`, a COFF archive or object this reads in full, spells
/// `dir` only where the tools record where they ran: member names (`lib.exe`)
/// and the `.debug$S` and `.debug$T` records (`cl.exe`'s `S_OBJNAME`, `-Z7`'s
/// `LF_BUILDINFO`). Not in section data, a section or symbol name: what the
/// code itself reads (`__FILE__` of a generated source) names the directory.
pub(crate) fn spells_dir_only_in_build_records(bytes: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return false;
    }
    let mut hasher = blake3::Hasher::new();
    let mut hash = Hash(&mut hasher, dir, false);
    let read = if bytes.starts_with(b"!<arch>\n") {
        archive(&mut hash, bytes)
    } else {
        object(&mut hash, bytes)
    };
    read.is_some() && !hash.2
}

/// `bytes` with each occurrence of `dir` cut out, the pieces joined by NUL.
pub(crate) fn without_dir(bytes: &[u8], dir: &[u8]) -> Vec<u8> {
    pieces(bytes, dir).join(&0u8)
}

/// The hasher, the directory cut out of what it hashes, and whether it was
/// cut out of what the code reads (see [`Hash::code`]).
struct Hash<'a>(&'a mut blake3::Hasher, &'a [u8], bool);

impl Hash<'_> {
    fn u64(&mut self, value: u64) {
        self.0.update(&value.to_le_bytes());
    }

    fn tag(&mut self, tag: &[u8]) {
        self.bytes(tag);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.0.update(bytes);
    }

    /// [`Self::text`] for what the code reads (section data, section and
    /// symbol names): notes whether the directory occurs there.
    fn code(&mut self, bytes: &[u8]) {
        if !self.1.is_empty() && pieces(bytes, self.1).len() > 1 {
            self.2 = true;
        }
        self.text(bytes);
    }

    /// `bytes` as the pieces between occurrences of the directory.
    fn text(&mut self, bytes: &[u8]) {
        let pieces = pieces(bytes, self.1);
        self.u64(pieces.len() as u64);
        for piece in pieces {
            self.bytes(piece);
        }
    }
}

/// `bytes` split at each occurrence of `dir`, left to right. A `/` and a `\`
/// match each other: Cargo can spell a build script's `OUT_DIR` with both
/// (`target/cranelift\debug\build\ring/<hash>\out` for a relative
/// `build.target-dir`, with its per-unit build directories), and `cl.exe`
/// records the path it was given with backslashes only.
fn pieces<'a>(bytes: &'a [u8], dir: &[u8]) -> Vec<&'a [u8]> {
    if dir.is_empty() {
        return vec![bytes];
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at + dir.len() <= bytes.len() {
        if same_path(&bytes[at..at + dir.len()], dir) {
            pieces.push(&bytes[start..at]);
            at += dir.len();
            start = at;
        } else {
            at += 1;
        }
    }
    pieces.push(&bytes[start..]);
    pieces
}

/// `a` and `b` byte for byte, a `/` and a `\` alike.
fn same_path(a: &[u8], b: &[u8]) -> bool {
    let separator = |byte: u8| byte == b'/' || byte == b'\\';
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(&x, &y)| x == y || (separator(x) && separator(y)))
}

/// Where `offset` falls in `bytes` once the occurrences of `dir` are cut out:
/// how many come before it, and the offset without them. `None` inside one.
fn offset_without(bytes: &[u8], dir: &[u8], offset: usize) -> Option<(u64, u64)> {
    let mut at = 0;
    for (before, piece) in pieces(bytes, dir).into_iter().enumerate() {
        let end = at + piece.len();
        if offset < end || (offset == end && end == bytes.len()) {
            return Some((before as u64, (offset - before * dir.len()) as u64));
        }
        at = end + dir.len();
        if offset < at {
            return None;
        }
    }
    None
}

fn u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u32_be(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn decimal(field: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(field).ok()?.trim_end_matches(' ');
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A NUL- or `/\n`-terminated name at `at` in an archive's long-name table.
fn long_name(table: &[u8], at: usize) -> Option<&[u8]> {
    let rest = table.get(at..)?;
    let end = rest
        .iter()
        .position(|&b| b == 0 || b == b'\n')
        .unwrap_or(rest.len());
    let name = &rest[..end];
    Some(name.strip_suffix(b"/").unwrap_or(name))
}

struct Member<'a> {
    offset: usize,
    name: &'a [u8],
    data: &'a [u8],
}

/// The archive's linker members and its other members, in order.
type Parsed<'a> = (Vec<&'a [u8]>, Vec<Member<'a>>);

fn parse(bytes: &[u8]) -> Option<Parsed<'_>> {
    let mut at = bytes.strip_prefix(b"!<arch>\n").map(|_| 8)?;
    let mut linker = Vec::new();
    let mut long_names: &[u8] = &[];
    let mut members = Vec::new();
    while at < bytes.len() {
        let header = bytes.get(at..at + HEADER)?;
        if &header[58..60] != b"`\n" {
            return None;
        }
        let size = decimal(&header[48..58])?;
        let data = bytes.get(at + HEADER..(at + HEADER).checked_add(size)?)?;
        let field = std::str::from_utf8(&header[..16])
            .ok()?
            .trim_end_matches(' ');
        match field {
            "/" if members.is_empty() && long_names.is_empty() && linker.len() < 2 => {
                linker.push(data)
            }
            "//" if members.is_empty() && long_names.is_empty() => long_names = data,
            name if name.starts_with('/') => {
                let offset = name[1..].parse().ok()?;
                members.push(Member {
                    offset: at,
                    name: long_name(long_names, offset)?,
                    data,
                });
            }
            name => members.push(Member {
                offset: at,
                name: name.strip_suffix('/').unwrap_or(name).as_bytes(),
                data,
            }),
        }
        at += HEADER + size + (size & 1);
    }
    if members.is_empty() {
        return None;
    }
    Some((linker, members))
}

fn archive(hash: &mut Hash<'_>, bytes: &[u8]) -> Option<()> {
    let (linker, members) = parse(bytes)?;
    let ordinals: HashMap<usize, u64> = members
        .iter()
        .enumerate()
        .map(|(ordinal, member)| (member.offset, ordinal as u64))
        .collect();
    hash.tag(b"members");
    hash.u64(members.len() as u64);
    for (index, table) in linker.iter().enumerate() {
        if index == 0 {
            first_linker_member(hash, table, &ordinals)?;
        } else {
            second_linker_member(hash, table, &ordinals)?;
        }
    }
    for member in &members {
        hash.text(member.name);
        if member.data.starts_with(&[0, 0, 0xFF, 0xFF]) {
            // A short import object (version 0) names a DLL and a symbol, no
            // path. Version 1 and up is the big-object format: not read.
            if u16_le(member.data, 4)? != 0 {
                return None;
            }
            hash.tag(b"import");
            hash.bytes(member.data);
        } else {
            object(hash, member.data)?;
        }
    }
    Some(())
}

/// `n` names after `at`, NUL-terminated.
fn names(bytes: &[u8], mut at: usize, n: usize) -> Option<Vec<&[u8]>> {
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        let rest = bytes.get(at..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        names.push(&rest[..end]);
        at += end + 1;
    }
    Some(names)
}

/// The first linker member: each symbol with the ordinal of its member.
fn first_linker_member(
    hash: &mut Hash<'_>,
    table: &[u8],
    ordinals: &HashMap<usize, u64>,
) -> Option<()> {
    let count = u32_be(table, 0)? as usize;
    let names = names(table, 4usize.checked_add(count.checked_mul(4)?)?, count)?;
    hash.tag(b"linker1");
    hash.u64(count as u64);
    for (index, name) in names.into_iter().enumerate() {
        let offset = u32_be(table, 4 + 4 * index)? as usize;
        hash.code(name);
        hash.u64(*ordinals.get(&offset)?);
    }
    Some(())
}

/// The second linker member (COFF): the members' ordinals, then each symbol
/// with the index it names in that list.
fn second_linker_member(
    hash: &mut Hash<'_>,
    table: &[u8],
    ordinals: &HashMap<usize, u64>,
) -> Option<()> {
    let members = u32_le(table, 0)? as usize;
    hash.tag(b"linker2");
    hash.u64(members as u64);
    for index in 0..members {
        let offset = u32_le(table, 4 + 4 * index)? as usize;
        hash.u64(*ordinals.get(&offset)?);
    }
    let at = 4usize.checked_add(members.checked_mul(4)?)?;
    let symbols = u32_le(table, at)? as usize;
    let indices = at + 4;
    let names = names(
        table,
        indices.checked_add(symbols.checked_mul(2)?)?,
        symbols,
    )?;
    hash.u64(symbols as u64);
    for (index, name) in names.into_iter().enumerate() {
        hash.u64(u16_le(table, indices + 2 * index)?.into());
        hash.code(name);
    }
    Some(())
}

/// What a section is to the hash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    /// `.debug$S`: CodeView symbols and subsections.
    Symbols,
    /// `.debug$T`: CodeView type records.
    Types,
    /// `.chks64`: checksums of the other sections.
    Checksums,
}

fn object(hash: &mut Hash<'_>, data: &[u8]) -> Option<()> {
    let machine = u16_le(data, 0)?;
    if !matches!(machine, 0x8664 | 0x014c | 0xaa64) {
        return None;
    }
    let sections = u16_le(data, 2)? as usize;
    let symbol_table = u32_le(data, 8)? as usize;
    let symbols = u32_le(data, 12)? as usize;
    if u16_le(data, 16)? != 0 {
        return None;
    }
    let characteristics = u16_le(data, 18)?;
    hash.tag(b"coff");
    hash.u64(machine.into());
    hash.u64(sections as u64);
    hash.u64(symbols as u64);
    hash.u64(characteristics.into());
    let strings_at = symbol_table.checked_add(symbols.checked_mul(18)?)?;
    let strings: &[u8] = if symbols == 0 && symbol_table == 0 {
        &[]
    } else {
        let size = u32_le(data, strings_at)? as usize;
        data.get(strings_at..strings_at.checked_add(size.max(4))?)?
    };
    let string = |offset: usize| -> Option<&[u8]> {
        let rest = strings.get(offset..)?;
        Some(&rest[..rest.iter().position(|&b| b == 0)?])
    };
    let mut kinds = Vec::with_capacity(sections);
    for index in 0..sections {
        let header = data.get(20 + 40 * index..20 + 40 * (index + 1))?;
        let short = &header[..8];
        let name = match short.strip_prefix(b"/") {
            Some(digits) => string(decimal(
                &digits[..digits.iter().position(|&b| b == 0).unwrap_or(digits.len())],
            )?)?,
            None => &short[..short.iter().position(|&b| b == 0).unwrap_or(8)],
        };
        let virtual_size = u32_le(header, 8)?;
        let raw_size = u32_le(header, 16)? as usize;
        let raw_at = u32_le(header, 20)? as usize;
        let relocations_at = u32_le(header, 24)? as usize;
        let relocation_count = u16_le(header, 32)? as usize;
        let flags = u32_le(header, 36)?;
        if flags & IMAGE_SCN_LNK_NRELOC_OVFL != 0 {
            return None;
        }
        let contents = if raw_size == 0 || flags & IMAGE_SCN_CNT_UNINITIALIZED_DATA != 0 {
            &[][..]
        } else {
            data.get(raw_at..raw_at.checked_add(raw_size)?)?
        };
        let relocations = data
            .get(relocations_at..relocations_at.checked_add(relocation_count.checked_mul(10)?)?)?;
        let kind = match name {
            b".debug$S" => Kind::Symbols,
            b".debug$T" => Kind::Types,
            b".chks64" => Kind::Checksums,
            name if name.starts_with(b".debug$") => return None,
            _ => Kind::Plain,
        };
        hash.code(name);
        hash.u64(virtual_size.into());
        hash.u64(flags.into());
        hash.u64(relocation_count as u64);
        match kind {
            Kind::Plain => {
                hash.code(contents);
                hash.bytes(relocations);
            }
            Kind::Symbols => debug_symbols(hash, contents, relocations)?,
            Kind::Types => {
                if relocation_count != 0 {
                    return None;
                }
                debug_types(hash, contents)?;
            }
            Kind::Checksums => hash.tag(b"chks64"),
        }
        kinds.push(kind);
    }
    let mut index = 0;
    while index < symbols {
        let record = data.get(symbol_table + 18 * index..symbol_table + 18 * (index + 1))?;
        let name = if record[..4] == [0; 4] {
            string(u32_le(record, 4)? as usize)?
        } else {
            &record[..record[..8].iter().position(|&b| b == 0).unwrap_or(8)]
        };
        let value = u32_le(record, 8)?;
        let section = i16::from_le_bytes([record[12], record[13]]);
        let class = record[16];
        let aux = record[17] as usize;
        hash.code(name);
        hash.bytes(&record[8..18]);
        let aux_records =
            data.get(symbol_table + 18 * (index + 1)..symbol_table + 18 * (index + 1 + aux))?;
        let derived = class == IMAGE_SYM_CLASS_STATIC
            && aux == 1
            && value == 0
            && section > 0
            && kinds
                .get(section as usize - 1)
                .is_some_and(|kind| *kind != Kind::Plain);
        if derived {
            // A section definition: its length and checksum follow from
            // the section's data, hashed above without the directory.
            hash.tag(b"section");
            hash.bytes(&aux_records[4..8]);
            hash.bytes(&aux_records[12..15]);
        } else {
            hash.code(aux_records);
        }
        index += 1 + aux;
    }
    Some(())
}

/// One record's data, and where it starts in its section.
struct Record<'a> {
    start: usize,
    data: &'a [u8],
    subsection: u64,
    index: u64,
}

/// `.debug$S`: the signature, then subsections (kind, length, data, padding
/// to four bytes); a symbols subsection is a run of records (length, kind,
/// data). Lengths and padding are left to this hash's framing.
fn debug_symbols(hash: &mut Hash<'_>, contents: &[u8], relocations: &[u8]) -> Option<()> {
    if u32_le(contents, 0)? != CV_SIGNATURE_C13 {
        return None;
    }
    hash.tag(b"debug$S");
    let mut records = Vec::new();
    let mut at = 4;
    let mut subsection = 0u64;
    while at < contents.len() {
        let kind = u32_le(contents, at)?;
        let length = u32_le(contents, at + 4)? as usize;
        let start = at + 8;
        let data = contents.get(start..start.checked_add(length)?)?;
        hash.u64(kind.into());
        if kind == DEBUG_S_SYMBOLS {
            let mut offset = 0;
            let mut index = 0u64;
            hash.tag(b"symbols");
            while offset < data.len() {
                let length = u16_le(data, offset)? as usize;
                let record_kind = u16_le(data, offset + 2)?;
                if length < 2 {
                    return None;
                }
                let record = data.get(offset + 4..offset + 2 + length)?;
                hash.u64(record_kind.into());
                hash.text(record);
                records.push(Record {
                    start: start + offset + 4,
                    data: record,
                    subsection,
                    index,
                });
                offset += 2 + length;
                index += 1;
            }
        } else {
            hash.text(data);
            records.push(Record {
                start,
                data,
                subsection,
                index: u64::MAX,
            });
        }
        at = start + length;
        let padded = at.next_multiple_of(4).min(contents.len());
        if contents[at..padded].iter().any(|&b| b != 0) {
            return None;
        }
        at = padded;
        subsection += 1;
    }
    hash.tag(b"relocations");
    for relocation in relocations.as_chunks::<10>().0 {
        let address = u32_le(relocation, 0)? as usize;
        let record = records
            .iter()
            .find(|record| record.start <= address && address < record.start + record.data.len())?;
        let (before, offset) = offset_without(record.data, hash.1, address - record.start)?;
        hash.u64(record.subsection);
        hash.u64(record.index);
        hash.u64(before);
        hash.u64(offset);
        hash.bytes(&relocation[4..10]);
    }
    Some(())
}

/// `.debug$T`: the signature, then type records (length, kind, data).
fn debug_types(hash: &mut Hash<'_>, contents: &[u8]) -> Option<()> {
    if u32_le(contents, 0)? != CV_SIGNATURE_C13 {
        return None;
    }
    hash.tag(b"debug$T");
    let mut at = 4;
    while at < contents.len() {
        let length = u16_le(contents, at)? as usize;
        let kind = u16_le(contents, at + 2)?;
        if length < 2 {
            return None;
        }
        hash.u64(kind.into());
        hash.text(contents.get(at + 4..at + 2 + length)?);
        at += 2 + length;
    }
    Some(())
}

#[cfg(test)]
mod tests;
