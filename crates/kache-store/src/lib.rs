//! Local artifact storage, independent of compiler parsing and remote transports.

pub mod atomic;
mod blob_drift;
mod blob_validation;
mod cc_memo;
pub mod config;
pub mod eviction;
mod eviction_evidence;
pub mod file_hash;
pub mod filesystem;
mod index_compaction;
pub mod link;
pub mod markers;
pub mod opcounts;
pub mod sharing;
mod store;
#[cfg(test)]
mod test_support;
#[cfg(target_os = "linux")]
pub mod workspace;

pub use blob_drift::{BlobRefcountDrift, blob_refcount_drift};
pub use eviction_evidence::{EvictionEvidenceCohort, EvictionHorizonEvidence};
pub use store::*;

/// Compiler-owned rules applied when publishing an entry. Implementations must
/// permit shared inodes only for outputs that remain immutable after publication.
pub trait ArtifactPolicy {
    fn allow_hardlink(name: &str) -> bool;
    fn allow_empty(name: &str, output_types: &[String]) -> bool;
    fn emit_kind(name: &str) -> Option<&'static str>;
    fn stable_after_store(name: &str) -> bool;
    /// Whether the executable `name` of an entry compiled as `crate_types` may
    /// be stored by hardlink where the filesystem has no reflink, sharing the
    /// read-only inode with the build output. Only an executable nothing
    /// rewrites in place qualifies. Such a blob is content-checked on a hit
    /// unless the file-hash memo vouches for its current fingerprint
    /// ([`ArtifactStore::get`]): a writer that ignores the read-only bit (root,
    /// or anyone after a chmod) could reach it through the output.
    fn allow_executable_hardlink(_name: &str, _crate_types: &[String]) -> bool {
        false
    }
}
