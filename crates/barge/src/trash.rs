//! Deleting only what Barge owns.
//!
//! Deletion never follows a symlink and never descends into another filesystem,
//! so a nested mount or a link planted in a root is skipped rather than followed.
//! A trash entry is deleted only when its marker still names the area.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::root::{read_marker, MarkerRead, MARKER_NAME};

/// The deepest tree deletion descends before it gives up on an entry.
pub const MAX_DEPTH: usize = 128;

/// What one tree deletion removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deletion {
    pub bytes: u64,
    /// Entries left in place: another filesystem, or a tree that is too deep.
    pub skipped: Vec<PathBuf>,
}

/// Delete a tree Barge owns. `area` is the `<namespace>/<area>` the marker must
/// name; anything else is refused and nothing is removed.
pub(crate) fn remove_owned_tree(path: &Path, area: &str) -> Result<Deletion> {
    remove_owned_tree_with(path, area, &device_of)
}

/// The testable core of [`remove_owned_tree`]: `device` answers which filesystem
/// an entry is on, so a test can claim a nested folder is another mount.
pub(crate) fn remove_owned_tree_with(
    path: &Path,
    area: &str,
    device: &dyn Fn(&fs::Metadata) -> u64,
) -> Result<Deletion> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| Error::io("cannot inspect", path, &source))?;
    if !metadata.is_dir() {
        // A sticky symlink or a file where a root folder should be: remove the
        // entry itself, never what it points at.
        fs::remove_file(path).map_err(|source| Error::io("cannot remove", path, &source))?;
        return Ok(Deletion::default());
    }
    match read_marker(path) {
        MarkerRead::Valid(marker) if marker.area == area => {}
        MarkerRead::Valid(marker) => {
            return Err(Error::refused(format!(
                "refusing to delete {}: its marker belongs to area {}",
                path.display(),
                marker.area
            )))
        }
        MarkerRead::Missing => {
            return Err(Error::refused(format!(
                "refusing to delete {}: it has no {MARKER_NAME}",
                path.display()
            )))
        }
        MarkerRead::Invalid(reason) => {
            return Err(Error::refused(format!(
                "refusing to delete {}: {reason}",
                path.display()
            )))
        }
    }
    let mut deletion = Deletion::default();
    let root_device = device(&metadata);
    if remove_contents(path, root_device, device, 0, &mut deletion)? {
        fs::remove_dir(path).map_err(|source| Error::io("cannot remove", path, &source))?;
    } else {
        // A nested mount or an over-deep tree stays: Barge never reaches across.
        deletion.skipped.push(path.to_path_buf());
    }
    Ok(deletion)
}

fn remove_contents(
    directory: &Path,
    root_device: u64,
    device: &dyn Fn(&fs::Metadata) -> u64,
    depth: usize,
    deletion: &mut Deletion,
) -> Result<bool> {
    if depth >= MAX_DEPTH {
        deletion.skipped.push(directory.to_path_buf());
        return Ok(false);
    }
    let mut empty = true;
    let entries =
        fs::read_dir(directory).map_err(|source| Error::io("cannot list", directory, &source))?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::io("cannot list", directory, &source))?;
        let child = entry.path();
        let metadata = match fs::symlink_metadata(&child) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::io("cannot inspect", &child, &source)),
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            fs::remove_file(&child)
                .map_err(|source| Error::io("cannot remove a symlink", &child, &source))?;
            continue;
        }
        if file_type.is_dir() {
            if device(&metadata) != root_device {
                deletion.skipped.push(child);
                empty = false;
                continue;
            }
            if remove_contents(&child, root_device, device, depth + 1, deletion)? {
                fs::remove_dir(&child)
                    .map_err(|source| Error::io("cannot remove", &child, &source))?;
            } else {
                empty = false;
            }
        } else {
            deletion.bytes = deletion.bytes.saturating_add(metadata.len());
            fs::remove_file(&child)
                .map_err(|source| Error::io("cannot remove", &child, &source))?;
        }
    }
    Ok(empty)
}

fn device_of(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_symlink_is_removed_but_never_followed() {
        let temp = crate::testing::TempDir::new("trash-symlink").unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        fs::create_dir(&root).unwrap();
        crate::root::write_marker(
            &root,
            &crate::root::RootMarker {
                schema: crate::root::ROOT_SCHEMA.to_string(),
                area: "test/area".to_string(),
                id: "0123456789abcdef".to_string(),
                label: "slot".to_string(),
                created_unix_ms: 1,
                expires_unix_ms: None,
            },
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        fs::write(root.join("file.txt"), b"data").unwrap();

        let deletion = remove_owned_tree(&root, "test/area").unwrap();

        assert!(deletion.bytes >= 4, "the file's bytes are counted");
        assert!(!root.exists());
        assert!(
            outside.join("keep.txt").exists(),
            "the link target survives"
        );
    }

    #[test]
    fn a_nested_filesystem_is_skipped() {
        use std::os::unix::fs::MetadataExt;
        let temp = crate::testing::TempDir::new("trash-device").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        crate::root::write_marker(
            &root,
            &crate::root::RootMarker {
                schema: crate::root::ROOT_SCHEMA.to_string(),
                area: "test/area".to_string(),
                id: "0123456789abcdef".to_string(),
                label: "slot".to_string(),
                created_unix_ms: 1,
                expires_unix_ms: None,
            },
        )
        .unwrap();
        fs::write(nested.join("keep.txt"), b"keep").unwrap();
        let nested_inode = fs::symlink_metadata(&nested).unwrap().ino();
        let root_device = fs::symlink_metadata(&root).unwrap().dev();

        let deletion = remove_owned_tree_with(&root, "test/area", &|metadata| {
            if metadata.is_dir() && metadata.ino() == nested_inode {
                root_device + 1
            } else {
                root_device
            }
        })
        .unwrap();

        assert!(
            nested.join("keep.txt").exists(),
            "the nested mount survives"
        );
        assert!(deletion.skipped.contains(&nested));
        assert!(deletion.skipped.contains(&root));
    }

    #[test]
    fn a_foreign_marker_is_refused() {
        let temp = crate::testing::TempDir::new("trash-foreign").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        fs::create_dir(&root).unwrap();
        crate::root::write_marker(
            &root,
            &crate::root::RootMarker {
                schema: crate::root::ROOT_SCHEMA.to_string(),
                area: "other/area".to_string(),
                id: "0123456789abcdef".to_string(),
                label: "slot".to_string(),
                created_unix_ms: 1,
                expires_unix_ms: None,
            },
        )
        .unwrap();

        let error = remove_owned_tree(&root, "test/area").unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
        assert!(root.exists());
    }
}
