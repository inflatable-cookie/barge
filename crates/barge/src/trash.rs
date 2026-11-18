//! Deleting only what Barge owns.
//!
//! Deletion never follows a symlink and never descends into another filesystem,
//! so a nested mount or a link planted in a root is skipped rather than followed.
//! A trash entry is deleted only when its marker still names the area, and the
//! marker is read through the descriptor the deletion then walks: the folder
//! whose ownership was checked is the folder that is removed.

#[cfg(test)]
use crate::area::EVICT_HOOK;
use crate::area::{
    lock_root, parent, unchanged_marker, unique_trash_path, Area, AreaLock, Eviction, ACTIVITY_DIR,
    BARGE_DIR, LOCKS_DIR, TRASH_DIR,
};
use crate::hold::{self, Mode};
use crate::pin::{pin_path, pin_state};
use crate::root::{self, read_marker, tree_bytes};
use crate::space::{SkippedRoot, SweepReport, SweptRoot};
use std::fs;
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
    pub(crate) static FAIL_DELETE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static OPEN_TREE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

impl Area {
    /// Evict a root: refuse when it is held or pinned, otherwise rename it into
    /// the area's trash and delete it there.
    pub fn evict(&self, id: &str) -> Result<Eviction> {
        self.core.ensure_writable()?;
        let root = self.resolve(id)?;
        #[cfg(test)]
        EVICT_HOOK.with(|hook| {
            let callback = hook.borrow_mut().take();
            if let Some(hook) = callback {
                hook();
            }
        });
        let directory = parent(&root.path)?;
        self.ensure_layout(&directory)?;
        let lock_path = directory
            .join(BARGE_DIR)
            .join(LOCKS_DIR)
            .join(format!("{id}.lock"));
        let Some(_lock_file) = lock_root(&lock_path, Mode::Exclusive, false)? else {
            return Err(Error::refused(format!(
                "root {id} is held; refusing to evict it"
            )));
        };
        let _area_lock = AreaLock::acquire(&directory)?;
        if pin_state(&directory, id)? {
            return Err(Error::refused(format!(
                "root {id} is pinned; refusing to evict it"
            )));
        }
        let size = tree_bytes(&root.path);
        let trash = directory.join(TRASH_DIR);
        fs::create_dir_all(&trash)
            .map_err(|source| Error::io("cannot create the trash at", &trash, &source))?;
        let trash_path = unique_trash_path(&trash, &root.folder);
        if !unchanged_marker(&root, &self.key().label()) {
            return Err(Error::refused("root marker changed before eviction"));
        }
        fs::rename(&root.path, &trash_path)
            .map_err(|source| Error::io("cannot evict the root to", &trash_path, &source))?;
        // The root is renamed: a waiting holder wakes to a missing root and the
        // lock file is no longer useful. Unlinking it does not release our lock;
        // dropping the descriptor does.
        let _ = fs::remove_file(&lock_path);
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        let _ = fs::remove_file(hold::activity_path(&activity, id));
        let pin = pin_path(&directory, id);
        let _ = fs::remove_file(&pin);
        #[cfg(test)]
        let injected = FAIL_DELETE.with(|flag| flag.replace(false));
        #[cfg(not(test))]
        let injected = false;
        let deletion = if injected {
            Err(Error::new(
                crate::ErrorKind::Io,
                "injected deletion failure",
            ))
        } else {
            crate::trash::remove_owned_tree_for_id(&trash_path, &self.key().label(), id)
        };
        let (deleted, kept_reason) = match deletion {
            Ok(deletion) if deletion.skipped.is_empty() => (true, None),
            Ok(deletion) => (
                false,
                Some(format!(
                    "{} entr{} stayed in place, starting at {}",
                    deletion.skipped.len(),
                    if deletion.skipped.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    deletion.skipped[0].display()
                )),
            ),
            Err(error) => {
                return Err(Error::new(
                    crate::ErrorKind::Io,
                    format!(
                        "{}; evicted root remains in trash at {}",
                        error.message(),
                        trash_path.display()
                    ),
                ))
            }
        };
        Ok(Eviction {
            area: self.key().label(),
            id: id.to_string(),
            label: root.label,
            path: root.path,
            trash_path,
            bytes: size.bytes,
            bytes_approximate: size.approximate,
            deleted,
            kept_reason,
        })
    }

    /// Delete every owned trash entry of this area.
    pub fn sweep_trash(&self) -> Result<SweepReport> {
        self.core.ensure_writable()?;
        let mut report = SweepReport::default();
        let key = self.key().label();
        for (directory, _) in self.search_dirs()? {
            let trash = directory.join(TRASH_DIR);
            let entries = match fs::read_dir(&trash) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(Error::io("cannot list the trash at", &trash, &source)),
            };
            self.ensure_layout(&directory)?;
            let _lock = AreaLock::acquire(&directory)?;
            for entry in entries.flatten() {
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
                if metadata.is_dir() {
                    match read_marker(&path) {
                        MarkerRead::Valid(marker) if marker.area == key => {
                            match crate::trash::remove_owned_tree(&path, &key) {
                                Ok(deletion) => report.deleted.push(SweptRoot {
                                    path,
                                    bytes: deletion.bytes,
                                }),
                                Err(error) => report.kept.push(SkippedRoot {
                                    path,
                                    reason: error.message().to_string(),
                                }),
                            }
                        }
                        MarkerRead::Valid(marker) => report.kept.push(SkippedRoot {
                            path,
                            reason: format!("its marker belongs to area {}", marker.area),
                        }),
                        MarkerRead::Missing => report.kept.push(SkippedRoot {
                            path,
                            reason: format!("no {}", root::MARKER_NAME),
                        }),
                        MarkerRead::Invalid(reason) => {
                            report.kept.push(SkippedRoot { path, reason })
                        }
                    }
                } else {
                    report.kept.push(SkippedRoot {
                        path,
                        reason: "not a folder".to_string(),
                    });
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
