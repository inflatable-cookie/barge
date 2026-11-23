//! Moving a declared area's folder to another folder on the same volume.
//!
//! The move is one `rename` on one filesystem, then the updated declaration. Roots
//! keep their IDs and markers. Pins, the area lock, hold locks, activity records
//! and trash are inside the folder, so they move with it. Nothing else changes:
//! no shared layout record is written, and a private account moves its own
//! declaration without any class placement.
//!
//! The move is same-volume only because a cross-filesystem `rename` is not one
//! atomic step; a copy-then-delete would be a second mechanic with its own
//! failure modes, and nothing needs it yet.
//!
//! The rename and the declaration are two writes. A crash between them leaves
//! the folder at the new path and the record at the old one. Nothing repairs that
//! automatically: [`Area::displaced_folder`] lets `status` and `doctor` report it.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{parent, Area, AreaLock, BARGE_DIR, TRASH_DIR};
use crate::area_spec::{AreaKey, AreaRecord};
use crate::error::{Error, Result};
use crate::hold::{self, Mode};
use crate::root::{read_marker, MarkerRead};
use crate::volume::VolumeState;
use crate::Barge;

/// How many folders below the volume base a displaced area folder is looked for.
/// `builds/oxide/pool` sits three levels down. The limit keeps `status` from
/// walking a whole external drive for every area that has no folder yet.
const DISPLACED_SEARCH_DEPTH: u32 = 3;

#[cfg(test)]
thread_local! {
    /// Runs with every root locked and before the rename, so a test can try a hold
    /// during the move. A test can also make the declaration write fail.
    pub(crate) static RELOCATE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

impl Barge {
    /// Move a declared area's folder to `new_folder`, relative to the same volume
    /// base, and record the new folder. The volume, absent policy, soft cap and
    /// reserve flag do not change.
    ///
    /// Refused, with nothing changed: a read-only account; an unknown area; an
    /// absent or different volume; a source folder that does not exist or is not
    /// a real directory; a destination that exists; a destination nested inside
    /// the source or the source inside the destination; a destination on another
    /// filesystem; and any root of the area that is held.
    ///
    /// The account state lock and the area lock are held throughout, and every
    /// root's hold lock is taken without waiting and kept until the move ends. A
    /// hold that starts during the move finds its lock taken, so it gets no hold
    /// on a root the move is about to change. If the new declaration cannot be
    /// written, the folder is renamed back and the write's error is returned.
    pub fn relocate_area(&self, key: &AreaKey, new_folder: &str) -> Result<AreaRecord> {
        self.core.ensure_writable()?;
        let _state = self.core.state.lock()?;
        let stored = self
            .core
            .state
            .read_area(key)?
            .ok_or_else(|| Error::not_found(format!("area {} is not declared", key.label())))?;
        let target = relocated_record(&stored, new_folder)?;
        let old_folder = stored.effective_folder();
        let new_folder = target.effective_folder();
        if old_folder == new_folder {
            return Err(Error::refused(format!(
                "area {} is already in folder {old_folder}",
                key.label()
            )));
        }
        if nested(&old_folder, &new_folder) {
            return Err(Error::refused(format!(
                "area {} cannot move from {old_folder} to {new_folder}: one folder lies inside the other",
                key.label()
            )));
        }

        let volume = self.core.volume(&stored.volume)?;
        volume.require_present()?;
        let base = volume.base();
        let source = base.join(&old_folder);
        let destination = base.join(&new_folder);
        let source_metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(source_error) if source_error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::refused(format!(
                    "area {} folder {} does not exist; nothing to move",
                    key.label(),
                    source.display()
                )))
            }
            Err(source_error) => return Err(Error::io("cannot inspect", &source, &source_error)),
        };
        if !source_metadata.is_dir() || source_metadata.file_type().is_symlink() {
            return Err(Error::refused(format!(
                "area {} folder {} is not a real directory",
                key.label(),
                source.display()
            )));
        }
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(Error::refused(format!(
                "{} already exists; refusing to move area {} over it",
                destination.display(),
                key.label()
            )));
        }
        // Created parents are inside the nearest folder that exists, so that
        // folder's filesystem decides whether the rename is one step.
        let nearest = nearest_existing(&destination);
        let nearest_device = nearest
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.dev());
        if nearest_device != Some(source_metadata.dev()) {
            return Err(Error::refused(format!(
                "{} is on another filesystem than {}; an area moves only within its volume",
                destination.display(),
                source.display()
            )));
        }

        let area = self.area_from_record(stored.clone())?;
        area.ensure_layout(&source)?;
        let _area_lock = AreaLock::acquire(&source)?;
        // Locking, not probing, closes the gap between the check and the rename: a
        // probe releases the lock, so a hold could start right after it. These
        // locks are declared after the area lock, so they release before it.
        let mut root_locks = Vec::new();
        for info in area.list_without_bytes()?.roots {
            let directory = parent(&info.path)?;
            let lock_path = hold::hold_lock_path(&directory, &info.id);
            let Some(lock) = hold::lock_root_try(&lock_path, Mode::Exclusive)? else {
                return Err(Error::refused(format!(
                    "root {} in area {} is held; refusing to move the area",
                    info.id,
                    key.label()
                )));
            };
            root_locks.push(lock);
        }

        #[cfg(test)]
        RELOCATE_HOOK.with(|hook| {
            let callback = hook.borrow_mut().take();
            if let Some(hook) = callback {
                hook();
            }
        });

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Error::io("cannot create", parent, &error))?;
        }
        crate::os::rename_no_replace(&source, &destination).map_err(|rename_error| {
            if rename_error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::refused(format!(
                    "{} already exists; refusing to move area {} over it",
                    destination.display(),
                    key.label()
                ))
            } else {
                Error::io("cannot move the area to", &destination, &rename_error)
            }
        })?;

        if let Err(write_error) = self.core.state.write_area(&target) {
            return Err(match crate::os::rename_no_replace(&destination, &source) {
                Ok(()) => write_error,
                Err(rollback_error) => Error::io_message(format!(
                    "{}; the area folder stayed at {} because it could not be moved back to {}: {rollback_error}",
                    write_error.message(),
                    destination.display(),
                    source.display()
                )),
            });
        }
        Ok(target)
    }
}

impl Area {
    /// The folder on the volume that holds this area's roots when the recorded
    /// folder is missing: a relocation that stopped between its rename and its
    /// declaration. `None` when the recorded folder exists, the volume is not
    /// present, or no folder with this area's roots is found within the search
    /// depth. A moved area with no roots has no marker to find, so it is not
    /// reported.
    pub(crate) fn displaced_folder(&self) -> Result<Option<PathBuf>> {
        if self.volume.state()? != VolumeState::Present {
            return Ok(None);
        }
        let base = self.volume.base();
        let recorded = base.join(self.record.effective_folder());
        if fs::symlink_metadata(&recorded).is_ok() {
            return Ok(None);
        }
        Ok(find_area_folder(
            &base,
            &self.key().label(),
            DISPLACED_SEARCH_DEPTH,
        ))
    }
}

/// The declaration with `new_folder`, validated like a declared folder. The
/// default folder is stored as no override, so a later declaration without one
/// still agrees.
fn relocated_record(stored: &AreaRecord, new_folder: &str) -> Result<AreaRecord> {
    let default = format!("{}/{}", stored.namespace, stored.name);
    let folder = (new_folder != default).then(|| new_folder.to_string());
    let target = AreaRecord {
        folder,
        ..stored.clone()
    };
    target.validate()?;
    if new_folder
        .split('/')
        .any(|component| component == BARGE_DIR || component == TRASH_DIR)
    {
        return Err(Error::usage(format!(
            "area folder {new_folder:?} may not use the reserved name {BARGE_DIR} or {TRASH_DIR}"
        )));
    }
    Ok(target)
}

/// Whether one relative folder path is the other or lies inside it.
fn nested(first: &str, second: &str) -> bool {
    first == second
        || second.starts_with(&format!("{first}/"))
        || first.starts_with(&format!("{second}/"))
}

/// The nearest path, starting at `path` itself and walking up, that exists.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| fs::metadata(candidate).is_ok())
        .map(Path::to_path_buf)
}

/// Search `directory`, and the folders below it up to `depth` levels, for an area
/// folder that holds roots of `area`. Symlinks and dot-folders are never followed
/// or searched: that covers Barge's own `.barge` and `.trash` and macOS metadata.
/// An area folder is not searched inside.
fn find_area_folder(directory: &Path, area: &str, depth: u32) -> Option<PathBuf> {
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if holds_roots_of(&path, area) {
            return Some(path);
        }
        if depth > 1 {
            if let Some(found) = find_area_folder(&path, area, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Whether `folder` is an area folder (it has Barge's folder) with a root whose
/// marker names `area`.
fn holds_roots_of(folder: &Path, area: &str) -> bool {
    if !folder.join(BARGE_DIR).is_dir() {
        return false;
    }
    let Ok(entries) = fs::read_dir(folder) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let is_dir = fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        is_dir
            && matches!(
                read_marker(&path),
                MarkerRead::Valid(marker) if marker.area == area
            )
    })
}
