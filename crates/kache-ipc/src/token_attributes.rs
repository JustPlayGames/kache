//! The security attributes of a Windows token, as `TokenSecurityAttributes`
//! returns them (`TOKEN_SECURITY_ATTRIBUTES_INFORMATION`, version 1).
//!
//! Plain data and pointer checks only, so the rule is tested on every host.

use std::io;
use std::mem::size_of;

/// The attribute Windows 11 gives each process token: two UINT64 values that
/// differ per process. It carries no access, so its values are not compared.
const PROCESS_NONCE: &str = "TSA://ProcUnique";

/// `TOKEN_SECURITY_ATTRIBUTES_INFORMATION_VERSION_V1`.
const VERSION_V1: u16 = 1;

/// One attribute without its values.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Attribute {
    name: Vec<u16>,
    value_type: u16,
    flags: u32,
    value_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawAttribute {
    name: UnicodeString,
    value_type: u16,
    reserved: u16,
    flags: u32,
    value_count: u32,
    values: *const u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawInformation {
    version: u16,
    reserved: u16,
    attribute_count: u32,
    attributes: *const RawAttribute,
}

fn invalid(what: &str) -> io::Error {
    io::Error::other(format!("invalid compiler token attributes: {what}"))
}

/// The offset of `length` bytes at `pointer` inside `bytes`.
fn offset_in(bytes: &[u8], pointer: usize, length: usize) -> io::Result<usize> {
    let offset = pointer
        .checked_sub(bytes.as_ptr() as usize)
        .ok_or_else(|| invalid("pointer before the record"))?;
    if offset > bytes.len() || length > bytes.len() - offset {
        return Err(invalid("pointer past the record"));
    }
    Ok(offset)
}

fn read<T: Copy>(bytes: &[u8], offset: usize) -> io::Result<T> {
    if offset > bytes.len() || size_of::<T>() > bytes.len() - offset {
        return Err(invalid("truncated record"));
    }
    Ok(unsafe { bytes.as_ptr().add(offset).cast::<T>().read_unaligned() })
}

/// Reads the attributes from `bytes`, whose pointers point into `bytes` itself.
pub(crate) fn parse(bytes: &[u8]) -> io::Result<Vec<Attribute>> {
    let header = read::<RawInformation>(bytes, 0)?;
    if header.version != VERSION_V1 {
        return Err(invalid("unknown version"));
    }
    let count = header.attribute_count as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let array_length = count
        .checked_mul(size_of::<RawAttribute>())
        .ok_or_else(|| invalid("attribute count"))?;
    let array = offset_in(bytes, header.attributes as usize, array_length)?;
    let mut attributes = (0..count)
        .map(|index| {
            let raw = read::<RawAttribute>(bytes, array + index * size_of::<RawAttribute>())?;
            let length = raw.name.length as usize;
            if !length.is_multiple_of(2) {
                return Err(invalid("odd name length"));
            }
            let name = offset_in(bytes, raw.name.buffer as usize, length)?;
            let name = bytes[name..name + length]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_ne_bytes(*pair))
                .collect();
            Ok(Attribute {
                name,
                value_type: raw.value_type,
                flags: raw.flags,
                value_count: raw.value_count,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    attributes.sort();
    Ok(attributes)
}

/// Whether two tokens of different generations (`ModifiedId`) differ in
/// their attributes only by the per-process nonce's values: the same
/// attributes by name, type, flags and value count, and no attribute but
/// `TSA://ProcUnique`. Any other attribute keeps them apart.
pub(crate) fn only_process_nonce_differs(client: &[Attribute], daemon: &[Attribute]) -> bool {
    let nonce = PROCESS_NONCE.encode_utf16().collect::<Vec<_>>();
    client == daemon && client.iter().all(|attribute| attribute.name == nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(name, type, flags, values)`
    type Spec<'a> = (&'a str, u16, u32, &'a [u64]);

    const UINT64: u16 = 2;
    const STRING: u16 = 3;

    /// A record laid out as `NtQueryInformationToken` returns it: header,
    /// attribute array, then names and values, every pointer into the buffer.
    fn record(specs: &[Spec<'_>]) -> Vec<u64> {
        let header = size_of::<RawInformation>();
        let array = header + specs.len() * size_of::<RawAttribute>();
        let data = specs
            .iter()
            .map(|(name, _, _, values)| (name.len() * 2).next_multiple_of(8) + values.len() * 8)
            .sum::<usize>();
        let mut storage = vec![0u64; (array + data).div_ceil(8)];
        let base = storage.as_mut_ptr().cast::<u8>();
        let mut next = array;
        let mut raw = Vec::new();
        for (name, value_type, flags, values) in specs {
            let name = name.encode_utf16().collect::<Vec<_>>();
            let name_at = unsafe { base.add(next) };
            unsafe {
                std::ptr::copy_nonoverlapping(name.as_ptr().cast::<u8>(), name_at, name.len() * 2)
            };
            next += (name.len() * 2).next_multiple_of(8);
            let values_at = unsafe { base.add(next) };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    values.as_ptr().cast::<u8>(),
                    values_at,
                    values.len() * 8,
                )
            };
            next += values.len() * 8;
            raw.push(RawAttribute {
                name: UnicodeString {
                    length: (name.len() * 2) as u16,
                    maximum_length: (name.len() * 2) as u16,
                    buffer: name_at.cast(),
                },
                value_type: *value_type,
                reserved: 0,
                flags: *flags,
                value_count: values.len() as u32,
                values: values_at,
            });
        }
        unsafe {
            for (index, attribute) in raw.iter().enumerate() {
                base.add(header + index * size_of::<RawAttribute>())
                    .cast::<RawAttribute>()
                    .write_unaligned(*attribute);
            }
            base.cast::<RawInformation>()
                .write_unaligned(RawInformation {
                    version: VERSION_V1,
                    reserved: 0,
                    attribute_count: specs.len() as u32,
                    attributes: base.add(header).cast(),
                });
        }
        storage
    }

    fn attributes(specs: &[Spec<'_>]) -> Vec<Attribute> {
        let storage = record(specs);
        let bytes =
            unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), storage.len() * 8) };
        parse(bytes).unwrap()
    }

    #[test]
    fn only_the_process_nonce_may_differ_between_token_generations() {
        // As cc-vm-1 (Windows 11 26H2) reported a process and its child.
        let parent = ("TSA://ProcUnique", UINT64, 0x41, &[0x6e, 0x4352d][..]);
        let child = ("TSA://ProcUnique", UINT64, 0x41, &[0x71, 0x43600][..]);
        let app = ("WIN://SYSAPPID", STRING, 0x1, &[0][..]);
        let other = ("TSA://Other", UINT64, 0x41, &[1][..]);
        let cases: &[(&str, &[Spec<'_>], &[Spec<'_>], bool)] = &[
            ("no attributes", &[], &[], true),
            ("nonce values differ", &[parent], &[child], true),
            ("one more attribute", &[child, app], &[parent], false),
            (
                "the same extra attribute",
                &[child, app],
                &[parent, app],
                false,
            ),
            ("another attribute alone", &[other], &[other], false),
            ("nonce on one side only", &[child], &[], false),
            (
                "nonce flags differ",
                &[("TSA://ProcUnique", UINT64, 0x1, &[0x71, 0x43600])],
                &[parent],
                false,
            ),
            (
                "nonce type differs",
                &[("TSA://ProcUnique", STRING, 0x41, &[0x71, 0x43600])],
                &[parent],
                false,
            ),
            (
                "nonce value count differs",
                &[("TSA://ProcUnique", UINT64, 0x41, &[0x71])],
                &[parent],
                false,
            ),
        ];
        for (case, client, daemon, accepted) in cases {
            assert_eq!(
                only_process_nonce_differs(&attributes(client), &attributes(daemon)),
                *accepted,
                "{case}"
            );
        }

        let storage = record(&[parent]);
        let bytes =
            unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), storage.len() * 8) };
        assert!(parse(&bytes[..size_of::<RawInformation>() + 8]).is_err());
    }
}
