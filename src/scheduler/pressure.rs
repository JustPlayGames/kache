//! Whether the machine, or this process's cgroup, is short of memory now.
//!
//! The scheduler weighs a compile by the memory it used before, but that
//! cannot see what else is running: several agents' builds, an IDE, a
//! browser. When memory runs short, a compile it admits anyway can push the
//! machine into swap or the OOM killer. Under pressure, [`super::Scheduler`]
//! therefore asks for the whole pool for a compile: it waits for the other
//! compiles to finish, so they run one after another until pressure eases.
//! Slots running tests hold still count against that need as before, so a
//! test waiting on a compile cannot stall it.
//!
//! - macOS: the kernel's own verdict, `kern.memorystatus_vm_pressure_level`,
//!   at warning or critical.
//! - Linux: memory PSI, some task stalled on memory for at least
//!   [`PSI_SOME_AVG10`] percent of the last ten seconds. The cgroup's own
//!   `memory.pressure` is read when there is one, so a container sees its
//!   limit rather than the host's.
//! - Windows: no stall measure exists, so the memory the machine has left:
//!   physical memory available under [`WINDOWS_MIN_AVAILABLE_PERCENT`] of the
//!   total, or commit (physical memory and page file) under
//!   [`WINDOWS_MIN_COMMIT_PERCENT`] of its limit. Past either, Windows pages
//!   the desktop out for the compiles, or allocations start to fail. The
//!   commit charge alone is no reading: it counts memory programs reserved
//!   and never touch (a WSL2 VM, a browser, the compilers themselves), so a
//!   desktop with a third of its memory free routinely charges more than it
//!   has.
//! - Elsewhere, and when nothing can be read: no pressure.
//!
//! Pressure is the late signal. Before it, the scheduler's memory gate
//! ([`fits`]) admits a compile only while the memory available now, less
//! what compiles admitted in the last moments are still expected to take
//! and less the compile's own expected peak, stays above a reserve: the
//! larger of [`RESERVE_PERCENT`] of the memory and [`RESERVE_FLOOR`]. The
//! available memory is what the operating system reports, so it sees
//! everything else on the machine (a WSL2 VM, an IDE, a browser, another
//! user's builds), which slots sized from total memory never did. One
//! compile always runs: a compile that finds no other compile holding a
//! slot is admitted whatever the memory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long one reading of the pressure is reused. Admission polls every few
/// milliseconds; memory pressure changes over seconds.
const SAMPLE_FOR: Duration = Duration::from_millis(250);

/// Share of the last ten seconds some task spent stalled on memory, in
/// percent, from which Linux counts as under pressure.
#[cfg(any(test, target_os = "linux"))]
pub(crate) const PSI_SOME_AVG10: f64 = 10.0;

/// Physical memory available, in percent of the total, under which Windows
/// counts as under pressure: there it pages. The memory gate keeps a
/// desktop's measured compiles above it too ([`windows_measured_reserve_bytes`]).
pub(crate) const WINDOWS_MIN_AVAILABLE_PERCENT: u64 = 10;

/// Commit left (physical memory and page file), in percent of the commit
/// limit, under which Windows counts as under pressure.
#[cfg(any(test, windows))]
pub(crate) const WINDOWS_MIN_COMMIT_PERCENT: u64 = 5;

/// Share of the machine's memory, in percent, the memory gate keeps free.
pub(crate) const RESERVE_PERCENT: u64 = 20;

/// The least memory the gate keeps free, whatever the share: below this a
/// desktop pages out.
pub(crate) const RESERVE_FLOOR: u64 = 4 * 1024 * 1024 * 1024;

/// The memory the gate keeps free on a machine with `total` bytes.
pub(crate) fn reserve_bytes(total: u64) -> u64 {
    (total / 100 * RESERVE_PERCENT).max(RESERVE_FLOOR)
}

/// Share of the memory, in percent, the gate keeps free while every compile
/// it weighs has a measured peak.
pub(crate) const MEASURED_RESERVE_PERCENT: u64 = 5;

/// The least memory the gate keeps free while every peak is measured.
pub(crate) const MEASURED_RESERVE_FLOOR: u64 = 1024 * 1024 * 1024;

/// The memory the gate keeps free while every compile it weighs (the one
/// asking and every one still running) has a measured peak: then the
/// expected peaks are what the compiles take, and the reserve only covers
/// what the machine does besides. Never more than [`reserve_bytes`].
pub(crate) fn measured_reserve_bytes(total: u64) -> u64 {
    (total / 100 * MEASURED_RESERVE_PERCENT)
        .max(MEASURED_RESERVE_FLOOR)
        .min(reserve_bytes(total))
}

/// The memory the gate keeps free on a Windows desktop while every compile
/// it weighs has a measured peak: the available memory under which Windows
/// pages ([`WINDOWS_MIN_AVAILABLE_PERCENT`], the pressure reading's own
/// line), never under [`RESERVE_FLOOR`] and never more than
/// [`reserve_bytes`]. Windows counts the standby cache as available and
/// takes it back first; what other programs hold is not available already.
/// With the peaks measured, a fifth of the memory on top kept a busy
/// desktop's compiles serialised: a 63 GiB machine at 12 GiB available
/// paged nothing out (no page writes), yet the 12.6 GiB reserve admitted
/// one compile at a time there, and two or three at 22 GiB.
pub(crate) fn windows_measured_reserve_bytes(total: u64) -> u64 {
    (total / 100 * WINDOWS_MIN_AVAILABLE_PERCENT)
        .max(RESERVE_FLOOR)
        .min(reserve_bytes(total))
}

/// The memory a machine (or this process's cgroup) has, and has available
/// now, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemoryReading {
    pub(crate) available: u64,
    pub(crate) total: u64,
    /// Inside a cgroup's memory limit, the part of its usage that is cache
    /// it reclaims before paging anything in use: its inactive file cache
    /// and reclaimable slab. `None` outside one, where `available` already
    /// counts the cache, and where a desktop may need the full reserve.
    pub(crate) reclaimable: Option<u64>,
}

/// Whether a compile expected to peak at `expected` bytes fits now: the
/// available memory, less `pending` bytes compiles admitted moments ago
/// have yet to take and less `expected`, stays at or above the reserve.
///
/// Inside a cgroup's limit (a container: no desktop to keep responsive)
/// and with `measured` (every compile involved has a measured peak), the
/// reserve is [`measured_reserve_bytes`] and the cache the cgroup reclaims
/// first counts as available. A guessed peak can be several times too
/// small (a crate like `windows` takes about 5 GB against the 512 MiB
/// guessed), so while one is involved the full reserve stays, and so does
/// the cache counted as used: on an 8 GB cgroup counting the cache alone
/// let a cold build's compiles be OOM-killed. Outside a cgroup the reserve
/// is what keeps a desktop from paging out: on Windows, with `measured`,
/// [`windows_measured_reserve_bytes`]; otherwise the full one.
pub(crate) fn fits(reading: MemoryReading, pending: u64, expected: u64, measured: bool) -> bool {
    let (available, reserve) = budget(reading, measured, cfg!(windows));
    available.saturating_sub(pending).saturating_sub(expected) >= reserve
}

/// The memory [`fits`] counts as available and the reserve it keeps, on
/// Windows when `windows`.
fn budget(reading: MemoryReading, measured: bool, windows: bool) -> (u64, u64) {
    match reading.reclaimable {
        Some(cache) if measured => (
            reading.available.saturating_add(cache).min(reading.total),
            measured_reserve_bytes(reading.total),
        ),
        None if measured && windows => (
            reading.available,
            windows_measured_reserve_bytes(reading.total),
        ),
        _ => (reading.available, reserve_bytes(reading.total)),
    }
}

/// `[cache] scheduler_memory_pressure`, set once at wrapper startup.
static ENABLED: AtomicBool = AtomicBool::new(true);

#[cfg(test)]
pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

thread_local! {
    static INVOCATION_ENABLED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Scheduler policy for one synchronous request. Machine pressure samples may
/// be reused; a client's opt-out must remain local to its invocation.
pub(crate) struct InvocationPressureScope {
    previous: Option<bool>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl InvocationPressureScope {
    pub(crate) fn enter(enabled: bool) -> Self {
        Self {
            previous: INVOCATION_ENABLED.with(|slot| slot.replace(Some(enabled))),
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for InvocationPressureScope {
    fn drop(&mut self) {
        INVOCATION_ENABLED.with(|slot| slot.set(self.previous));
    }
}

// A pressure answer a test sets for its own thread, so other tests running
// at the same time are not affected.
#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn force(pressured: Option<bool>) {
    FORCED.with(|forced| forced.set(pressured));
}

// A memory reading a test sets for its own thread. While a test pins the
// pressure answer and sets no reading, the gate has no reading: slot-count
// fixtures do not depend on the host's memory.
#[cfg(test)]
thread_local! {
    static FORCED_MEMORY: std::cell::Cell<Option<MemoryReading>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn force_memory(reading: Option<MemoryReading>) {
    FORCED_MEMORY.with(|forced| forced.set(reading));
}

/// Whether the memory policy (pressure and the gate) is on for this
/// invocation: `[cache] scheduler_memory_pressure`.
pub(crate) fn enabled() -> bool {
    INVOCATION_ENABLED.with(|slot| {
        slot.get()
            .unwrap_or_else(|| ENABLED.load(Ordering::Relaxed))
    })
}

/// The memory reading the gate admits against, `None` when the policy is
/// off or nothing can be read. Reused for [`SAMPLE_FOR`] like pressure.
pub(crate) fn memory_now() -> Option<MemoryReading> {
    if !enabled() {
        return None;
    }
    #[cfg(test)]
    if let Some(reading) = FORCED_MEMORY.with(std::cell::Cell::get) {
        return Some(reading);
    }
    #[cfg(test)]
    if FORCED.with(std::cell::Cell::get).is_some() {
        return None;
    }
    thread_local! {
        static LAST_MEMORY: std::cell::Cell<Option<(Instant, Option<MemoryReading>)>> =
            const { std::cell::Cell::new(None) };
    }
    let now = Instant::now();
    LAST_MEMORY.with(|last| match last.get() {
        Some((at, reading)) if now.saturating_duration_since(at) < SAMPLE_FOR => reading,
        _ => {
            let reading = read_memory();
            last.set(Some((now, reading)));
            reading
        }
    })
}

/// Whether admission should hold back now.
pub(crate) fn now() -> bool {
    if !enabled() {
        return false;
    }
    #[cfg(test)]
    if let Some(pressured) = FORCED.with(std::cell::Cell::get) {
        return pressured;
    }
    sampled(Instant::now(), under_pressure)
}

thread_local! {
    static LAST: std::cell::Cell<Option<(Instant, bool)>> = const { std::cell::Cell::new(None) };
}

/// The reading taken within [`SAMPLE_FOR`] of `now`, else a new one from
/// `read`.
pub(crate) fn sampled(now: Instant, read: impl FnOnce() -> bool) -> bool {
    LAST.with(|last| match last.get() {
        Some((at, pressured)) if now.saturating_duration_since(at) < SAMPLE_FOR => pressured,
        _ => {
            let pressured = read();
            last.set(Some((now, pressured)));
            pressured
        }
    })
}

/// The weight to admit a compile with: under pressure, the whole pool, so it
/// waits for every other compile to finish.
pub(crate) fn weight(weight: u32, pool: u32, pressured: bool) -> u32 {
    if pressured { pool } else { weight }
}

/// Whether macOS's pressure level (1 normal, 2 warning, 4 critical) is
/// pressure.
#[cfg(any(test, target_os = "macos"))]
pub(crate) fn macos_pressured(level: i32) -> bool {
    level >= 2
}

/// Whether a PSI file's `some avg10` is at or over [`PSI_SOME_AVG10`].
/// `None` when the text has no such line.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn psi_pressured(text: &str) -> Option<bool> {
    let some = text.lines().find(|line| line.starts_with("some "))?;
    let avg10 = some
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()?;
    Some(avg10 >= PSI_SOME_AVG10)
}

#[cfg(target_os = "macos")]
fn under_pressure() -> bool {
    let mut level: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, and `level`/`size` describe a
    // buffer of exactly the size the kernel writes for this integer.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&raw mut level).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    rc == 0 && macos_pressured(level)
}

#[cfg(target_os = "linux")]
fn under_pressure() -> bool {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    static CGROUP: OnceLock<Option<PathBuf>> = OnceLock::new();
    let cgroup = CGROUP.get_or_init(|| {
        super::ResourceDomain::from_files(
            Path::new("/proc/self/cgroup"),
            Path::new("/proc/self/mountinfo"),
        )
        .map(|domain| domain.current.join("memory.pressure"))
    });
    pressured_from(cgroup.as_deref(), Path::new("/proc/pressure/memory"))
}

/// The cgroup's own PSI reading when `cgroup` names a readable one, else the
/// host's; no pressure when neither can be read.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn pressured_from(cgroup: Option<&std::path::Path>, host: &std::path::Path) -> bool {
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| psi_pressured(&text))
    };
    cgroup
        .and_then(read)
        .or_else(|| read(host))
        .unwrap_or(false)
}

/// Whether Windows' memory figures (`GlobalMemoryStatusEx`) are pressure:
/// physical memory available under [`WINDOWS_MIN_AVAILABLE_PERCENT`] of the
/// total, or commit left under [`WINDOWS_MIN_COMMIT_PERCENT`] of its limit.
/// Totals of zero are no reading, not pressure.
///
/// The commit charged is not compared with the physical memory. Commit is
/// what programs reserved, not what they use: Windows backs it with the page
/// file and needs physical pages only for what is touched, which the
/// available figure already counts. A desktop with a WSL2 VM, a browser and
/// a few dozen compilers charges more than its RAM with a third of it free;
/// counting that as pressure made every compile ask for the whole pool, so
/// a 32-slot pool ran one compile at a time (a miss waited 136 s for a
/// permit at the median against 0.8 s, 2026-10-31).
#[cfg(any(test, windows))]
pub(crate) fn windows_pressured(
    available_physical: u64,
    total_physical: u64,
    available_commit: u64,
    total_commit: u64,
) -> bool {
    let short = |available: u64, total: u64, percent: u64| {
        total > 0 && u128::from(available) * 100 < u128::from(total) * u128::from(percent)
    };
    short(available_physical, total_physical, WINDOWS_MIN_AVAILABLE_PERCENT)
        || short(available_commit, total_commit, WINDOWS_MIN_COMMIT_PERCENT)
}

#[cfg(windows)]
fn under_pressure() -> bool {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a zeroed MEMORYSTATUSEX with its length set is what the call
    // expects, and it writes only within that struct.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0)
        && windows_pressured(
            status.ullAvailPhys,
            status.ullTotalPhys,
            status.ullAvailPageFile,
            status.ullTotalPageFile,
        )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn under_pressure() -> bool {
    false
}

/// `MemAvailable:` and `MemTotal:` of `/proc/meminfo`, in bytes.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn meminfo_reading(text: &str) -> Option<MemoryReading> {
    let field = |name: &str| {
        let kib: u64 = text
            .lines()
            .find_map(|line| line.strip_prefix(name))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        kib.checked_mul(1024)
    };
    Some(MemoryReading {
        available: field("MemAvailable:")?,
        total: field("MemTotal:")?,
        reclaimable: None,
    })
}

/// The machine's reading bounded by a cgroup limit and its headroom: a
/// container's compiles fit in its own limit, not the host's memory.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn within_cgroup(
    machine: MemoryReading,
    limit: Option<u64>,
    headroom: Option<u64>,
    reclaimable: u64,
) -> MemoryReading {
    match (limit, headroom) {
        (Some(limit), Some(headroom)) => MemoryReading {
            available: machine.available.min(headroom),
            total: machine.total.min(limit),
            reclaimable: Some(reclaimable),
        },
        // A limit whose usage cannot be read: its size bounds the total,
        // and nothing is known about its use.
        (Some(limit), None) => MemoryReading {
            available: machine.available.min(limit),
            total: machine.total.min(limit),
            reclaimable: None,
        },
        _ => machine,
    }
}

#[cfg(target_os = "linux")]
fn read_memory() -> Option<MemoryReading> {
    use std::path::Path;
    use std::sync::OnceLock;
    static DOMAIN: OnceLock<Option<super::ResourceDomain>> = OnceLock::new();
    let machine = meminfo_reading(&std::fs::read_to_string("/proc/meminfo").ok()?)?;
    let domain = DOMAIN.get_or_init(|| {
        super::ResourceDomain::from_files(
            Path::new("/proc/self/cgroup"),
            Path::new("/proc/self/mountinfo"),
        )
    });
    Some(match domain.as_ref().map(super::ResourceDomain::snapshot) {
        Some(snapshot) => within_cgroup(
            machine,
            snapshot.memory_limit_bytes,
            snapshot.memory_headroom_bytes,
            snapshot.memory_reclaimable_bytes,
        ),
        None => machine,
    })
}

#[cfg(windows)]
fn read_memory() -> Option<MemoryReading> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: as in `under_pressure`.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0 && status.ullTotalPhys > 0).then_some(
        MemoryReading {
            available: status.ullAvailPhys,
            total: status.ullTotalPhys,
            reclaimable: None,
        },
    )
}

/// macOS keeps pressure to the kernel's verdict; the gate has no reading.
#[cfg(not(any(target_os = "linux", windows)))]
fn read_memory() -> Option<MemoryReading> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_clients_keep_their_pressure_policy_on_reused_workers() {
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            for enabled in [false, true] {
                let barrier = &barrier;
                threads.spawn(move || {
                    force(Some(true));
                    let mut observations = Vec::new();
                    for _ in 0..2 {
                        let _scope = InvocationPressureScope::enter(enabled);
                        barrier.wait();
                        let before = now();
                        let nested = std::panic::catch_unwind(|| {
                            let _child = InvocationPressureScope::enter(!enabled);
                            std::panic::panic_any(now());
                        })
                        .err()
                        .and_then(|payload| payload.downcast::<bool>().ok())
                        .map(|value| *value);
                        let after = now();
                        barrier.wait();
                        observations.push((before, nested, after));
                    }
                    for (before, nested, after) in observations {
                        assert_eq!(before, enabled);
                        assert_eq!(nested, Some(!enabled));
                        assert_eq!(after, enabled);
                    }
                    assert!(INVOCATION_ENABLED.with(|slot| slot.get().is_none()));
                    force(None);
                });
            }
        });
    }

    #[test]
    fn pressure_takes_the_whole_pool() {
        assert_eq!(weight(2, 8, false), 2);
        assert_eq!(weight(2, 8, true), 8);
        // An ask that is already the whole pool stays there either way.
        assert_eq!(weight(8, 8, false), 8);
        assert_eq!(weight(8, 8, true), 8);
        assert_eq!(weight(1, 1, false), 1);
        assert_eq!(weight(1, 1, true), 1);
    }

    #[test]
    fn a_forced_answer_replaces_the_machine_until_cleared() {
        let _lock = crate::test_support::process_state_test_lock();
        set_enabled(true);
        let live = under_pressure();
        force(Some(!live));
        assert_eq!(now(), !live, "the pin replaces whatever the machine says");
        force(Some(live));
        assert_eq!(now(), live);
        force(None);
        assert_eq!(now(), live, "clearing the pin reads the machine again");
    }

    #[test]
    fn reads_the_macos_level() {
        assert!(!macos_pressured(1));
        assert!(macos_pressured(2));
        assert!(macos_pressured(4));
        assert!(!macos_pressured(0));
    }

    #[test]
    fn reads_windows_memory_figures() {
        const GIB: u64 = 1 << 30;
        // 64 GiB of RAM, 96 GiB of commit: plenty left of both.
        assert!(!windows_pressured(20 * GIB, 64 * GIB, 50 * GIB, 96 * GIB));
        // Physical memory under a tenth of the total.
        assert!(windows_pressured(6 * GIB, 64 * GIB, 50 * GIB, 96 * GIB));
        assert!(!windows_pressured(7 * GIB, 64 * GIB, 50 * GIB, 96 * GIB));
        // Commit nearly spent while physical memory still looks free (a
        // small page file: what is charged is still under the RAM).
        assert!(windows_pressured(20 * GIB, 64 * GIB, 3 * GIB, 66 * GIB));
        assert!(!windows_pressured(20 * GIB, 64 * GIB, 4 * GIB, 66 * GIB));
        // No reading is no pressure; figures past 2^64 / 100 do not overflow.
        assert!(!windows_pressured(0, 0, 0, 0));
        assert!(!windows_pressured(u64::MAX / 2, u64::MAX, u64::MAX / 2, u64::MAX));
    }

    #[test]
    fn the_reserve_is_a_fifth_of_memory_and_never_under_4_gib() {
        const GIB: u64 = 1 << 30;
        assert_eq!(reserve_bytes(8 * GIB), 4 * GIB);
        assert_eq!(reserve_bytes(20 * GIB), 4 * GIB);
        assert_eq!(reserve_bytes(64 * GIB), 64 * GIB / 100 * 20);
        assert_eq!(reserve_bytes(196 * GIB), 196 * GIB / 100 * 20);
        assert_eq!(reserve_bytes(0), 4 * GIB);
    }

    #[test]
    fn a_compile_fits_while_the_reserve_stays_free() {
        const GIB: u64 = 1 << 30;
        let reading = MemoryReading {
            available: 10 * GIB,
            total: 16 * GIB,
            reclaimable: None,
        };
        assert!(fits(reading, 0, 6 * GIB, false));
        assert!(!fits(reading, 0, 6 * GIB + 1, false));
        assert!(fits(reading, 2 * GIB, 4 * GIB, false));
        assert!(!fits(reading, 2 * GIB, 5 * GIB, false));
        assert!(!fits(reading, u64::MAX, u64::MAX, false), "saturates");
    }

    #[test]
    fn a_windows_desktop_keeps_its_paging_line_free_for_measured_compiles() {
        const GIB: u64 = 1 << 30;
        const MB: u64 = 1_000_000;
        // The 63 GiB desktop of 2026-11-01 at 22 GiB available, two measured
        // 4.7 GB compiles admitted moments ago and a third asking.
        let total = 63 * GIB;
        let reading = MemoryReading {
            available: 22 * GIB,
            total,
            reclaimable: None,
        };
        let left = |(available, reserve): (u64, u64)| {
            available
                .saturating_sub(2 * 4_700 * MB)
                .saturating_sub(4_700 * MB)
                >= reserve
        };
        assert_eq!(windows_measured_reserve_bytes(total), total / 100 * 10);
        assert_eq!(budget(reading, true, true), (22 * GIB, total / 100 * 10));
        assert!(left(budget(reading, true, true)), "the third runs");
        assert!(!left(budget(reading, true, false)), "the full reserve elsewhere");
        assert!(!left(budget(reading, false, true)), "and for a guessed peak");
        // Never under the floor, never over the full reserve.
        assert_eq!(windows_measured_reserve_bytes(16 * GIB), RESERVE_FLOOR);
        assert_eq!(windows_measured_reserve_bytes(8 * GIB), reserve_bytes(8 * GIB));
        assert_eq!(windows_measured_reserve_bytes(0), reserve_bytes(0));
    }

    #[test]
    fn reads_meminfo_and_bounds_it_by_the_cgroup() {
        const KIB: u64 = 1024;
        let text = "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:    9000000 kB\n";
        let machine = meminfo_reading(text).unwrap();
        assert_eq!(
            machine,
            MemoryReading {
                available: 9_000_000 * KIB,
                total: 16_000_000 * KIB,
                reclaimable: None,
            }
        );
        assert_eq!(meminfo_reading("MemTotal: 1 kB\n"), None);
        let limit = 4_000_000 * KIB;
        assert_eq!(
            within_cgroup(machine, Some(limit), Some(KIB), KIB),
            MemoryReading {
                available: KIB,
                total: limit,
                reclaimable: Some(KIB),
            }
        );
        assert_eq!(
            within_cgroup(machine, Some(limit), None, KIB),
            MemoryReading {
                available: limit,
                total: limit,
                reclaimable: None,
            }
        );
        assert_eq!(within_cgroup(machine, None, None, KIB), machine);
    }

    #[test]
    fn windows_commit_charged_past_physical_memory_is_not_pressure() {
        const MIB: u64 = 1 << 20;
        // A 64 GiB desktop with a WSL2 VM and a build running (2026-11-01):
        // 22.0 GiB of 63.2 available, 74.2 GiB charged of a 155.2 GiB
        // commit limit. Nothing pages; reading it as pressure serialised
        // every compile on the machine.
        let (total, available) = (64_717 * MIB, 22_528 * MIB);
        let (limit, left) = (158_925 * MIB, 82_944 * MIB);
        assert!(limit - left > total, "charged past physical memory");
        assert!(!windows_pressured(available, total, left, limit));
        // The same machine pages once available memory runs out, and fails
        // allocations once commit does, whatever the charge.
        assert!(windows_pressured(6 * 1024 * MIB, total, left, limit));
        assert!(windows_pressured(available, total, 7 * 1024 * MIB, limit));
    }

    /// At the admission, that machine's reading leaves a compile its own
    /// weight of a 32-slot pool, where the reading as pressure made a
    /// 3-slot compile ask for all 32 (one compile at a time).
    #[test]
    fn a_compile_on_a_desktop_charged_past_its_memory_asks_its_own_weight() {
        const MIB: u64 = 1 << 20;
        let (total, available) = (64_717 * MIB, 22_528 * MIB);
        let (limit, left) = (158_925 * MIB, 82_944 * MIB);
        let pressured = windows_pressured(available, total, left, limit);
        assert_eq!(weight(3, 32, pressured), 3);
        let paging = windows_pressured(6 * 1024 * MIB, total, left, limit);
        assert_eq!(weight(3, 32, paging), 32);
    }

    #[test]
    fn reads_psi() {
        let calm = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(psi_pressured(calm), Some(false));
        let under = "some avg10=10.00 avg60=3.10 avg300=1.00 total=123\nfull avg10=4.00 avg60=0 avg300=0 total=9\n";
        assert_eq!(psi_pressured(under), Some(true));
        let just_below = "some avg10=9.99 avg60=50.00 avg300=50.00 total=1\n";
        assert_eq!(psi_pressured(just_below), Some(false));
        assert_eq!(psi_pressured("full avg10=90.00\n"), None);
        assert_eq!(psi_pressured("some avg60=90.00\n"), None);
        assert_eq!(psi_pressured("some avg10=lots\n"), None);
        assert_eq!(psi_pressured(""), None);
        assert_eq!(PSI_SOME_AVG10, 10.0);
    }

    #[test]
    fn prefers_the_cgroup_reading_then_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let calm = dir.path().join("calm");
        let busy = dir.path().join("busy");
        let bad = dir.path().join("bad");
        let missing = dir.path().join("missing");
        std::fs::write(&calm, "some avg10=0.00 avg60=0 avg300=0 total=0\n").unwrap();
        std::fs::write(&busy, "some avg10=50.00 avg60=0 avg300=0 total=0\n").unwrap();
        std::fs::write(&bad, "nothing useful\n").unwrap();
        assert!(
            pressured_from(Some(&busy), &calm),
            "the cgroup's own reading wins"
        );
        assert!(!pressured_from(Some(&calm), &busy), "even when calm");
        assert!(
            pressured_from(Some(&missing), &busy),
            "unreadable cgroup: host"
        );
        assert!(pressured_from(Some(&bad), &busy), "unusable cgroup: host");
        assert!(pressured_from(None, &busy));
        assert!(!pressured_from(None, &calm));
        assert!(
            !pressured_from(Some(&missing), &missing),
            "nothing readable: none"
        );
    }

    #[test]
    fn the_switch_turns_pressure_off() {
        let _guard = crate::test_support::process_state_test_lock();
        force(Some(true));
        set_enabled(false);
        assert!(!now());
        set_enabled(true);
        assert!(now());
        force(None);
    }

    #[test]
    fn a_reading_is_reused_for_a_while() {
        // Far past any reading an earlier test left on this thread.
        let start = Instant::now() + Duration::from_secs(3600);
        assert!(sampled(start, || true));
        assert!(sampled(start + Duration::from_millis(249), || false));
        assert!(!sampled(start + SAMPLE_FOR, || false));
        assert!(!sampled(start + SAMPLE_FOR, || panic!("reused")));
        assert_eq!(SAMPLE_FOR, Duration::from_millis(250));
    }
}
