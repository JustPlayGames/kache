//! Block cloning on Windows: `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on ReFS and
//! Dev Drive volumes, with the ranges, refusals and per-volume memo of
//! [`crate::block_clone`].
//!
//! Each (source volume, destination volume) pair is probed once per process
//! without writing anything: two volumes, or a destination volume without
//! `FILE_SUPPORTS_BLOCK_REFCOUNTING` (NTFS), never clone, so later files fail
//! at once and the caller copies, instead of each creating, sizing, failing
//! and deleting a destination. A volume-level refusal from a real clone
//! (`ERROR_INVALID_FUNCTION`, `ERROR_NOT_SUPPORTED`, `ERROR_NOT_SAME_DEVICE`)
//! is remembered too; any other failure fails only that file. Where the
//! volume clones, the clone is the one kache always made: whole clusters
//! cloned, the sub-cluster tail copied, a file under one cluster refused.
//! `KACHE_WINDOWS_BLOCK_CLONE=0` turns cloning off.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows_sys::Win32::Storage::FileSystem::{
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE};
use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING;

use crate::block_clone::{
    CloneRange, PairMemo, PairState, aligned_ranges, probe_state, switch_turns_off,
};

/// The environment switch: `0`, `false`, `off` or `no` turns block cloning
/// off, so every clone attempt fails at once and callers copy.
pub(crate) const SWITCH: &str = "KACHE_WINDOWS_BLOCK_CLONE";

fn turned_off() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| switch_turns_off(std::env::var(SWITCH).ok().as_deref()))
}

fn memo() -> MutexGuard<'static, PairMemo> {
    static MEMO: OnceLock<Mutex<PairMemo>> = OnceLock::new();
    MEMO.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A volume as the memo names it: its `\\?\Volume{GUID}\` name, which is the
/// same through every drive letter and mount folder, else its root path.
#[derive(Debug, Clone)]
struct Volume {
    root: Vec<u16>,
    id: String,
    /// Whether `id` is the volume's GUID name, which alone can be compared:
    /// a `subst` drive's root names the volume beneath it another way.
    guid: bool,
}

/// The volume holding `path` (which need not exist), cached by its parent
/// directory: a restore writes many files into one directory.
fn volume_of(path: &Path) -> Option<Volume> {
    static DIRS: OnceLock<Mutex<std::collections::HashMap<PathBuf, Volume>>> = OnceLock::new();
    const MAX_DIRS: usize = 1024;
    let dir = path.parent().unwrap_or(path).to_path_buf();
    let dirs = DIRS.get_or_init(Default::default);
    if let Some(volume) = dirs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&dir)
    {
        return Some(volume.clone());
    }
    let volume = query_volume(path)?;
    let mut dirs = dirs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if dirs.len() >= MAX_DIRS {
        dirs.clear();
    }
    dirs.insert(dir, volume.clone());
    Some(volume)
}

fn query_volume(path: &Path) -> Option<Volume> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = [0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `root` holds `root.len()` units.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return None;
    }
    let root_len = root.iter().position(|&c| c == 0)?;
    let root: Vec<u16> = root[..=root_len].to_vec();
    let mut name = [0u16; 64];
    // SAFETY: `root` is NUL-terminated and ends in a backslash, as the call
    // requires; `name` holds `name.len()` units (a GUID name needs 50).
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    };
    let guid = ok != 0;
    let id = if guid {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        String::from_utf16_lossy(&name[..len])
    } else {
        String::from_utf16_lossy(&root[..root_len])
    };
    Some(Volume {
        id: id.to_ascii_lowercase(),
        root,
        guid,
    })
}

/// Whether the volume reports `FILE_SUPPORTS_BLOCK_REFCOUNTING`, or `None`
/// when it cannot be asked.
fn block_refcounting(volume: &Volume) -> Option<bool> {
    let mut flags: u32 = 0;
    // SAFETY: `volume.root` is NUL-terminated; every buffer passed is null
    // with a zero size, except `flags`, which outlives the call.
    let ok = unsafe {
        GetVolumeInformationW(
            volume.root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    (ok != 0).then_some(flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0)
}

/// The pair's state from the memo, probing the volumes the first time.
fn pair_state(source: &Volume, destination: &Volume, dst: &Path) -> Option<PairState> {
    if let Some(state) = memo().get(&source.id, &destination.id) {
        return Some(state);
    }
    let same_volume = (source.guid && destination.guid).then_some(source.id == destination.id);
    let state = probe_state(
        same_volume,
        block_refcounting(destination),
        crate::copy::windows_cluster_size(dst).ok(),
    )?;
    memo().set(&source.id, &destination.id, state);
    Some(state)
}

fn unsupported(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

/// Clone `src` into a new file `dst`. Fails without creating `dst` when the
/// switch is off or the volume pair is known not to clone; any other failure
/// removes the `dst` it created. The caller copies on any error.
pub(crate) fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    if turned_off() {
        return Err(unsupported(format!(
            "block cloning is turned off ({SWITCH})"
        )));
    }
    let pair = volume_of(src).zip(volume_of(dst));
    let state = pair
        .as_ref()
        .and_then(|(source, destination)| pair_state(source, destination, dst));
    // An empty file "clones" on any volume (nothing to share), as it always
    // did, so only a file with bytes skips to the caller's copy.
    if let Some(PairState::Refused(code)) = state
        && fs::metadata(src)?.len() != 0
    {
        return Err(unsupported(match code {
            Some(code) => format!(
                "these volumes do not block-clone (Windows error {code}, remembered for this process)"
            ),
            None => "the destination volume does not block-clone".to_string(),
        }));
    }

    let mut src_file = File::open(src)?;
    let len = src_file.metadata()?.len();
    // Create a new, writable destination without replacing an existing name.
    let mut dst_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dst)?;

    let ids = pair
        .as_ref()
        .map(|(source, destination)| (source.id.as_str(), destination.id.as_str()));
    let result = clone_into(&mut src_file, &mut dst_file, dst, len, state, ids);
    drop(dst_file);
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

/// The clone kache always made: the whole clusters by FSCTL, the bytes after
/// them copied. Only the cluster size now comes from the memo when known.
fn clone_into(
    src_file: &mut File,
    dst_file: &mut File,
    dst: &Path,
    len: u64,
    state: Option<PairState>,
    ids: Option<(&str, &str)>,
) -> io::Result<()> {
    if len == 0 {
        return Ok(()); // empty file: dst already created empty, nothing to clone
    }

    // Cluster size of the destination volume; clone ranges must be aligned to
    // it. A file smaller than one cluster has no aligned range at all, so bail
    // before the FSCTL rather than issuing a call that cannot succeed: this is
    // the common case for the small files.
    let cluster = match state {
        Some(PairState::Clones { cluster }) => cluster,
        _ => crate::copy::windows_cluster_size(dst)?,
    };
    if len < cluster {
        return Err(unsupported("file smaller than one cluster".to_string()));
    }
    let ranges = aligned_ranges(len, cluster)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cluster exceeds clone range"))?;
    let clone_len = (len / cluster) * cluster;

    // Allocate the destination clusters and set EOF to the cloned prefix.
    dst_file.set_len(clone_len)?;
    let src_h = src_file.as_raw_handle();
    let dst_h = dst_file.as_raw_handle();
    for range in ranges {
        if let Err(error) = duplicate(src_h, dst_h, range) {
            if let Some((source, destination)) = ids {
                memo().record_failure(source, destination, error.raw_os_error());
            }
            return Err(error);
        }
    }

    // Byte-copy the sub-cluster tail, if any, then set the exact final length.
    if clone_len < len {
        src_file.seek(SeekFrom::Start(clone_len))?;
        dst_file.seek(SeekFrom::Start(clone_len))?;
        let copied = io::copy(&mut (&mut *src_file).take(len - clone_len), dst_file)?;
        if copied != len - clone_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short tail copy",
            ));
        }
    }
    dst_file.set_len(len)?;
    Ok(())
}

fn duplicate(
    src: std::os::windows::io::RawHandle,
    dst: std::os::windows::io::RawHandle,
    range: CloneRange,
) -> io::Result<()> {
    let data = DUPLICATE_EXTENTS_DATA {
        FileHandle: src as _,
        SourceFileOffset: range.offset as i64,
        TargetFileOffset: range.offset as i64,
        ByteCount: range.len as i64,
    };
    let mut returned: u32 = 0;
    // SAFETY: both handles are open for the call's duration; `data` and
    // `returned` outlive it; no output buffer.
    let ok = unsafe {
        DeviceIoControl(
            dst as _,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &data as *const DUPLICATE_EXTENTS_DATA as *const _,
            size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Clone a file of `len` bytes, or skip when this volume refuses.
    fn clone_or_skip(name: &str, len: usize) {
        let dir = TempDir::new(name);
        let src = dir.write("source", len);
        let dst = dir.path().join("clone");
        let original = fs::read(&src).unwrap();
        match clone_file(&src, &dst) {
            Ok(()) => {
                assert_eq!(fs::read(&dst).unwrap(), original, "{len} bytes");
                assert_eq!(fs::metadata(&dst).unwrap().len(), len as u64);
                // Independent file: writing the clone leaves the source alone.
                fs::write(&dst, b"changed").unwrap();
                assert_eq!(fs::read(&src).unwrap(), original);
            }
            Err(error) => {
                assert!(!dst.exists(), "failed clone left a destination: {error}");
                eprintln!("skipped: this volume does not block-clone {len} bytes: {error}");
            }
        }
    }

    #[test]
    fn clones_a_file_ending_on_a_cluster_or_skips() {
        clone_or_skip("wclone-aligned", 256 * 1024);
    }

    #[test]
    fn clones_a_file_with_a_partial_last_cluster_or_skips() {
        clone_or_skip("wclone-tail", 256 * 1024 + 123);
    }

    #[test]
    fn clones_a_file_smaller_than_a_cluster_or_skips() {
        clone_or_skip("wclone-small", 100);
    }

    #[test]
    fn clones_an_empty_file_or_skips() {
        clone_or_skip("wclone-empty", 0);
    }

    #[test]
    fn the_same_volume_is_named_alike_from_two_directories() {
        let dir = TempDir::new("wclone-volume");
        let a = dir.path().join("a");
        let b = dir.path().join("b").join("c");
        fs::create_dir_all(&b).unwrap();
        let (Some(left), Some(right)) = (volume_of(&a), volume_of(&b.join("x"))) else {
            eprintln!("skipped: the volume of the temp directory cannot be read");
            return;
        };
        assert_eq!(left.id, right.id);
    }

    #[test]
    fn a_refused_pair_fails_before_creating_the_destination() {
        let dir = TempDir::new("wclone-refused");
        let src = dir.write("source", 128 * 1024);
        // A missing parent: an attempt to create the destination would fail
        // with NotFound, so Unsupported shows that nothing was attempted.
        let dst = dir.path().join("missing").join("clone");
        let (Some(source), Some(destination)) = (volume_of(&src), volume_of(&dst)) else {
            eprintln!("skipped: the volume of the temp directory cannot be read");
            return;
        };
        let before = memo().get(&source.id, &destination.id);
        memo().set(&source.id, &destination.id, PairState::Refused(Some(1)));
        let result = clone_file(&src, &dst);
        match before {
            Some(state) => memo().set(&source.id, &destination.id, state),
            None => memo().forget(&source.id, &destination.id),
        }
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported, "{error}");
        assert!(!dst.exists());
        // An empty file still "clones" as before: it has nothing to share.
        let empty = dir.write("empty", 0);
        memo().set(&source.id, &destination.id, PairState::Refused(Some(1)));
        let empty_dst = dir.path().join("empty-clone");
        let result = clone_file(&empty, &empty_dst);
        match before {
            Some(state) => memo().set(&source.id, &destination.id, state),
            None => memo().forget(&source.id, &destination.id),
        }
        result.unwrap();
        assert_eq!(fs::metadata(&empty_dst).unwrap().len(), 0);
    }

    #[test]
    fn a_failed_clone_never_replaces_an_existing_destination() {
        let dir = TempDir::new("wclone-existing");
        let src = dir.write("source", 128 * 1024);
        let dst = dir.path().join("destination");
        fs::write(&dst, b"keep").unwrap();
        assert!(clone_file(&src, &dst).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"keep");
    }
}
