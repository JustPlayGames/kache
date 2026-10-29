//! Start a cold daemon while retaining compiler creation in the frontend.

use crate::Execution;
use anyhow::Result;
use kache_ipc::execution::Attempt;
use std::path::Path;
use std::process::Stdio;

/// Start a daemon and hand it the compile: its answer, or
/// [`Attempt::Unavailable`] when none was started.
pub(super) fn try_bootstrap(
    execution: &Execution<'_>,
    socket: &Path,
    disabled: bool,
    trusted_pipeline: bool,
) -> Result<Attempt> {
    let bootstrap = ["kache".into(), "daemon".into(), "start".into()];
    if !disabled
        && let Ok(mut command) = super::engine_command(
            execution.executable,
            execution.client_version,
            &bootstrap,
            execution.started_at,
        )
        && command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    {
        execution.try_execute_attempt(socket, trusted_pipeline)
    } else {
        Ok(Attempt::Unavailable)
    }
}
