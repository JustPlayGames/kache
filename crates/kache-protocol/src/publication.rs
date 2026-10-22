use crate::BuildEvent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HandoffFile {
    /// Absolute path of the snapshot, under the store's handoff directory.
    pub path: String,
    /// Name the file takes in the entry.
    pub store_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublishCcRequest {
    /// Client executable mtime. Daemons from 1.0.x and earlier drain when it
    /// is newer than their own; later daemons ignore it.
    #[serde(default)]
    pub client_epoch: u64,
    /// Release requesting an upgrade. Absent from legacy and read-only requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    pub cache_key: String,
    pub crate_name: String,
    pub target: String,
    pub files: Vec<HandoffFile>,
    pub stdout: String,
    pub stderr: String,
    pub compile_time_ms: u64,
    /// Whether the entry may be uploaded to a writable remote.
    pub publishes_to_remote: bool,
    /// The build event the wrapper would have logged, with the store fields
    /// still empty. The daemon fills them in and writes it.
    pub event: BuildEvent,
    /// The read-set memo of a deferred compile, recorded here instead of in
    /// the wrapper: a hundred-row write transaction the next compile of the
    /// build script would otherwise wait for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<CcMemoHandoff>,
    /// What only a rustc compile carries. Present exactly on a
    /// `PublishRustc` hand-off: a daemon that does not know that request
    /// refuses it, so a wrapper never has a rustc compile stored as a C/C++
    /// object without its digest checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rustc: Option<RustcPublish>,
}

/// The rest of a rustc compile's store: what `Store::put_handed_off` and
/// the bookkeeping after a synchronous rustc put need.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RustcPublish {
    pub crate_types: Vec<String>,
    pub features: Vec<String>,
    pub profile: String,
    /// One per [`PublishCcRequest::files`] entry, in the same order.
    pub outputs: Vec<RustcOutput>,
    /// Cargo's `-C metadata` unit, recorded with the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Large executables to record for prestaging once stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prestage: Vec<PrestageHint>,
    /// The wrapper staged and published the entry's blobs itself and only
    /// the index was too busy to register it: the files are links to those
    /// blobs, registered in a batch (`Store::register_handed_off_batch`)
    /// without staging them again. `None`: the files are snapshots of the
    /// outputs, stored with `Store::put_handed_off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedPublish>,
}

/// A rustc entry the wrapper staged ([`RustcPublish::staged`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StagedPublish {
    /// The entry's files as the wrapper staged them, one per
    /// [`PublishCcRequest::files`] entry, in the same order.
    pub files: Vec<kache_format::CachedFile>,
    /// What the wrapper's staging found (`StorePutResult`).
    pub output_blobs: u32,
    pub duplicate_blobs: u32,
    pub new_blobs: u32,
}

/// One snapshotted rustc output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RustcOutput {
    /// The digest the wrapper read from the snapshot before it returned to
    /// Cargo: the bytes the compiler wrote. The put refuses a snapshot that
    /// no longer hashes to it.
    pub hash: String,
    /// The compiler's output the snapshot links, whose file-hash memo is
    /// seeded once the entry is stored. `None` for a file of the wrapper's
    /// own (normalized dep-info, a debug bundle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The wrapper re-dated this check unit's `.rmeta` as Cargo is about to.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redated: bool,
}

/// A large executable the next build may want staged ahead of its restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrestageHint {
    pub target_dir: String,
    pub dest: String,
    pub store_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CcMemoHandoff {
    pub memo_key: String,
    pub preprocessed_hash: String,
    pub inputs: Vec<CcPreprocessMemoInput>,
}

/// A compiler input and the filesystem identity observed by its wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CcPreprocessMemoInput {
    pub name: String,
    #[serde(flatten)]
    pub fingerprint: FileFingerprint,
    pub content: String,
    #[serde(default)]
    pub mapped: String,
    #[serde(default)]
    pub observed_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: i64,
}
