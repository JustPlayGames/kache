//! Adapt engine-owned streams to the shared synchronous client.

use super::InvocationContext;
use anyhow::Result;
pub(crate) use kache_ipc::execution::Attempt;
use std::path::Path;

#[cfg(all(test, unix))]
pub(crate) fn try_execute(
    socket: &Path,
    args: &[String],
    context: &InvocationContext,
) -> Result<Option<i32>> {
    try_execute_with_trust(socket, args, context, false)
}

#[cfg(all(test, unix))]
pub(crate) fn try_execute_with_trust(
    socket: &Path,
    args: &[String],
    context: &InvocationContext,
    trusted_pipeline: bool,
) -> Result<Option<i32>> {
    Ok(
        match try_execute_attempt(socket, args, context, trusted_pipeline)? {
            Attempt::Completed(exit) => Some(exit),
            Attempt::Declined | Attempt::Unavailable | Attempt::Lost => None,
        },
    )
}

/// [`try_execute_with_trust`], telling a daemon that went away during the
/// compile ([`Attempt::Lost`]) apart from one that never took it.
pub(crate) fn try_execute_attempt(
    socket: &Path,
    args: &[String],
    context: &InvocationContext,
    trusted_pipeline: bool,
) -> Result<Attempt> {
    #[cfg(unix)]
    let (files, terminal) = match context.io.transfer_files() {
        Ok(files) => files,
        Err(error) => {
            tracing::debug!(%error, "compiler streams cannot be transferred");
            return Ok(Attempt::Unavailable);
        }
    };
    #[cfg(windows)]
    let (files, terminal) = (Vec::new(), false);
    let executable = match crate::platform::frontend_executable() {
        Ok(path) => path,
        Err(error) => {
            tracing::debug!(%error, "compiler client executable cannot be resolved");
            return Ok(Attempt::Unavailable);
        }
    };
    let args = args
        .iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    kache_client::Execution {
        entry: kache_protocol::execution::ExecutionEntry::Compiler,
        args: &args,
        env: &context.env,
        started_at: context.started_at,
        executable: &executable,
        files: &files,
        terminal,
        client_version: crate::VERSION,
    }
    .try_execute_attempt(socket, trusted_pipeline)
}
