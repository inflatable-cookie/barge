//! Deleting only what Barge owns.
//!
//! Deletion never follows a symlink and never descends into another filesystem,
//! so a nested mount or a link planted in a root is skipped rather than followed.
//! A trash entry is deleted only when its marker still names the area, and the
//! marker is read through the descriptor the deletion then walks: the folder
//! whose ownership was checked is the folder that is removed.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::os::{Removed, Tree};
use crate::root::{read_marker_in, MarkerRead, MARKER_NAME};

/// What one tree deletion removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deletion {
    pub bytes: u64,
    /// Entries left in place: another filesystem, a tree that is too deep, or a
    /// folder a racing writer replaced at the path.
    pub skipped: Vec<PathBuf>,
}

/// Delete a tree Barge owns. `area` is the `<namespace>/<area>` the marker must
/// name; anything else is refused and nothing is removed.
pub(crate) fn remove_owned_tree(path: &Path, area: &str) -> Result<Deletion> {
    remove_owned_tree_checked(path, area, None)
}

pub(crate) fn remove_owned_tree_for_id(path: &Path, area: &str, id: &str) -> Result<Deletion> {
    remove_owned_tree_checked(path, area, Some(id))
}

fn remove_owned_tree_checked(path: &Path, area: &str, id: Option<&str>) -> Result<Deletion> {
    // One descriptor for the ownership check and for the walk, so a replacement
    // at the path between the two is never walked.
    let tree = Tree::open(path)?;
    #[cfg(test)]
    OPEN_TREE_HOOK.with(|hook| {
        let callback = hook.borrow_mut().take();
        if let Some(hook) = callback {
            hook();
        }
    });
    match read_marker_in(tree.directory(), tree.path()) {
        MarkerRead::Valid(marker) if marker.area == area && id.is_none_or(|id| marker.id == id) => {
        }
        MarkerRead::Valid(marker) => {
            return Err(Error::refused(format!(
                "refusing to delete {}: its marker does not match area {} or the expected root ID",
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
    let Removed {
        bytes,
        mut skipped,
        removed,
    } = tree.remove()?;
    if !removed && skipped.is_empty() {
        // A nested mount or an over-deep tree stays: Barge never reaches across.
        skipped.push(path.to_path_buf());
    }
    Ok(Deletion { bytes, skipped })
}

#[cfg(test)]
thread_local! {
    static OPEN_TREE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn marker(id: &str) -> crate::root::RootMarker {
        crate::root::RootMarker {
            schema: crate::root::ROOT_SCHEMA.to_string(),
            area: "test/area".to_string(),
            id: id.to_string(),
            label: "slot".to_string(),
            created_unix_ms: 1,
            expires_unix_ms: None,
            folder: None,
        }
    }

    #[test]
    fn a_symlink_is_removed_but_never_followed() {
        let temp = crate::testing::TempDir::new("trash-symlink").unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let root = temp.path().join("slot-0123456789abcdef");
        fs::create_dir(&root).unwrap();
        crate::root::write_marker(&root, &marker("0123456789abcdef")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        fs::write(root.join("file.txt"), b"data").unwrap();

        let deletion = remove_owned_tree(&root, "test/area").unwrap();

        assert!(deletion.bytes >= 4, "the file's bytes are counted");
        assert!(deletion.skipped.is_empty());
        assert!(!root.exists());
        assert!(
            outside.join("keep.txt").exists(),
            "the link target survives"
        );
    }

    #[test]
    fn a_replacement_at_the_trash_path_is_never_walked() {
        let temp = crate::testing::TempDir::new("trash-replaced").unwrap();
        let entry = temp.path().join("slot-0123456789abcdef");
        fs::create_dir(&entry).unwrap();
        crate::root::write_marker(&entry, &marker("0123456789abcdef")).unwrap();
        fs::write(entry.join("owned.bin"), b"owned").unwrap();
        // Ask the hook to put a folder with no marker of its own where the entry
        // is, in the moment between opening the entry and reading its marker.
        let moved = temp.path().join("moved");
        let hook_entry = entry.clone();
        let hook_moved = moved.clone();
        OPEN_TREE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&hook_entry, &hook_moved).unwrap();
                fs::create_dir(&hook_entry).unwrap();
                fs::write(hook_entry.join("replacement.bin"), b"keep me").unwrap();
            }))
        });

        let deletion = remove_owned_tree(&entry, "test/area").unwrap();

        assert!(
            entry.join("replacement.bin").exists(),
            "the replacement at the path is never walked"
        );
        assert!(
            !moved.join("owned.bin").exists(),
            "the entry whose marker was read is the one deleted"
        );
        assert!(
            deletion.skipped.contains(&entry),
            "the path changed under it"
        );
    }

    #[test]
    fn eviction_delete_refuses_a_different_root_id() {
        let temp = crate::testing::TempDir::new("trash-id").unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        crate::root::write_marker(&root, &marker("replacement")).unwrap();
        assert!(remove_owned_tree_for_id(&root, "test/area", "original").is_err());
        assert!(root.exists());
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
                folder: None,
            },
        )
        .unwrap();

        let error = remove_owned_tree(&root, "test/area").unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
        assert!(root.exists());
    }
}
