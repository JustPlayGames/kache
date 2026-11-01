//! What an admitted compile already holds of its expected peak.
//!
//! The memory gate counts a compile admitted in the last
//! [`super::ADMISSION_RAMP`] at a share of its expected peak, so a burst of
//! admissions does not all see the memory the first are about to take. Once
//! the compiler runs, the memory it holds already shows as used in the
//! available figure, and counting its share on top counted that memory
//! twice. The admission record therefore names the compiler process it
//! started ([`spawned`]), and the gate charges a running compile only what
//! of its peak it does not hold yet ([`charge`]).

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::Duration;
use std::time::SystemTime;

thread_local! {
    /// The admission record of the compile this thread holds a permit for.
    static CURRENT: RefCell<Option<Arc<Mutex<PathBuf>>>> = const { RefCell::new(None) };
}

/// The record at `path` stands for the compile this thread runs next.
pub(super) fn track(path: PathBuf) -> Arc<Mutex<PathBuf>> {
    let shared = Arc::new(Mutex::new(path));
    CURRENT.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&shared)));
    shared
}

/// The record at `path` is gone: this thread's next process is no compile
/// it stands for.
pub(super) fn forget(path: &Arc<Mutex<PathBuf>>) {
    CURRENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, path))
        {
            *slot = None;
        }
    });
}

/// A process this thread started for the compile it holds a permit for:
/// the record names it from now on (the latest one, the compiler, once the
/// key's probes are done). A rename keeps the record's age. Only Windows
/// reads what a process holds ([`held_by`]), so only it names them.
#[cfg(any(windows, test))]
pub(crate) fn spawned(pid: u32) {
    let Some(shared) = CURRENT.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let mut path = shared.lock().unwrap_or_else(|poison| poison.into_inner());
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let renamed = path.with_file_name(with_child(name, pid));
    if std::fs::rename(&*path, &renamed).is_ok() {
        *path = renamed;
    }
}

/// `name` naming `pid` as its compiler, in place of any it named before:
/// `<bytes>-<pid>-<counter>-c<child>`, then the measured suffix.
#[cfg(any(windows, test))]
pub(super) fn with_child(name: &str, pid: u32) -> String {
    let (stem, measured) = match name.strip_suffix(super::MEASURED_SUFFIX) {
        Some(stem) => (stem, super::MEASURED_SUFFIX),
        None => (name, ""),
    };
    let stem: Vec<&str> = stem
        .split('-')
        .filter(|segment| child_in(segment).is_none())
        .collect();
    format!("{}-c{pid}{measured}", stem.join("-"))
}

/// The compiler a record's name names.
pub(super) fn child_of(name: &str) -> Option<u32> {
    name.split('-').find_map(child_in)
}

fn child_in(segment: &str) -> Option<u32> {
    segment
        .strip_prefix('c')
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))?
        .parse()
        .ok()
}

/// What an admitted compile still counts for: its ramp `share` of the
/// `expected` peak, but never more than the part of the peak it does not
/// hold yet when it is known to hold `used` bytes.
pub(super) fn charge(expected: u64, share: u64, used: Option<u64>) -> u64 {
    match used {
        Some(used) => share.min(expected.saturating_sub(used)),
        None => share,
    }
}

/// The memory process `pid` holds now (its private bytes, what the
/// compile's measured peak counts too), when it is a process started at or
/// after `not_before`: an earlier one is another process under a reused
/// pid. `None` when unknown.
#[cfg(windows)]
pub(super) fn held_by(pid: u32, not_before: SystemTime) -> Option<u64> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: OpenProcess returned a handle this function owns.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    if unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    let started = from_filetime(created)?;
    // File times and process times tick differently; a second covers it.
    if started + Duration::from_secs(1) < not_before {
        return None;
    }
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    counters.cb = size;
    if unsafe {
        K32GetProcessMemoryInfo(
            process.as_raw_handle(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            size,
        )
    } == 0
    {
        return None;
    }
    u64::try_from(counters.PrivateUsage).ok()
}

/// Elsewhere the compile's share counts in full, as before.
#[cfg(not(windows))]
pub(super) fn held_by(_pid: u32, _not_before: SystemTime) -> Option<u64> {
    None
}

#[cfg(windows)]
fn from_filetime(time: windows_sys::Win32::Foundation::FILETIME) -> Option<SystemTime> {
    // 100 ns ticks since 1601-01-01; the Unix epoch is 11,644,473,600 s later.
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let unix = ticks.checked_sub(11_644_473_600 * 10_000_000)?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_nanos(unix.saturating_mul(100)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_names_its_latest_compiler_before_the_measured_suffix() {
        let measured = format!("4754288640-2076-10{}", super::super::MEASURED_SUFFIX);
        let named = with_child(&measured, 31896);
        assert_eq!(named, "4754288640-2076-10-c31896-m");
        assert_eq!(child_of(&named), Some(31896));
        assert_eq!(with_child(&named, 7), "4754288640-2076-10-c7-m");
        assert_eq!(with_child("536870912-9-0", 12), "536870912-9-0-c12");
        assert_eq!(child_of("536870912-9-0"), None);
        assert_eq!(
            child_of(&measured),
            None,
            "the measured suffix names nothing"
        );
    }

    #[test]
    fn a_running_compile_is_charged_only_the_peak_it_does_not_hold_yet() {
        const GB: u64 = 1_000_000_000;
        // Admitted 20 s into a 60 s ramp: two thirds of its 4.7 GB peak.
        let share = 4_700_000_000 / 3 * 2;
        assert_eq!(charge(4_700_000_000, share, None), share, "not started");
        assert_eq!(
            charge(4_700_000_000, share, Some(GB)),
            share,
            "the ramp bounds it"
        );
        assert_eq!(charge(4_700_000_000, share, Some(3 * GB)), 1_700_000_000);
        assert_eq!(
            charge(4_700_000_000, share, Some(5 * GB)),
            0,
            "past its peak"
        );
    }

    #[test]
    fn spawning_renames_this_threads_record_and_dropping_it_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1024-1-0-m");
        std::fs::write(&path, b"").unwrap();
        let shared = track(path);
        spawned(42);
        let named = dir.path().join("1024-1-0-c42-m");
        assert!(named.exists());
        assert_eq!(*shared.lock().unwrap(), named);
        forget(&shared);
        spawned(43);
        assert!(named.exists(), "a forgotten record is not renamed");
    }

    /// The gate's sum over the records: a record naming a running process
    /// is charged what of its peak that process lacks, an unnamed one its
    /// whole share.
    #[cfg(windows)]
    #[test]
    fn the_gate_charges_a_running_compiler_only_the_peak_it_lacks() {
        use std::os::windows::fs::FileTimesExt;
        const GIB: u64 = 1 << 30;
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("admitted");
        std::fs::create_dir_all(&records).unwrap();
        let me = std::process::id();
        let expected = held_by(me, SystemTime::UNIX_EPOCH).unwrap() + GIB;
        let named = records.join(format!("{expected}-1-0-c{me}-m"));
        let file = std::fs::File::create(&named).unwrap();
        // This process started before the record, as a compiler never does.
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        file.set_times(std::fs::FileTimes::new().set_created(created))
            .unwrap();
        drop(file);
        let pending = super::super::pending_bytes(dir.path(), SystemTime::now());
        // Other tests' threads move this process's memory a little.
        let slack = GIB / 4;
        assert!(pending.abs_diff(GIB) < slack, "{pending}");
        std::fs::write(records.join(format!("{expected}-1-1-m")), b"").unwrap();
        let both = super::super::pending_bytes(dir.path(), SystemTime::now());
        assert!(both.abs_diff(GIB + expected) < slack, "{both}");
    }

    #[cfg(windows)]
    #[test]
    fn this_process_holds_memory_and_a_later_start_is_another_process() {
        let me = std::process::id();
        let held = held_by(me, SystemTime::UNIX_EPOCH).unwrap();
        assert!(held > 1024 * 1024, "{held}");
        let later = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(held_by(me, later), None);
    }
}
