//! Stopping a compile only when nothing it started can outlive it: see
//! [`super::stop_compile`].

use std::process::Child;
use std::time::{Duration, Instant};

/// How long a compiler may take to stop (a thread in uninterruptible
/// IO stops when that IO ends) before it is let go on instead.
const STOP_DEADLINE: Duration = Duration::from_millis(250);

pub(super) fn stop_if_alone(child: &mut Child) -> Result<(), &'static str> {
    if !matches!(child.try_wait(), Ok(None)) {
        return Err("the compiler has exited");
    }
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        return Err("unusable pid");
    };
    // SAFETY: `pid` is this process's own child, not yet reaped (above,
    // and only this thread reaps it while it holds the child), so it
    // names no other process.
    if unsafe { libc::kill(pid, libc::SIGSTOP) } != 0 {
        return Err("SIGSTOP failed");
    }
    let verdict = alone_before_codegen(pid);
    // SAFETY: as above.
    unsafe {
        libc::kill(
            pid,
            if verdict.is_ok() {
                libc::SIGKILL
            } else {
                libc::SIGCONT
            },
        );
    }
    verdict
}

/// Wait for every thread of `pid` to be stopped, then check that none
/// has a child or is a codegen thread.
fn alone_before_codegen(pid: libc::pid_t) -> Result<(), &'static str> {
    let task_dir = format!("/proc/{pid}/task");
    let tasks = frozen_tasks(&task_dir)?;
    for tid in &tasks {
        let children = std::fs::read_to_string(format!("{task_dir}/{tid}/children"))
            .map_err(|_| "its children are unreadable")?;
        if !children.trim().is_empty() {
            return Err("it runs a child process (its linker)");
        }
        let comm = std::fs::read_to_string(format!("{task_dir}/{tid}/comm"))
            .map_err(|_| "its threads are unreadable")?;
        if is_codegen_thread(comm.trim_end_matches('\n')) {
            return Err("its codegen has started");
        }
    }
    Ok(())
}

/// The process's threads once all are stopped by a signal ('T'; a
/// traced one, 't', can be resumed by its tracer) and the set read twice
/// in a row is the same: a thread created after the first read was
/// created by one still running then, which the second read catches.
fn frozen_tasks(task_dir: &str) -> Result<Vec<String>, &'static str> {
    let started = Instant::now();
    let mut pause = Duration::from_micros(50);
    loop {
        let first = tasks(task_dir)?;
        let mut stopped = true;
        for tid in &first {
            match task_state(task_dir, tid)? {
                'T' => {}
                'Z' | 'X' | 'x' => return Err("the compiler is exiting"),
                _ => {
                    stopped = false;
                    break;
                }
            }
        }
        if stopped && tasks(task_dir)? == first {
            return Ok(first);
        }
        if started.elapsed() >= STOP_DEADLINE {
            return Err("it did not stop in time");
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(2));
    }
}

fn tasks(task_dir: &str) -> Result<Vec<String>, &'static str> {
    let mut tasks = std::fs::read_dir(task_dir)
        .map_err(|_| "its threads are unreadable")?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect::<Vec<_>>();
    tasks.sort_unstable();
    if tasks.is_empty() {
        return Err("the compiler is exiting");
    }
    Ok(tasks)
}

/// The state letter of `/proc/<pid>/task/<tid>/stat`, which follows the
/// command name's closing parenthesis (the name may hold any byte).
fn task_state(task_dir: &str, tid: &str) -> Result<char, &'static str> {
    let stat =
        std::fs::read(format!("{task_dir}/{tid}/stat")).map_err(|_| "the compiler is exiting")?;
    parse_state(&stat).ok_or("its thread state is unreadable")
}

pub(super) fn parse_state(stat: &[u8]) -> Option<char> {
    let close = stat.iter().rposition(|&byte| byte == b')')?;
    stat.get(close + 2).map(|&byte| byte as char)
}

/// rustc's codegen threads: the coordinator, and the LLVM workers it
/// names after their work ("opt cgu.00", "lto ...", "copy ...").
pub(super) fn is_codegen_thread(comm: &str) -> bool {
    comm == "coordinator"
        || ["opt ", "lto ", "copy "]
            .iter()
            .any(|prefix| comm.starts_with(prefix))
}
