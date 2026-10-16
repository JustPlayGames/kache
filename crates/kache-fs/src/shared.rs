//! Cache directories shared by several Unix accounts through a group.
//!
//! A directory that is setgid and gives its group full access (`chmod 2770`
//! on a group every account is in) is a group store: each account must be able
//! to read, replace and remove what another one created there. Files kache
//! creates with a mode of their own would not be: temp files are `0600`,
//! SQLite creates `0644`, a copied blob keeps the build output's mode, and a
//! hardlinked one keeps the output's group. The creator widens them here: the
//! store's group, and the owner's permission bits copied to the group, so a
//! read-only blob stays read-only and a private directory becomes the group's.

use std::path::Path;

/// The group of `dir` when it is a group store directory (setgid, group rwx).
pub fn group_store_gid(dir: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(dir).ok()?;
        (meta.is_dir() && meta.mode() & 0o2070 == 0o2070).then(|| meta.gid())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Share `path` with the group when its parent is a group store directory.
/// Best-effort; does nothing for entries another account owns.
pub fn share_in_group_store(path: &Path) {
    if let Some(gid) = path.parent().and_then(group_store_gid) {
        share_with_group(path, gid);
    }
}

/// Give `path` the group `gid` and the owner's permission bits for the group.
/// Best-effort; does nothing for symlinks and entries another account owns.
pub fn share_with_group(path: &Path, gid: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        // SAFETY: geteuid has no arguments, pointers, or preconditions.
        if meta.file_type().is_symlink() || meta.uid() != unsafe { libc::geteuid() } {
            return;
        }
        if meta.gid() != gid {
            let _ = std::os::unix::fs::chown(path, None, Some(gid));
        }
        let special = if meta.is_dir() {
            meta.mode() & 0o7000
        } else {
            0
        };
        let mode = meta.mode() & 0o777;
        let wanted = special | mode | ((mode & 0o700) >> 3);
        if wanted != meta.mode() & 0o7777 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, gid);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn a_group_store_shares_owner_bits_with_the_group() {
        let dir = crate::testutil::TempDir::new("group-store");
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        assert_eq!(group_store_gid(&store), None);
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let gid = std::fs::metadata(&store).unwrap().gid();
        assert_eq!(group_store_gid(&store), Some(gid));

        for (before, after) in [
            (0o600, 0o660),
            (0o400, 0o440),
            (0o444, 0o444),
            (0o500, 0o550),
        ] {
            let file = store.join(format!("f{before:o}"));
            std::fs::write(&file, b"x").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(before)).unwrap();
            share_in_group_store(&file);
            assert_eq!(mode(&file), after, "{before:o}");
        }
        let private = store.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o2700)).unwrap();
        share_in_group_store(&private);
        assert_eq!(mode(&private), 0o2770);
    }

    #[test]
    fn outside_a_group_store_nothing_changes() {
        let dir = crate::testutil::TempDir::new("no-group-store");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        share_in_group_store(&file);
        assert_eq!(mode(&file), 0o600);
    }
}
