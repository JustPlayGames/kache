//! The platform-free half of Windows block cloning
//! (`FSCTL_DUPLICATE_EXTENTS_TO_FILE` on ReFS and Dev Drive): which ranges to
//! clone, which failures say "this volume pair never clones", and the
//! per-process memo of what each volume pair answered.
//!
//! The memo exists for volumes that cannot clone (NTFS, two volumes): without
//! it every restored or stored file there created its destination, asked the
//! volume for its cluster size, sized the file, failed the FSCTL and deleted
//! the destination again before the caller copied. Where cloning works,
//! nothing changes: the same ranges are cloned the same way.
//!
//! Everything here is pure so the rules are tested on every platform.

use std::collections::HashMap;

/// Microsoft's limit for one FSCTL: the cloned region must be less than 4 GiB.
pub(crate) const MAX_CLONE_RANGE: u64 = 4 << 30;

/// Win32 errors meaning the volume (or the pair of volumes) cannot block-clone
/// at all, so no file there ever will: NTFS answers `ERROR_INVALID_FUNCTION`,
/// a filter or older ReFS `ERROR_NOT_SUPPORTED`, and two volumes
/// `ERROR_NOT_SAME_DEVICE`.
const ERROR_INVALID_FUNCTION: i32 = 1;
const ERROR_NOT_SAME_DEVICE: i32 = 17;
const ERROR_NOT_SUPPORTED: i32 = 50;

/// Largest cluster multiple a single FSCTL may clone, or `None` for a cluster
/// size no range can use (zero, or not below 4 GiB).
pub(crate) fn chunk_limit(cluster: u64) -> Option<u64> {
    if cluster == 0 || cluster >= MAX_CLONE_RANGE {
        return None;
    }
    Some(((MAX_CLONE_RANGE - 1) / cluster) * cluster)
}

/// One FSCTL call: `len` bytes at the same offset in source and destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CloneRange {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

/// The cluster-aligned ranges that clone the whole clusters of a file of
/// `len` bytes, each below 4 GiB. The bytes after them (less than one
/// cluster) are copied. `None` when the cluster size is unusable.
pub(crate) fn aligned_ranges(len: u64, cluster: u64) -> Option<Vec<CloneRange>> {
    let limit = chunk_limit(cluster)?;
    let aligned_len = (len / cluster) * cluster;
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < aligned_len {
        let chunk = (aligned_len - offset).min(limit);
        ranges.push(CloneRange { offset, len: chunk });
        offset += chunk;
    }
    Some(ranges)
}

/// What one failed FSCTL says about the volume pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloneRefusal {
    /// The volume or the pair of volumes cannot block-clone: remember it.
    VolumePair,
    /// Only this file failed (alignment, a block shared by too many files
    /// (`ERROR_BLOCK_TOO_MANY_REFERENCES`), a full disk, a locked file...):
    /// copy it and keep cloning the others.
    ThisFile,
}

/// Classify a failed FSCTL from its Win32 error code.
pub(crate) fn classify_refusal(raw_os_error: Option<i32>) -> CloneRefusal {
    match raw_os_error {
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SAME_DEVICE | ERROR_NOT_SUPPORTED) => {
            CloneRefusal::VolumePair
        }
        _ => CloneRefusal::ThisFile,
    }
}

/// What this process knows about block cloning from one volume to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairState {
    /// The pair cannot clone: skip the attempt and copy. Holds the Win32 code
    /// that said so, if a clone said so rather than the volume's flags.
    Refused(Option<i32>),
    /// The destination volume clones with `cluster` byte clusters.
    Clones { cluster: u64 },
}

/// Most volume pairs one process remembers. A build restores from one store
/// into one or two target volumes; the bound only keeps a long-lived process
/// that walks many volumes from growing without limit.
const MAX_PAIRS: usize = 64;

/// Per-process memo of [`PairState`] by (source volume, destination volume).
#[derive(Debug, Default)]
pub(crate) struct PairMemo {
    pairs: HashMap<(String, String), PairState>,
}

impl PairMemo {
    pub(crate) fn get(&self, source: &str, destination: &str) -> Option<PairState> {
        self.pairs
            .get(&(source.to_owned(), destination.to_owned()))
            .copied()
    }

    pub(crate) fn set(&mut self, source: &str, destination: &str, state: PairState) {
        let key = (source.to_owned(), destination.to_owned());
        if self.pairs.len() >= MAX_PAIRS && !self.pairs.contains_key(&key) {
            self.pairs.clear();
        }
        self.pairs.insert(key, state);
    }

    /// Drop what is known about the pair (tests restore the memo with it).
    #[cfg(all(test, windows))]
    pub(crate) fn forget(&mut self, source: &str, destination: &str) {
        self.pairs
            .remove(&(source.to_owned(), destination.to_owned()));
    }

    /// Remember that a clone between the pair failed with `raw_os_error`.
    /// Only a [`CloneRefusal::VolumePair`] error changes what is known.
    pub(crate) fn record_failure(
        &mut self,
        source: &str,
        destination: &str,
        raw_os_error: Option<i32>,
    ) {
        if classify_refusal(raw_os_error) == CloneRefusal::VolumePair {
            self.set(source, destination, PairState::Refused(raw_os_error));
        }
    }
}

/// The state a pair starts in, from what the volumes say before any clone:
/// whether both paths are on one volume (`None` when a volume has no GUID
/// name to compare, such as a `subst` drive or a share), whether the
/// destination volume reports `FILE_SUPPORTS_BLOCK_REFCOUNTING`, and its
/// cluster size. `None` means unknown: try the clone as before and learn
/// from its error.
pub(crate) fn probe_state(
    same_volume: Option<bool>,
    block_refcounting: Option<bool>,
    cluster: Option<u64>,
) -> Option<PairState> {
    if same_volume == Some(false) {
        return Some(PairState::Refused(Some(ERROR_NOT_SAME_DEVICE)));
    }
    match (block_refcounting, cluster) {
        (Some(false), _) => Some(PairState::Refused(None)),
        (Some(true), Some(cluster))
            if same_volume == Some(true) && chunk_limit(cluster).is_some() =>
        {
            Some(PairState::Clones { cluster })
        }
        _ => None,
    }
}

/// Whether `value` turns block cloning off: `0`, `false`, `off` or `no`, in
/// any case. Anything else, and no value, leaves it on.
pub(crate) fn switch_turns_off(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        let value = value.trim();
        ["0", "false", "off", "no"]
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const K64: u64 = 64 * 1024;
    const K4: u64 = 4 * 1024;

    #[test]
    fn chunk_limit_is_the_largest_cluster_multiple_below_4_gib() {
        assert_eq!(chunk_limit(K4), Some(MAX_CLONE_RANGE - K4));
        assert_eq!(chunk_limit(K64), Some(MAX_CLONE_RANGE - K64));
        assert_eq!(chunk_limit(3), Some(MAX_CLONE_RANGE - 1));
        assert_eq!(chunk_limit(0), None);
        assert_eq!(chunk_limit(MAX_CLONE_RANGE), None);
        for cluster in [512, K4, K64, 2 * 1024 * 1024] {
            let limit = chunk_limit(cluster).unwrap();
            assert_eq!(limit % cluster, 0);
            assert!(limit < MAX_CLONE_RANGE);
            assert!(limit + cluster >= MAX_CLONE_RANGE);
        }
    }

    #[test]
    fn whole_clusters_are_cloned_and_the_tail_is_left_to_copy() {
        assert_eq!(
            aligned_ranges(3 * K64, K64).unwrap(),
            vec![CloneRange {
                offset: 0,
                len: 3 * K64
            }]
        );
        assert_eq!(
            aligned_ranges(2 * K4 + 123, K4).unwrap(),
            vec![CloneRange {
                offset: 0,
                len: 2 * K4
            }]
        );
        assert!(aligned_ranges(100, K64).unwrap().is_empty());
        assert!(aligned_ranges(0, K4).unwrap().is_empty());
        assert_eq!(aligned_ranges(10, 0), None);
        assert_eq!(aligned_ranges(10, MAX_CLONE_RANGE), None);
    }

    #[test]
    fn a_large_file_is_cut_into_aligned_ranges_below_4_gib() {
        let len = 9 * (1u64 << 30) + 5;
        let ranges = aligned_ranges(len, K64).unwrap();
        assert_eq!(ranges.len(), 3);
        let mut next = 0;
        for range in &ranges {
            assert_eq!(range.offset, next);
            assert_eq!(range.offset % K64, 0);
            assert_eq!(range.len % K64, 0);
            assert!(range.len < MAX_CLONE_RANGE);
            next += range.len;
        }
        assert_eq!(next, (len / K64) * K64);
    }

    #[test]
    fn only_volume_errors_refuse_the_pair() {
        for code in [1, 17, 50] {
            assert_eq!(classify_refusal(Some(code)), CloneRefusal::VolumePair);
        }
        // ERROR_INVALID_PARAMETER, ERROR_BLOCK_TOO_MANY_REFERENCES,
        // ERROR_DISK_FULL, ERROR_SHARING_VIOLATION, ERROR_ACCESS_DENIED.
        for code in [87, 347, 112, 32, 5] {
            assert_eq!(classify_refusal(Some(code)), CloneRefusal::ThisFile);
        }
        assert_eq!(classify_refusal(None), CloneRefusal::ThisFile);
    }

    #[test]
    fn the_memo_remembers_a_refused_pair_and_not_a_file_failure() {
        let mut memo = PairMemo::default();
        let clones = PairState::Clones { cluster: K4 };
        memo.set("A", "A", clones);
        memo.record_failure("A", "A", Some(347));
        assert_eq!(memo.get("A", "A"), Some(clones));
        memo.record_failure("A", "A", Some(1));
        assert_eq!(memo.get("A", "A"), Some(PairState::Refused(Some(1))));
        // Other pairs are untouched.
        assert_eq!(memo.get("A", "B"), None);
        assert_eq!(memo.get("B", "A"), None);
    }

    #[test]
    fn the_memo_stays_bounded() {
        let mut memo = PairMemo::default();
        for i in 0..(MAX_PAIRS * 3) {
            memo.set(&i.to_string(), "D", PairState::Refused(None));
            assert!(memo.pairs.len() <= MAX_PAIRS);
        }
        let last = (MAX_PAIRS * 3 - 1).to_string();
        assert_eq!(memo.get(&last, "D"), Some(PairState::Refused(None)));
    }

    #[test]
    fn the_probe_refuses_two_volumes_and_a_volume_without_refcounting() {
        assert_eq!(
            probe_state(Some(false), Some(true), Some(K4)),
            Some(PairState::Refused(Some(17)))
        );
        assert_eq!(
            probe_state(Some(true), Some(false), Some(K4)),
            Some(PairState::Refused(None))
        );
        assert_eq!(
            probe_state(None, Some(false), Some(K4)),
            Some(PairState::Refused(None))
        );
        assert_eq!(
            probe_state(Some(true), Some(true), Some(K64)),
            Some(PairState::Clones { cluster: K64 })
        );
        // Unknown volume, flags or cluster: decide nothing, let the clone
        // answer.
        assert_eq!(probe_state(None, Some(true), Some(K4)), None);
        assert_eq!(probe_state(Some(true), None, Some(K4)), None);
        assert_eq!(probe_state(Some(true), Some(true), None), None);
        assert_eq!(probe_state(Some(true), Some(true), Some(0)), None);
    }

    #[test]
    fn the_switch_turns_off_only_on_an_off_value() {
        for off in ["0", "false", "FALSE", "off", "No", " 0 "] {
            assert!(switch_turns_off(Some(off)), "{off}");
        }
        for on in ["1", "true", "on", "", "yes"] {
            assert!(!switch_turns_off(Some(on)), "{on}");
        }
        assert!(!switch_turns_off(None));
    }
}
