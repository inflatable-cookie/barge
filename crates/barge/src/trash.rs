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
//!
//! A sweep claims what it will delete under the area lock and deletes it after
//! releasing the lock, so a large deletion never stalls holds. A claim renames
//! the entry to [`CLAIM_PREFIX`] plus its name and keeps the exclusive lock on
//! the folder it opened, so a later sweep finishes a claim an interrupted sweep
//! left behind and never walks a tree another deletion is walking.
//!
//! A deletion removes a root's marker last, so the only folder it can leave with
//! no marker is the empty one its own last step could not remove. A sweep reaps
//! exactly that residue — the `rmdir` that removes it is also the test that it is
//! empty — and keeps every other folder without a marker. A writer that ignores
//! the hold and refills an emptied folder therefore leaves markerless trash, which
//! Barge refuses and reports rather than empties.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::area::{Area, AreaLock, TRASH_DIR};
use crate::error::{Error, Result};
use crate::hold::Mode;
use crate::os::{Reaped, Removed, Tree};
use crate::root::{read_marker_in, MarkerRead, MARKER_NAME};
use crate::space::{SkippedRoot, SweepReport, SweptRoot};

/// A folder opened for deletion, with the marker that was read through the same
/// descriptor, or the reason it will not be deleted.
enum Opened {
    /// The marker names this area (and root ID, when one was required).
    Owned(Tree),
    /// The folder holds no marker at all: Barge did not write it, so only the
    /// empty residue a deletion leaves may be reaped from it.
    Unmarked(Tree),
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
        MarkerRead::Missing => Opened::Unmarked(tree),
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
        // An eviction deletes the root it evicted, which carried a marker, so a
        // folder without one is not the entry it renamed: it is refused here and
        // the next sweep decides. An eviction that stopped leaves its entry in
        // the trash with its marker, and a sweep covers all of the trash.
        Opened::Unmarked(_) => Err(Error::refused(format!(
            "refusing to delete {}: it has no {MARKER_NAME}",
            path.display()
        ))),
        Opened::Refused(reason) => Err(Error::refused(reason)),
    }
}

/// A trash entry a sweep has claimed for deletion: opened, its marker read
/// through the descriptor the deletion walks, and locked so no second deletion
/// walks it too.
struct Claimed {
    tree: Tree,
    /// Where the entry was before it was claimed, which is what the report says.
    path: PathBuf,
}

/// Delete a root Barge created and could not finish writing a marker into.
///
/// Only a folder holding nothing but Barge's own marker is removed, and the walk
/// runs on the descriptor it was opened on, so a replacement at the path is never
/// walked and nothing a consumer put there is deleted.
pub(crate) fn remove_half_made(path: &Path) -> Result<Removed> {
    let tree = Tree::open(path)?;
    if !tree.holds_only(&[OsStr::new(MARKER_NAME)])? {
        return Err(Error::refused(format!(
            "refusing to delete {}: it holds an entry Barge did not create",
            path.display()
        )));
    }
    tree.remove()
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
    static SWEEP_DELETE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

impl Area {
    /// Delete every owned trash entry of this area, and reap the empty folder an
    /// interrupted deletion can leave in its place.
    ///
    /// The area lock is taken to list the trash and to claim what this pass will
    /// delete: an eviction renames its root into the trash under that same lock,
    /// so a sweep never deletes an entry whose rename it has not seen finish. The
    /// lock is released before any tree is deleted, so holds and other area
    /// operations continue while a large tree goes; the only removal under the
    /// lock is an empty leftover's single `rmdir`.
    ///
    /// A sweep covers the whole trash, so an eviction that stopped after its
    /// rename is finished here too: its entry still carries the marker, which is
    /// what a claim needs.
    pub fn sweep_trash(&self) -> Result<SweepReport> {
        self.core.ensure_writable()?;
        let key = self.key().label();
        let mut report = SweepReport::default();
        for (directory, _) in self.search_dirs()? {
            self.ensure_layout(&directory)?;
            let trash = directory.join(TRASH_DIR);
            // A pass claims and deletes a bounded batch, so a large trash never
            // holds one descriptor per entry; the next pass takes the rest. An
            // entry a pass leaves in place is reported once, not per pass.
            let mut reported_kept = HashSet::new();
            let mut first = true;
            loop {
                let claimed = {
                    let _lock = AreaLock::acquire(&directory)?;
                    claim_trash(&trash, &key, &mut report, &mut reported_kept, first)?
                };
                first = false;
                if claimed.is_empty() {
                    break;
                }
                for claim in claimed {
                    delete_claimed(claim, &mut report)?;
                }
            }
        }
        Ok(report)
    }
}

/// The name a sweep gives a trash entry while it deletes it. Eviction never
/// makes a name with this prefix, so a leftover claim is recognised as one.
const CLAIM_PREFIX: &str = ".barge-claim-";

/// How many trash entries one claim pass opens at once.
const CLAIM_BATCH: usize = 16;

/// List the trash under the area lock and claim the entries this pass will
/// delete. Each claimed entry is opened and its marker read through the
/// descriptor the deletion walks, and its exclusive lock is taken so a second
/// sweep or eviction leaves it alone; the pass stops claiming once
/// [`CLAIM_BATCH`] descriptors are held.
///
/// `first` is true for the pass that opens the directory: only then is an entry
/// that already carries a claim name taken, because a claim still there after
/// its own pass holds part of a tree this sweep could not remove.
fn claim_trash(
    trash: &Path,
    area: &str,
    report: &mut SweepReport,
    reported_kept: &mut HashSet<PathBuf>,
    first: bool,
) -> Result<Vec<Claimed>> {
    let entries = match fs::read_dir(trash) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::io("cannot list the trash at", trash, &source)),
    };
    let mut claimed = Vec::new();
    for entry in entries {
        #[cfg(test)]
        let entry = if FAIL_READ.with(|flag| flag.replace(false)) {
            Err(std::io::Error::other("injected readdir failure"))
        } else {
            entry
        };
        let entry =
            entry.map_err(|source| Error::io("cannot read a trash entry at", trash, &source))?;
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::io("cannot inspect", &path, &source)),
        };
        if metadata.file_type().is_symlink() {
            kept(
                report,
                reported_kept,
                path,
                "a symbolic link, never followed",
            );
            continue;
        }
        if !metadata.is_dir() {
            kept(report, reported_kept, path, "not a folder");
            continue;
        }
        if claimed.len() >= CLAIM_BATCH {
            // The next pass lists it again, after this batch is deleted.
            continue;
        }
        if !first && is_claim(&path) {
            // A claim this sweep left behind holds what it could not remove; it
            // was reported when it was claimed, and the next sweep takes it.
            continue;
        }
        match open_owned(&path, area, None)? {
            Opened::Owned(tree) => {
                if !crate::os::flock(tree.directory(), Mode::Exclusive, false)? {
                    kept(report, reported_kept, path, "another sweep is deleting it");
                    continue;
                }
                claimed.push(Claimed {
                    tree: claim(tree, &path, trash)?,
                    path,
                });
            }
            // Deletion removes a root's marker last, so a deletion interrupted at
            // its last step leaves an empty folder with none. That residue is the
            // only folder without a marker a sweep touches.
            Opened::Unmarked(tree) => reap_unmarked(tree, path, report, reported_kept),
            Opened::Refused(reason) => kept(report, reported_kept, path, &reason),
        }
    }
    Ok(claimed)
}

/// Record an entry a pass did not claim, once per sweep.
fn kept(
    report: &mut SweepReport,
    reported_kept: &mut HashSet<PathBuf>,
    path: PathBuf,
    reason: &str,
) {
    if reported_kept.insert(path.clone()) {
        report.kept.push(SkippedRoot {
            path,
            reason: reason.to_string(),
        });
    }
}

/// Reap a trash entry that carries no marker: `rmdir` is the emptiness test, so
/// the folder goes only while it is empty, and everything else is kept. A writer
/// that refills an emptied folder leaves markerless trash, which is refused here
/// and reported rather than emptied.
fn reap_unmarked(
    tree: Tree,
    path: PathBuf,
    report: &mut SweepReport,
    reported_kept: &mut HashSet<PathBuf>,
) {
    match tree.remove_if_empty() {
        Ok(Reaped::Removed) => report.deleted.push(SweptRoot { path, bytes: 0 }),
        Ok(Reaped::Replaced) => kept(report, reported_kept, path, "another folder is at its path"),
        Ok(Reaped::HoldsEntry) => kept(
            report,
            reported_kept,
            path,
            &format!("it has no {MARKER_NAME} and is not empty"),
        ),
        Err(error) => kept(report, reported_kept, path, error.message()),
    }
}

/// Rename a trash entry to its claim name, keeping the descriptor its marker was
/// read through, so the deletion walks the folder whose marker it saw. A claim
/// an earlier sweep left behind already carries the name.
fn claim(tree: Tree, path: &Path, trash: &Path) -> Result<Tree> {
    if is_claim(path) {
        return Ok(tree);
    }
    let name = path
        .file_name()
        .ok_or_else(|| Error::unknown_state(format!("{} has no name to claim", path.display())))?;
    let mut base = OsString::from(CLAIM_PREFIX);
    base.push(name);
    let mut counter = 0_u64;
    loop {
        let candidate = if counter == 0 {
            trash.join(&base)
        } else {
            let mut claimed = base.clone();
            claimed.push(format!(".{counter}"));
            trash.join(claimed)
        };
        counter += 1;
        match crate::os::rename_no_replace(path, &candidate) {
            Ok(()) => return Ok(tree.rebased(candidate)),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(Error::io("cannot claim the trash entry", path, &source)),
        }
    }
}

fn is_claim(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(CLAIM_PREFIX))
}

/// Delete a claimed tree, outside the area lock, and report it against the name
/// it had in the trash.
fn delete_claimed(claim: Claimed, report: &mut SweepReport) -> Result<()> {
    #[cfg(test)]
    SWEEP_DELETE_HOOK.with(|hook| {
        let callback = hook.borrow_mut().take();
        if let Some(hook) = callback {
            hook();
        }
    });
    let Claimed { tree, path } = claim;
    let claimed_path = tree.path().to_path_buf();
    match tree.remove() {
        Ok(removed) => {
            report.deleted.push(SweptRoot {
                path,
                bytes: removed.bytes,
            });
            Ok(())
        }
        Err(error) => Err(Error::new(
            error.kind(),
            format!(
                "{}; trash remains at {}",
                error.message(),
                claimed_path.display()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;
    use std::sync::{mpsc, Arc};

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

    /// An area on a fake volume, which keeps every file it touches real.
    fn fixture(name: &str) -> (crate::testing::TempDir, Area) {
        let temp = crate::testing::TempDir::new(name).unwrap();
        let mount = temp.folder("mount").unwrap();
        let state = temp.folder("state").unwrap();
        let os = Arc::new(crate::FakeOs::with_volume(
            &mount,
            "11111111-2222-3333-4444-555555555555",
            100,
        ));
        let barge =
            crate::Barge::open_with(state, os, Arc::new(crate::ManualClock::new(1))).unwrap();
        barge
            .register_volume(crate::VolumeRegistration::new("disk", &mount).floor_bytes(0))
            .unwrap();
        let area = barge
            .area(crate::AreaSpec::new("test", "area", "disk"))
            .unwrap();
        (temp, area)
    }

    /// A root this area owns, moved into the trash as a crash after eviction's
    /// rename would leave it.
    fn trashed(area: &Area, label: &str) -> PathBuf {
        let root = area.create_root(label).unwrap();
        let trash = root.path.parent().unwrap().join(TRASH_DIR);
        fs::create_dir_all(&trash).unwrap();
        let path = trash.join(&root.folder);
        fs::rename(&root.path, &path).unwrap();
        path
    }

    #[test]
    fn a_hold_can_be_taken_while_a_sweep_deletes() {
        let (_temp, area) = fixture("trash-hold");
        let doomed = trashed(&area, "old");
        let survivor = area.create_root("new").unwrap();
        let directory = survivor.path.parent().unwrap().to_path_buf();
        let other = area.clone();
        let survivor_id = survivor.id.clone();
        let held = Rc::new(RefCell::new(None));
        let taken = held.clone();
        SWEEP_DELETE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                // The sweep deletes outside the area lock, so a hold taken here
                // never waits for the tree to be gone. Waiting for zero says so
                // without risking a hang in the regression this covers.
                assert!(
                    AreaLock::acquire_within(&directory, std::time::Duration::ZERO)
                        .unwrap()
                        .is_some(),
                    "the sweep holds the area lock while it deletes"
                );
                *taken.borrow_mut() = Some(other.hold(&survivor_id, Mode::Exclusive).unwrap());
            }))
        });

        let report = area.sweep_trash().unwrap();

        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].path, doomed);
        assert!(!doomed.exists(), "the tree was deleted after the hold");
        assert!(
            held.borrow().is_some(),
            "the hold was taken during the delete"
        );
    }

    #[test]
    fn two_sweeps_never_delete_the_same_tree() {
        let (_temp, area) = fixture("trash-race");
        let leaves = [trashed(&area, "one"), trashed(&area, "two")];
        let (claimed_tx, claimed_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let first_area = area.clone();
        let first = std::thread::spawn(move || {
            SWEEP_DELETE_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    claimed_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                }))
            });
            first_area.sweep_trash()
        });
        // The first sweep holds both claims and is inside the first deletion.
        claimed_rx.recv().unwrap();

        // A second sweep must leave those claims alone, and still delete an
        // entry that arrives meanwhile without erroring on the claims.
        let late = trashed(&area, "three");
        let second = area.sweep_trash().unwrap();
        resume_tx.send(()).unwrap();
        let first = first.join().unwrap().unwrap();

        assert_eq!(second.deleted.len(), 1);
        assert_eq!(second.deleted[0].path, late);
        assert_eq!(second.kept.len(), 2, "{:?}", second.kept);
        assert!(second
            .kept
            .iter()
            .all(|kept| kept.reason.contains("another sweep")));
        assert_eq!(first.deleted.len(), 2);
        let mut deleted: Vec<PathBuf> = first
            .deleted
            .iter()
            .chain(&second.deleted)
            .map(|swept| swept.path.clone())
            .collect();
        deleted.sort();
        deleted.dedup();
        assert_eq!(deleted.len(), 3, "no tree is counted twice: {deleted:?}");
        assert!(leaves.iter().all(|path| !path.exists()));
    }

    /// Deny a folder's own removal, decided by the mode of its parent, while
    /// leaving its entries removable.
    fn deny_parent_writes(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).unwrap();
    }

    fn allow_writes(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_final_step_that_cannot_remove_the_entry_leaves_it_for_the_next_sweep() {
        if crate::os::running_as_root() {
            // Root ignores the mode that makes the last step fail.
            return;
        }
        let (_temp, area) = fixture("trash-final-failure");
        let entry = trashed(&area, "old");
        let trash = entry.parent().unwrap().to_path_buf();
        fs::write(entry.join("artifact.bin"), b"artifact").unwrap();
        let marker = match crate::root::read_marker(&entry) {
            crate::root::MarkerRead::Valid(marker) => marker,
            other => panic!("the fixture has an owned marker: {other:?}"),
        };
        // The entry's own contents can go, but the trash refuses the entry's own
        // removal, so the deletion fails at its last step with the marker gone.
        deny_parent_writes(&trash);

        let error = remove_evicted_tree(&entry, "test/area", &marker.id).unwrap_err();

        allow_writes(&trash);
        assert_eq!(error.kind(), crate::error::ErrorKind::Io, "{error}");
        assert!(!entry.join("artifact.bin").exists(), "the tree was walked");
        assert!(
            !entry.join(MARKER_NAME).exists() && fs::read_dir(&entry).unwrap().next().is_none(),
            "the last step leaves an empty folder with no marker"
        );

        // That empty residue is what the next sweep reaps, so it does not stay in
        // the trash as a folder no pass can claim.
        let report = area.sweep_trash().unwrap();

        assert!(report.kept.is_empty(), "{:#?}", report.kept);
        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].path, entry);
        assert_eq!(report.deleted[0].bytes, 0);
        assert!(!entry.exists(), "the next sweep reaps the residue");
    }

    /// A trash folder with nothing in it, for the leftovers a deletion can leave.
    fn empty_trash(area: &Area) -> PathBuf {
        let trash = trashed(area, "old").parent().unwrap().to_path_buf();
        // The fixture's own entry is not part of the case.
        for entry in fs::read_dir(&trash).unwrap() {
            fs::remove_dir_all(entry.unwrap().path()).unwrap();
        }
        trash
    }

    #[test]
    fn an_empty_leftover_is_reaped_but_a_filled_one_is_kept() {
        let (_temp, area) = fixture("trash-reap");
        let trash = empty_trash(&area);
        // The residue of an interrupted deletion, including a claim an
        // interrupted sweep left: both are empty and carry no marker.
        let plain = trash.join("slot-0123456789abcdef");
        fs::create_dir(&plain).unwrap();
        let mut claim_name = OsString::from(CLAIM_PREFIX);
        claim_name.push("slot-fedcba9876543210");
        let claimed = trash.join(claim_name);
        fs::create_dir(&claimed).unwrap();
        // A folder Barge did not write, holding an entry: never emptied.
        let filled = trash.join("slot-1111111111111111");
        fs::create_dir(&filled).unwrap();
        fs::write(filled.join("consumer.bin"), b"consumer").unwrap();

        let report = area.sweep_trash().unwrap();

        let mut reaped: Vec<PathBuf> = report.deleted.iter().map(|s| s.path.clone()).collect();
        reaped.sort();
        assert_eq!(reaped, vec![claimed, plain]);
        assert!(report.deleted.iter().all(|swept| swept.bytes == 0));
        assert_eq!(report.kept.len(), 1, "{:#?}", report.kept);
        assert_eq!(report.kept[0].path, filled);
        assert!(
            report.kept[0].reason.contains("is not empty"),
            "{}",
            report.kept[0].reason
        );
        assert!(filled.join("consumer.bin").exists(), "its entry survives");
    }

    #[test]
    fn a_leftover_whose_path_changed_is_not_reaped() {
        let (_temp, area) = fixture("trash-reap-moved");
        let trash = empty_trash(&area);
        let leftover = trash.join("slot-0123456789abcdef");
        fs::create_dir(&leftover).unwrap();
        let moved = trash.join("moved");
        // Between opening the entry and reaping it, a racing writer moves it and
        // leaves a folder of its own, with an entry, at the path.
        let hook_leftover = leftover.clone();
        let hook_moved = moved.clone();
        OPEN_TREE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&hook_leftover, &hook_moved).unwrap();
                fs::create_dir(&hook_leftover).unwrap();
                fs::write(hook_leftover.join("replacement.bin"), b"keep me").unwrap();
            }))
        });

        let report = area.sweep_trash().unwrap();

        assert!(
            leftover.join("replacement.bin").exists(),
            "the folder at the path was never reaped"
        );
        assert!(moved.is_dir(), "the entry that was opened stays put");
        assert!(report.deleted.is_empty(), "{:#?}", report.deleted);
        assert_eq!(report.kept.len(), 1, "{:#?}", report.kept);
        assert_eq!(report.kept[0].path, leftover);
        assert!(
            report.kept[0]
                .reason
                .contains("another folder is at its path"),
            "{}",
            report.kept[0].reason
        );
    }

    #[test]
    fn an_interrupted_deletion_keeps_the_marker_so_the_next_sweep_finishes_it() {
        let (_temp, area) = fixture("trash-marker-last");
        let entry = trashed(&area, "old");
        let build = entry.join("build");
        fs::create_dir(&build).unwrap();
        fs::write(build.join("artifact.bin"), b"artifact").unwrap();
        let marker = entry.join(MARKER_NAME);

        // A nested-mount stand-in from the seam stops the walk partway, as an
        // I/O error, a racing writer or a crash would.
        let removed = Tree::open(&entry)
            .unwrap()
            .remove_with(&|path, device| {
                if path.ends_with("build") {
                    device + 1
                } else {
                    device
                }
            })
            .unwrap();

        assert!(!removed.removed);
        assert!(
            marker.exists(),
            "the marker goes last, so it is still there"
        );
        assert!(build.join("artifact.bin").exists());

        let report = area.sweep_trash().unwrap();

        assert!(report.kept.is_empty(), "{:#?}", report.kept);
        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].path, entry);
        assert!(!entry.exists(), "the next sweep finishes the tree");
    }

    #[test]
    fn a_claimed_tree_left_part_way_is_finished_by_the_next_sweep() {
        let (_temp, area) = fixture("trash-claim-marker-last");
        let entry = trashed(&area, "old");
        let build = entry.join("build");
        fs::create_dir(&build).unwrap();
        fs::write(build.join("artifact.bin"), b"artifact").unwrap();
        // A sweep claims under the area lock and deletes after it, so a deletion
        // that stops leaves the claim name behind.
        let mut claim_name = std::ffi::OsString::from(CLAIM_PREFIX);
        claim_name.push(entry.file_name().unwrap());
        let leftover = entry.parent().unwrap().join(claim_name);
        fs::rename(&entry, &leftover).unwrap();

        let removed = Tree::open(&leftover)
            .unwrap()
            .remove_with(&|path, device| {
                if path.ends_with("build") {
                    device + 1
                } else {
                    device
                }
            })
            .unwrap();
        assert!(!removed.removed);
        assert!(
            leftover.join(MARKER_NAME).exists(),
            "the claim keeps its marker"
        );

        let report = area.sweep_trash().unwrap();

        assert!(report.kept.is_empty(), "{:#?}", report.kept);
        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].path, leftover);
        assert!(!leftover.exists(), "the next sweep finishes the claim");
    }

    #[test]
    fn a_claim_left_by_an_interrupted_sweep_is_swept() {
        let (_temp, area) = fixture("trash-leftover");
        let entry = trashed(&area, "old");
        let name = entry.file_name().unwrap().to_owned();
        let mut claim_name = std::ffi::OsString::from(CLAIM_PREFIX);
        claim_name.push(name);
        let leftover = entry.parent().unwrap().join(claim_name);
        fs::rename(&entry, &leftover).unwrap();

        let report = area.sweep_trash().unwrap();

        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].path, leftover);
        assert!(!leftover.exists());
    }

    #[test]
    fn a_kept_entry_is_reported_once_and_a_large_trash_is_cleared() {
        let (_temp, area) = fixture("trash-batches");
        let trash = trashed(&area, "doomed").parent().unwrap().to_path_buf();
        // A folder in the trash whose marker names another area stays, however
        // many passes the rest of the trash takes.
        let foreign = trash.join("slot-0123456789abcdef");
        fs::create_dir(&foreign).unwrap();
        crate::root::write_marker(
            &foreign,
            &crate::root::RootMarker {
                area: "other/area".to_string(),
                ..marker("0123456789abcdef")
            },
        )
        .unwrap();
        for index in 0..CLAIM_BATCH + 2 {
            trashed(&area, &format!("slot-{index}"));
        }

        let report = area.sweep_trash().unwrap();

        assert_eq!(report.deleted.len(), CLAIM_BATCH + 3);
        assert_eq!(report.kept.len(), 1, "reported once, not once per pass");
        assert_eq!(report.kept[0].path, foreign);
        assert!(foreign.is_dir());
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 1);
    }

    #[test]
    fn an_interrupted_eviction_leaves_an_entry_the_next_sweep_finishes() {
        let (_temp, area) = fixture("trash-interrupted-eviction");
        let root = area.create_root("old").unwrap();
        fs::write(root.path.join("artifact.bin"), b"artifact").unwrap();
        // The delete the eviction runs fails, as an I/O error or a process that
        // exits would leave it: the entry is already renamed into the trash, with
        // its marker and everything else it held.
        FAIL_DELETE.with(|flag| flag.set(true));

        let error = area.evict(&root.id).unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Io, "{error}");
        let entry = root
            .path
            .parent()
            .unwrap()
            .join(TRASH_DIR)
            .join(&root.folder);
        assert!(
            entry.join(MARKER_NAME).exists(),
            "the entry keeps its marker"
        );
        assert!(entry.join("artifact.bin").exists());

        // A sweep covers the whole trash, so it finishes what the eviction left,
        // without the eviction path needing a markerless case of its own.
        let report = area.sweep_trash().unwrap();

        assert!(report.kept.is_empty(), "{:#?}", report.kept);
        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].path, entry);
        assert!(!entry.exists());
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
