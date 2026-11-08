//! Lane timers (KACHE-NEXT66-1109; not for a release): opt-in, in-process marks around a
//! compile's hand-off, written next to the phase traces (`KACHE_PHASE_TRACE_DIR`) as one
//! small JSON file per process (frontend) or per handed-over compile (daemon).
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static ON: AtomicBool = AtomicBool::new(false);
static MARKS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());

/// Turn the process-wide marks on (the frontend, when the trace directory is set).
pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

/// Whether the process-wide marks are on.
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Record a process-wide mark when on.
pub fn mark(name: &'static str) {
    if enabled()
        && let Ok(mut marks) = MARKS.lock()
    {
        marks.push((name, Instant::now()));
    }
}

/// The process-wide marks so far.
pub fn take() -> Vec<(&'static str, Instant)> {
    MARKS.lock().map(|mut marks| std::mem::take(&mut *marks)).unwrap_or_default()
}

/// Write `marks` (relative to `base`) as `<dir>/<file>.json`.
pub fn write(
    dir: &std::path::Path,
    file: &str,
    pid: u32,
    base: Instant,
    marks: &[(&'static str, Instant)],
    extra: &str,
) {
    let now = Instant::now();
    let epoch_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_micros());
    let base_epoch = epoch_now.saturating_sub(now.saturating_duration_since(base).as_micros());
    let mut out = format!("{{\"pid\":{pid},\"epoch_us\":{base_epoch}{extra},\"marks\":[");
    for (index, (name, at)) in marks.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "[\"{name}\",{}]",
            at.saturating_duration_since(base).as_micros()
        ));
    }
    out.push_str(&format!("],\"written_us\":{}}}\n", now.saturating_duration_since(base).as_micros()));
    let _ = std::fs::write(dir.join(format!("{file}.json")), out);
}
