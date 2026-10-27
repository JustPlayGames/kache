//! Apply the application's delegation policy to shared token evidence.

use kunobi_daemon::local::security::SecurityContextComparison;

#[cfg(windows)]
pub(crate) fn matches_daemon(
    process: std::os::windows::io::BorrowedHandle<'_>,
    trusted_pipeline: bool,
) -> std::io::Result<bool> {
    kunobi_daemon::local::security::compare_with_current_process(process)
        .map(|comparison| permits_delegation(comparison, trusted_pipeline))
}

/// Upstream lets a context that differs only by Windows 11's per-process
/// nonce (`TSA://ProcUnique`) in through the trusted pipeline alone. This
/// fork has let that one difference in without it since tasty.47 (every
/// process, a child of the daemon's own included, has its own nonce), so
/// either rule passing is enough.
const FORK_ACCEPTS_PROCESS_NONCE: bool = true;

fn permits_delegation(comparison: SecurityContextComparison, trusted_pipeline: bool) -> bool {
    match comparison {
        SecurityContextComparison::Equivalent => true,
        SecurityContextComparison::ProcessIdentityDiffers => {
            trusted_pipeline || FORK_ACCEPTS_PROCESS_NONCE
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_process_identity_differences_pass_without_the_trust_opt_in() {
        for trusted in [false, true] {
            assert!(permits_delegation(
                SecurityContextComparison::Equivalent,
                trusted
            ));
            assert!(permits_delegation(
                SecurityContextComparison::ProcessIdentityDiffers,
                trusted
            ));
            assert!(!permits_delegation(
                SecurityContextComparison::Incompatible,
                trusted
            ));
        }
    }
}
