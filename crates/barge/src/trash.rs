//! Deleting only what Barge owns.
//!
//! Deletion never follows a symlink and never descends into another filesystem,
//! so a nested mount or a link planted in a root is skipped rather than followed.
//! A trash entry is deleted only when its marker still names the area, and the
//! marker is read through the descriptor the deletion then walks: the folder
//! whose ownership was checked is the folder that is removed.
//!
//! Eviction itself lives in [`crate::area`], next to the locks whose ordering it
//! depends on; this module is the deletion it performs and the trash sweep.

use std::fs;
use std::path::Path;

use crate::area::{Area, AreaLock, TRASH_DIR};
use crate::error::{Error, Result};
use crate::os::{Removed, Tree};
use crate::root::{read_marker_in, MarkerRead, MARKER_NAME};
use crate::space::{SkippedRoot, SweepReport, SweptRoot};

/// A folder opened for deletion, with the marker that was read through the same
/// descriptor, or the reason it will not be deleted.
enum Opened {
    /// The marker names this area (and root ID, when one was required).
    Owned(Tree),
    /// The folder is not Barge's to delete, with the reason to report.
    Refused(String),
}

/// Open a folder and read its marker through the descriptor the removal then
/// walks, so one read decides ownership and a replacement at the path between the
/// check and the removal is never walked.
fn open_owned(path: &Path, area: &str, id: Option<&str>) -> Result<Opened> {
    let tree = Tree::open(path)?;
    // A racing writer may put a different folder at the path now; the walk below
    // still sees the one that was opened.
    #[cfg(test)]
    OPEN_TREE_HOOK.with(|hook| {
        let callback = hook.borrow_mut().take();
        if let Some(hook) = callback {
            hook();
        }
    });
    Ok(match read_marker_in(tree.directory(), tree.path()) {
        MarkerRead::Valid(marker) if marker.area == area && id.is_none_or(|id| marker.id == id) => {
            Opened::Owned(tree)
        }
        MarkerRead::Valid(marker) => Opened::Refused(format!(
            "refusing to delete {}: its marker names area {}, not {area} or the expected root ID",
            path.display(),
            marker.area
        )),
        MarkerRead::Missing => Opened::Refused(format!(
            "refusing to delete {}: it has no {MARKER_NAME}",
            path.display()
        )),
        MarkerRead::Invalid(reason) => {
            Opened::Refused(format!("refusing to delete {}: {reason}", path.display()))
        }
    })
}

/// Delete an evicted root's trash entry, which must still name the area and the
/// root ID.
pub(crate) fn remove_evicted_tree(path: &Path, area: &str, id: &str) -> Result<Removed> {
    #[cfg(test)]
    if FAIL_DELETE.with(|flag| flag.replace(false)) {
        return Err(Error::io_message("injected deletion failure"));
    }
    #[cfg(test)]
    DELETE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    match remove_owned(path, area, Some(id)) {
        Err(error) if matches!(fs::symlink_metadata(path), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound) =>
        {
            let _ = error;
            Ok(Removed {
                removed: true,
                ..Removed::default()
            })
        }
        other => other,
    }
}

fn remove_owned(path: &Path, area: &str, id: Option<&str>) -> Result<Removed> {
    match open_owned(path, area, id)? {
        Opened::Owned(tree) => tree.remove(),
        Opened::Refused(reason) => Err(Error::refused(reason)),
    }
}

/// Why a deletion of an owned tree left part of it in place.
pub(crate) fn stayed_reason(removed: &Removed) -> String {
    let first = removed
        .skipped
        .first()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    format!(
        "{} entr{} stayed in place, starting at {first}",
        removed.skipped.len(),
        if removed.skipped.len() == 1 {
            "y"
        } else {
            "ies"
        }
    )
}

#[cfg(test)]
thread_local! {
    pub(crate) static DELETE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static FAIL_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static FAIL_DELETE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static OPEN_TREE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

impl Area {
    /// Delete every owned trash entry of this area.
    ///
    /// The area lock is taken before the listing: an eviction renames its root
    /// into the trash under that same lock, so a sweep never deletes an entry
    /// whose rename it has not seen finish.
    pub fn sweep_trash(&self) -> Result<SweepReport> {
        self.core.ensure_writable()?;
        let key = self.key().label();
        let mut report = SweepReport::default();
        for (directory, _) in self.search_dirs()? {
            self.ensure_layout(&directory)?;
            let _lock = AreaLock::acquire(&directory)?;
            let trash = directory.join(TRASH_DIR);
            let entries = match fs::read_dir(&trash) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(Error::io("cannot list the trash at", &trash, &source)),
            };
            for entry in entries {
                #[cfg(test)]
                let entry = if FAIL_READ.with(|flag| flag.replace(false)) {
                    Err(std::io::Error::other("injected readdir failure"))
                } else {
                    entry
                };
                let entry = entry
                    .map_err(|source| Error::io("cannot read a trash entry at", &trash, &source))?;
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => return Err(Error::io("cannot inspect", &path, &source)),
                };
                if metadata.file_type().is_symlink() {
                    report.kept.push(SkippedRoot {
                        path,
                        reason: "a symbolic link, never followed".to_string(),
                    });
                    continue;
                }
                if !metadata.is_dir() {
                    report.kept.push(SkippedRoot {
                        path,
                        reason: "not a folder".to_string(),
                    });
                    continue;
                }
                match open_owned(&path, &key, None)? {
                    Opened::Owned(tree) => match tree.remove() {
                        Ok(removed) => report.deleted.push(SweptRoot {
                            path,
                            bytes: removed.bytes,
                        }),
                        Err(error) => {
                            return Err(Error::new(
                                error.kind(),
                                format!("{}; trash remains at {}", error.message(), path.display()),
                            ))
                        }
                    },
                    Opened::Refused(reason) => report.kept.push(SkippedRoot { path, reason }),
                }
            }
        }
        Ok(report)
    }
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
    fn readdir_failure_is_reported() {
        use std::sync::Arc;
        let temp = crate::testing::TempDir::new("trash-read").unwrap();
        let mount = temp.folder("mount").unwrap();
        let state = temp.folder("state").unwrap();
        let os = Arc::new(crate::FakeOs::with_volume(
            &mount,
            "11111111-2222-3333-4444-555555555555",
            100,
        ));
        let b = crate::Barge::open_with(state, os, Arc::new(crate::ManualClock::new(1))).unwrap();
        b.register_volume(crate::VolumeRegistration::new("disk", &mount).floor_bytes(0))
            .unwrap();
        let area = b
            .area(crate::AreaSpec::new("test", "area", "disk"))
            .unwrap();
        let root = area.create_root("slot").unwrap();
        FAIL_DELETE.with(|flag| flag.set(true));
        assert!(area.evict(&root.id).is_err());
        FAIL_READ.with(|flag| flag.set(true));
        assert!(area
            .sweep_trash()
            .unwrap_err()
            .message()
            .contains("injected readdir"));
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

        let deletion = remove_evicted_tree(&root, "test/area", "0123456789abcdef").unwrap();

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

        let deletion = remove_evicted_tree(&entry, "test/area", "0123456789abcdef").unwrap();

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
        assert!(remove_evicted_tree(&root, "test/area", "original").is_err());
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

        let error = remove_evicted_tree(&root, "test/area", "0123456789abcdef").unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
        assert!(root.exists());
    }
}
