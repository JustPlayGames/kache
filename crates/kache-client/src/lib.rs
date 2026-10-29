//! Compiler invocation capture and synchronous handoff to a running Kache daemon.

#[cfg(any(unix, windows))]
mod execution;
#[cfg(any(unix, windows))]
pub use execution::Execution;

#[cfg(any(unix, windows))]
mod stdio;
#[cfg(any(unix, windows))]
pub use stdio::Stdio;
#[cfg(unix)]
mod creation_mask;
#[cfg(unix)]
pub use creation_mask::creation_mask;
mod argv_file;
mod frontend;
pub use frontend::run;

/// Internal argv envelope passed from the frontend to its companion engine.
pub const FORWARDED_ARGV_FLAG: &str = "--kache-forwarded-argv-v1";

/// Set on the engine a frontend forwards to after the running daemon
/// declined this compile. The engine then neither asks the daemon again nor
/// starts one: a busy daemon would decline again, costing every compile a
/// second round trip while it is busiest.
pub const FRONTEND_DECLINED_ENV: &str = "KACHE_FRONTEND_DECLINED";

/// Set on the engine a frontend forwards to after the daemon went away during
/// this compile, after Commit ([`kache_ipc::execution::Attempt::Lost`]). The
/// engine runs the compile once more on its own: it does not ask a daemon or
/// start one, and first removes what the lost attempt may have left half
/// written.
pub const FRONTEND_DAEMON_LOST_ENV: &str = "KACHE_FRONTEND_DAEMON_LOST";

mod entry;
pub use entry::{EngineEntry, engine_entry};

include!(concat!(env!("OUT_DIR"), "/commands.rs"));
