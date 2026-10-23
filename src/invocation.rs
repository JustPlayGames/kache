#[cfg(any(unix, windows))]
pub(crate) mod execution;
pub(crate) mod io;
pub(crate) mod process;
#[cfg(unix)]
pub(crate) mod unix_broker;
#[cfg(windows)]
pub(crate) mod windows_broker;
mod workspace;

/// Inputs and timing transferred to one synchronous compiler request.
pub(crate) struct InvocationContext {
    /// The existing daemon owns background work started by this invocation.
    pub(crate) daemon: Option<std::sync::Arc<crate::daemon::Daemon>>,
    pub(crate) env: InvocationEnv,
    pub(crate) started_at: std::time::Instant,
    pub(crate) shim_stem: String,
    pub(crate) io: io::InvocationIo,
    pub(crate) cancellation: Option<process::Cancellation>,
    #[cfg(unix)]
    pub(crate) jobserver: kache_ipc::jobserver::Descriptors,
}

/// Keep a store-key waiter inside the caller's cancellation lifetime.
pub(crate) fn wait_for_committed(
    store: &crate::store::Store,
    key: &str,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<bool> {
    store.wait_for_committed_with(key, timeout, || Ok(process::check()?))
}

pub(crate) use kache_config::InvocationEnv;

#[cfg(unix)]
thread_local! {
    /// The client process a daemon-run compiler request serves, set for the
    /// request's worker thread ([`ClientScope`]). `None` when the pipeline
    /// runs in the client's own process (the local engine).
    static CLIENT_PID: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Name the client process the current thread's compiler request serves.
/// The previous value comes back when the scope ends, so a reused worker
/// thread never answers for an earlier client.
#[cfg(unix)]
pub(crate) struct ClientScope {
    previous: Option<u32>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(unix)]
impl ClientScope {
    pub(crate) fn enter(client_pid: u32) -> Self {
        Self {
            previous: CLIENT_PID.with(|slot| slot.replace(Some(client_pid))),
            _thread: std::marker::PhantomData,
        }
    }
}

#[cfg(unix)]
impl Drop for ClientScope {
    fn drop(&mut self) {
        CLIENT_PID.with(|slot| slot.set(self.previous));
    }
}

/// The process that ran this compiler invocation: Cargo for a unit, when
/// Cargo runs Kache as its wrapper. A request the daemon runs for a client
/// names the client's parent, not the daemon's; the local engine names its
/// own. `None` when that process is gone, or for init.
#[cfg(target_os = "linux")]
pub(crate) fn cargo_pid() -> Option<u32> {
    let pid = match CLIENT_PID.with(std::cell::Cell::get) {
        Some(client) => parent_of(client)?,
        None => std::os::unix::process::parent_id(),
    };
    (pid > 1).then_some(pid)
}

/// The parent of `pid`, from field 4 of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parent_from_stat(&stat)
}

/// The command name sits in parentheses and may hold spaces and parentheses
/// itself; the fields after it never do.
#[cfg(any(target_os = "linux", test))]
fn parent_from_stat(stat: &str) -> Option<u32> {
    let (_, after) = stat.rsplit_once(')')?;
    after.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_is_read_after_the_command_name() {
        assert_eq!(parent_from_stat("42 (cargo) S 7 42 42 0"), Some(7));
        assert_eq!(parent_from_stat("42 (a) b (c) R 9 1"), Some(9));
        assert_eq!(parent_from_stat("42 (cargo"), None);
    }

    /// A daemon-run request names its client's parent; outside a request
    /// (or after it) this process's own parent is named again.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_client_scope_names_the_clients_parent() {
        let own = std::os::unix::process::parent_id();
        assert_eq!(cargo_pid(), (own > 1).then_some(own));
        {
            let _client = ClientScope::enter(std::process::id());
            assert_eq!(cargo_pid(), (own > 1).then_some(own));
            let child = std::process::Command::new("sleep")
                .arg("5")
                .spawn()
                .unwrap();
            let _inner = ClientScope::enter(child.id());
            assert_eq!(cargo_pid(), Some(std::process::id()));
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_eq!(cargo_pid(), (own > 1).then_some(own));
    }
}
