//! Areas: a consumer's declaration, where new roots go, and the operations on
//! the roots in it (create, list, hold, pin, evict, sweep).
//!
//! The stored declaration is authoritative. A caller that presents a different
//! one is refused before any effect, naming the differing fields.
//!
//! A root is found by its ID wherever it lives: on the volume, or in the area's
//! declared fallback. The location decides only where *new* roots go, so a
//! fallback root a consumer still uses stays listed, holdable and pinnable after
//! the volume returns.

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

pub use crate::area_spec::{Absent, AreaKey, AreaRecord, AreaSpec, AREA_SCHEMA};

use crate::error::{Error, Result};
use crate::hold::{self, Hold, Mode};
use crate::os::TreeSize;
use crate::pin::pin_state;
use crate::root::{
    self, derived_folder, mint_id, read_marker, tree_bytes, Listing, MarkerRead, Root, RootInfo,
    RootMarker, Straggler, ROOT_SCHEMA,
};
use crate::space::{SkippedRoot, SweepReport, SweptRoot};
use crate::volume::{Volume, VolumeState};
use crate::Core;

/// The schema name of a pin record.
pub use crate::pin::PIN_SCHEMA;

mod relocate;

pub(crate) const BARGE_DIR: &str = ".barge";
pub(crate) const TRASH_DIR: &str = ".trash";
pub(crate) const LOCKS_DIR: &str = "locks";
pub(crate) const PINS_DIR: &str = "pins";
pub(crate) const ACTIVITY_DIR: &str = "activity";
const AREA_LOCK: &str = "area.lock";
/// How many attempts a non-waiting hold makes at a refusal that may be transient,
/// and the pause between them. The count is what bounds the retry, not a clock:
/// a scheduler delay stretches the wait but never cuts the attempts short.
const HOLD_ATTEMPTS: u32 = 100;
const HOLD_RETRY_STEP: Duration = Duration::from_millis(2);

/// Where an area's roots live right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// The volume is present; new roots go here.
    Volume(PathBuf),
    /// The volume is absent; new roots go here, visibly.
    Fallback(PathBuf),
    /// No usable location, with the reason a consumer should report.
    Unavailable(String),
}

impl Location {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Volume(path) | Self::Fallback(path) => Some(path),
            Self::Unavailable(_) => None,
        }
    }

    /// True when the location is the registered volume.
    pub fn on_volume(&self) -> bool {
        matches!(self, Self::Volume(_))
    }
}

/// The result of evicting one root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Eviction {
    pub area: String,
    pub id: String,
    pub label: String,
    pub path: PathBuf,
    pub trash_path: PathBuf,
    pub bytes: u64,
    /// True when the tree size could not be measured exactly.
    pub bytes_approximate: bool,
    /// False when the trash copy could not be deleted and is left for a sweep.
    pub deleted: bool,
    /// Why the trash copy stayed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_reason: Option<String>,
}

/// One declared area with the volume it belongs to.
#[derive(Debug, Clone)]
pub struct Area {
    pub(crate) core: Core,
    record: AreaRecord,
    volume: Volume,
}

impl Area {
    pub(crate) fn new(core: Core, record: AreaRecord, volume: Volume) -> Self {
        Self {
            core,
            record,
            volume,
        }
    }

    pub fn record(&self) -> &AreaRecord {
        &self.record
    }

    pub fn key(&self) -> AreaKey {
        self.record.key()
    }

    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Where new roots go, or why there is nowhere.
    pub fn location(&self) -> Result<Location> {
        let volume_dir = self.volume.base().join(self.record.effective_folder());
        match self.volume.state()? {
            VolumeState::Present => Ok(Location::Volume(volume_dir)),
            VolumeState::Mismatch { registered, found } => Ok(Location::Unavailable(format!(
                "volume at {} is {found}, not the registered {registered}",
                self.volume.spec().mount.display()
            ))),
            VolumeState::Absent => Ok(match &self.record.absent {
                Absent::Fail => Location::Unavailable(format!(
                    "volume {} is absent; attach it at {}",
                    self.volume.name(),
                    self.volume.spec().mount.display()
                )),
                Absent::Fallback { path } => Location::Fallback(path.clone()),
            }),
        }
    }

    /// Create a root with a fresh ID and its marker.
    pub fn create_root(&self, label: &str) -> Result<Root> {
        self.create_root_with_expiry(label, None)
    }

    /// Adopt an existing directory directly inside this area's volume location
    /// or declared fallback. Adoption records ownership without changing its
    /// contents.
    pub fn adopt_root(&self, folder: &str, label: &str, id: &str) -> Result<Root> {
        self.core.ensure_writable()?;
        root::validate_label(label)?;
        root::validate_id(id)?;
        if !root::valid_folder(folder) {
            return Err(Error::usage(format!(
                "root folder {folder:?} must be one plain name without separators, control characters, '.' or '..'"
            )));
        }
        if matches!(folder, BARGE_DIR | TRASH_DIR) {
            return Err(Error::refused(format!(
                "root folder {folder:?} is reserved by Barge"
            )));
        }
        let key = self.key().label();
        let mut candidate = None;
        for (directory, _) in self.search_dirs()? {
            let path = directory.join(folder);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    check_adoptable(&path, &metadata)?;
                    if candidate.is_none() {
                        candidate = Some((directory, path));
                    }
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::io("cannot inspect", &path, &source)),
            }
        }
        let (_directory, path) = candidate.ok_or_else(|| {
            Error::not_found(format!(
                "root folder {folder:?} does not exist directly inside area {}",
                self.key().label()
            ))
        })?;

        #[cfg(test)]
        ADOPT_HOOK.with(|hook| {
            let callback = hook.borrow_mut().take();
            if let Some(hook) = callback {
                hook();
            }
        });

        let directories = self.search_dirs()?;
        let mut locks = Vec::new();
        for (dir, _) in &directories {
            self.ensure_layout(dir)?;
            if !locks
                .iter()
                .any(|(locked, _): &(PathBuf, AreaLock)| locked == dir)
            {
                locks.push((dir.clone(), AreaLock::acquire(dir)?));
            }
        }
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| Error::io("cannot inspect", &path, &e))?;
        check_adoptable(&path, &metadata)?;
        match read_marker(&path) {
            MarkerRead::Valid(marker) => {
                let differences = marker_differences(&marker, &key, id, label, folder);
                if differences.is_empty() {
                    return Ok(root_from_marker(path, marker));
                }
                return Err(Error::refused(format!(
                    "root marker differs in {}",
                    differences.join(", ")
                )));
            }
            MarkerRead::Invalid(reason) => {
                return Err(Error::refused(format!(
                    "cannot adopt {}: {reason}",
                    path.display()
                )))
            }
            MarkerRead::Missing => {}
        }
        // Two adoptions can find the folder free at the same moment, so the
        // duplicate-ID check belongs under the lock that serialises them: an
        // earlier reading never authorises a second root with the same ID.
        for info in self.list_without_bytes()?.roots {
            if info.id == id && info.path != path {
                return Err(Error::refused(format!(
                    "root id {id} is already used by folder {} in area {}",
                    info.path.display(),
                    key
                )));
            }
        }
        let marker = RootMarker {
            schema: crate::root::ADOPTED_ROOT_SCHEMA.to_string(),
            area: key,
            id: id.to_string(),
            label: label.to_string(),
            created_unix_ms: self.core.clock.now_unix_ms(),
            expires_unix_ms: None,
            folder: Some(folder.to_string()),
        };
        root::write_marker(&path, &marker)?;
        Ok(root_from_marker(path, marker))
    }

    /// Create a root that expires at `expires_unix_ms` (scratch uses this).
    pub(crate) fn create_root_with_expiry(
        &self,
        label: &str,
        expires_unix_ms: Option<u64>,
    ) -> Result<Root> {
        self.core.ensure_writable()?;
        root::validate_label(label)?;
        let directory = self.available_dir()?;
        self.ensure_layout(&directory)?;
        let _area_lock = AreaLock::acquire(&directory)?;
        let now = self.core.clock.now_unix_ms();
        let mut attempts = 0;
        loop {
            let id = mint_id()?;
            let path = directory.join(derived_folder(label, &id));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let marker = RootMarker {
                        schema: ROOT_SCHEMA.to_string(),
                        area: self.key().label(),
                        id: id.clone(),
                        label: label.to_string(),
                        created_unix_ms: now,
                        expires_unix_ms,
                        folder: None,
                    };
                    if let Err(error) = root::write_marker(&path, &marker) {
                        // The folder was created a moment ago and holds nothing
                        // but a partial marker, so it is deleted through the
                        // owned walk: a replacement at the path is never walked.
                        let _ = crate::trash::remove_half_made(&path);
                        return Err(error);
                    }
                    self.record_activity(&directory, &id, now);
                    let folder = derived_folder(label, &id);
                    return Ok(Root {
                        id,
                        label: label.to_string(),
                        path,
                        folder,
                        created_unix_ms: now,
                        expires_unix_ms,
                    });
                }
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempts += 1;
                    if attempts >= 3 {
                        return Err(Error::io("cannot create a root in", &path, &source));
                    }
                }
                Err(source) => return Err(Error::io("cannot create a root in", &path, &source)),
            }
        }
    }

    /// List owned roots and stragglers without walking root contents.
    pub fn list_without_bytes(&self) -> Result<Listing> {
        self.list_impl(false)
    }

    /// List owned roots and stragglers, including the size of each root tree.
    pub fn list(&self) -> Result<Listing> {
        self.list_impl(true)
    }

    fn list_impl(&self, include_bytes: bool) -> Result<Listing> {
        #[cfg(test)]
        LIST_CALLS.with(|calls| calls.set(calls.get() + 1));
        let mut listing = Listing::default();
        let key = self.key().label();
        for (directory, on_volume, path, marker_read) in self.scan()? {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let barge = directory.join(BARGE_DIR);
            let locks = barge.join(LOCKS_DIR);
            let activity = barge.join(ACTIVITY_DIR);
            match marker_read {
                MarkerRead::Missing => listing.stragglers.push(Straggler {
                    path,
                    reason: format!("no {}", root::MARKER_NAME),
                }),
                MarkerRead::Invalid(reason) => listing.stragglers.push(Straggler { path, reason }),
                MarkerRead::Valid(marker) if marker.area != key => {
                    listing.stragglers.push(Straggler {
                        path,
                        reason: format!("its marker belongs to area {}", marker.area),
                    })
                }
                MarkerRead::Valid(marker) => {
                    if name != root::marker_folder(&marker) {
                        listing.stragglers.push(Straggler {
                            path,
                            reason: "its folder name does not match its marker".to_string(),
                        });
                        continue;
                    }
                    let held = hold::is_locked(&locks.join(format!("{}.lock", marker.id)))?;
                    // An unreadable pin is never read as "unpinned": unknown is
                    // never safe. Eviction reports the real reason.
                    let pinned = pin_state(&directory, &marker.id).unwrap_or(true);
                    let last_used = hold::read_activity(&activity, &marker.id)
                        .unwrap_or(marker.created_unix_ms)
                        .max(marker.created_unix_ms);
                    let size = if include_bytes {
                        tree_bytes(&path)
                    } else {
                        TreeSize::default()
                    };
                    listing.roots.push(RootInfo {
                        id: marker.id.clone(),
                        label: marker.label.clone(),
                        path,
                        created_unix_ms: marker.created_unix_ms,
                        last_used_unix_ms: last_used,
                        expires_unix_ms: marker.expires_unix_ms,
                        held,
                        pinned,
                        bytes: size.bytes,
                        bytes_approximate: size.approximate,
                        on_volume,
                    });
                }
            }
        }
        listing.roots.sort_by(|left, right| {
            (left.created_unix_ms, &left.id).cmp(&(right.created_unix_ms, &right.id))
        });
        listing
            .stragglers
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(listing)
    }

    pub(crate) fn now_unix_ms(&self) -> u64 {
        self.core.clock.now_unix_ms()
    }

    /// The bytes the area's owned roots hold.
    pub fn bytes(&self) -> Result<u64> {
        Ok(self.list()?.bytes())
    }

    /// A root's bytes: its recorded size when one was taken completely, its
    /// measured size otherwise. Only complete measurements are recorded, so a
    /// cached value is never a lower bound reported as exact.
    pub(crate) fn recorded_or_measure_bytes(&self, info: &RootInfo, persist: bool) -> TreeSize {
        let Ok(directory) = parent(&info.path) else {
            return tree_bytes(&info.path);
        };
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        if let Some(bytes) = hold::read_activity_bytes(&activity, &info.id) {
            return TreeSize {
                bytes,
                approximate: false,
            };
        }
        let identity = crate::os::path_identity(&info.path).ok().flatten();
        let size = tree_bytes(&info.path);
        if persist {
            // A complete measurement is cached; an incomplete one clears any
            // earlier cached size rather than leaving a stale number behind.
            let recorded = if size.approximate {
                hold::RecordedSize::Unknown
            } else {
                hold::RecordedSize::Exact(size.bytes)
            };
            let key = self.key().label();
            let measurement = hold::Measurement {
                root_id: &info.id,
                path: &info.path,
                identity,
                size: recorded,
                unix_ms: info.last_used_unix_ms,
            };
            let _ = hold::record_size_if_unchanged(&directory, &measurement, |marker| {
                marker.id == info.id && marker.area == key
            });
        }
        size
    }

    fn scan(&self) -> Result<Vec<(PathBuf, bool, PathBuf, MarkerRead)>> {
        let mut found = Vec::new();
        for (directory, on_volume) in self.search_dirs()? {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::io("cannot list the area at", &directory, &e)),
            };
            for entry in entries {
                let entry =
                    entry.map_err(|e| Error::io("cannot list the area at", &directory, &e))?;
                if matches!(entry.file_name().to_str(), Some(BARGE_DIR | TRASH_DIR)) {
                    continue;
                }
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::io("cannot inspect", &path, &e)),
                };
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    let marker = read_marker(&path);
                    found.push((directory.clone(), on_volume, path, marker));
                }
            }
        }
        Ok(found)
    }

    /// Find a root by its ID, on the volume or in the fallback.
    pub fn resolve(&self, id: &str) -> Result<Root> {
        root::validate_id(id)?;
        let key = self.key().label();
        for (_, _, path, marker) in self.scan()? {
            if let MarkerRead::Valid(marker) = marker {
                if marker.area == key
                    && marker.id == id
                    && path
                        .file_name()
                        .is_some_and(|name| name == root::marker_folder(&marker).as_str())
                {
                    return Ok(root_from_marker(path, marker));
                }
            }
        }
        Err(Error::not_found(format!(
            "no root {id} in area {}",
            self.key().label()
        )))
    }

    /// Hold a root, waiting for another holder to finish.
    pub fn hold(&self, id: &str, mode: Mode) -> Result<Hold> {
        self.core.ensure_writable()?;
        let root = self.resolve(id)?;
        let directory = self.hold_directory(&root)?;
        let lock_path = hold::hold_lock_path(&directory, &root.id);
        let file = hold::lock_root_waiting(&lock_path, mode)?;
        self.begin_hold(root, directory, file, mode)
    }

    /// Try to hold a root without waiting. `None` means something else holds it.
    ///
    /// A refusal can be transient. A listing probes a lock exclusively for a
    /// moment, and a child that another thread of this process spawns keeps a
    /// copy of every descriptor open at its fork until it execs. So a refusal is
    /// retried up to `HOLD_ATTEMPTS` times, `HOLD_RETRY_STEP` apart, which is
    /// about 200 ms on an idle machine. The hold lands if contention clears
    /// within those attempts, including a return that releases early. A holder
    /// still present after the last attempt gets `None`. The caller waits only
    /// that long and does not wait for a return to finish. Callers recheck lease
    /// state under the hold they get.
    pub fn try_hold(&self, id: &str, mode: Mode) -> Result<Option<Hold>> {
        self.core.ensure_writable()?;
        for attempt in 1..=HOLD_ATTEMPTS {
            let root = self.resolve(id)?;
            let directory = self.hold_directory(&root)?;
            let lock_path = hold::hold_lock_path(&directory, &root.id);
            if let Some(file) = hold::lock_root_try(&lock_path, mode)? {
                return self.begin_hold(root, directory, file, mode).map(Some);
            }
            if attempt < HOLD_ATTEMPTS {
                std::thread::sleep(HOLD_RETRY_STEP);
            }
        }
        Ok(None)
    }

    /// The directory a root's hold lock and activity records live in.
    fn hold_directory(&self, root: &Root) -> Result<PathBuf> {
        let directory = parent(&root.path)?;
        self.ensure_layout(&directory)?;
        Ok(directory)
    }

    /// Finish an acquired hold, after checking the root is still the one that was
    /// resolved.
    ///
    /// An eviction holds this same lock across its rename, so a waiting hold can
    /// wake up on a root that no longer exists; it must not report a hold on a
    /// path a later root may use. Dropping `file` releases the lock in every
    /// early return.
    fn begin_hold(&self, root: Root, directory: PathBuf, file: File, mode: Mode) -> Result<Hold> {
        let current = self.resolve(&root.id)?;
        if current.path != root.path {
            return Err(Error::not_found(format!(
                "root {} in area {} moved while it was being held",
                root.id,
                self.key().label()
            )));
        }
        let _area_lock = AreaLock::acquire(&directory)?;
        let marker = match read_marker(&current.path) {
            MarkerRead::Valid(marker)
                if marker.id == root.id && marker.area == self.key().label() =>
            {
                marker
            }
            _ => return Err(Error::not_found("root marker changed while acquiring hold")),
        };
        let root_directory = crate::os::open_directory(&current.path)
            .map_err(|e| Error::io("cannot open root", &current.path, &e))?;
        self.record_activity(&directory, &root.id, self.core.clock.now_unix_ms());
        Ok(Hold::new(
            mode,
            file,
            current.path,
            marker,
            root_directory,
            self.core.clock.clone(),
        ))
    }

    /// Evict a root: refuse it when it is held or pinned, otherwise rename it
    /// into the area's trash and delete it there.
    pub fn evict(&self, id: &str) -> Result<Eviction> {
        self.core.ensure_writable()?;
        let root = self.resolve(id)?;
        self.evict_resolved(root)
    }

    /// Evict a root a listing already resolved, without reading the area again.
    ///
    /// A prune walks its candidates once; re-resolving each one before evicting
    /// it would make the run quadratic in the area's size. The root's marker is
    /// revalidated under both locks before the rename, so an earlier listing
    /// never authorises an eviction by itself.
    pub(crate) fn evict_listed(&self, info: &RootInfo) -> Result<Eviction> {
        self.core.ensure_writable()?;
        self.evict_resolved(root_from_info(info))
    }

    fn evict_resolved(&self, root: Root) -> Result<Eviction> {
        let key = self.key().label();
        let id = root.id.clone();
        #[cfg(test)]
        EVICT_HOOK.with(|hook| {
            let callback = hook.borrow_mut().take();
            if let Some(hook) = callback {
                hook();
            }
        });
        let directory = self.hold_directory(&root)?;
        let lock_path = hold::hold_lock_path(&directory, &id);
        let mut hold_lock = None;
        for _ in 0..2 {
            hold_lock = hold::lock_root_try(&lock_path, Mode::Exclusive)?;
            if hold_lock.is_some() {
                break;
            }
            // A listing can briefly hold the lock for its own probe.
            #[cfg(test)]
            EVICT_PROBE_HOOK.with(|hook| {
                if let Some(callback) = hook.borrow_mut().take() {
                    callback();
                }
            });
        }
        let Some(hold_lock) = hold_lock else {
            return Err(Error::refused(format!(
                "root {id} is held; refusing to evict it"
            )));
        };
        let area_lock = AreaLock::acquire(&directory)?;
        if pin_state(&directory, &id)? {
            return Err(Error::refused(format!(
                "root {id} is pinned; refusing to evict it"
            )));
        }
        // The size is never measured here: walking the tree under the area lock
        // would hold it for as long as the data takes to read. The deletion
        // below counts what it removes, and a release's recorded size covers a
        // deletion that leaves part of the tree in the trash.
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        let recorded = hold::read_activity_bytes(&activity, &id);
        let trash = directory.join(TRASH_DIR);
        fs::create_dir_all(&trash)
            .map_err(|source| Error::io("cannot create the trash at", &trash, &source))?;
        if !unchanged_marker(&root, &key) {
            return Err(Error::refused("root marker changed before eviction"));
        }
        let mut counter = 0_u64;
        let trash_path = loop {
            let candidate = if counter == 0 {
                trash.join(&root.folder)
            } else {
                trash.join(format!("{}.{counter}", root.folder))
            };
            counter += 1;
            match crate::os::rename_no_replace(&root.path, &candidate) {
                Ok(()) => break candidate,
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(Error::io("cannot evict the root to", &candidate, &source))
                }
            }
        };
        // The root is renamed: a waiting holder wakes to a missing root and the
        // lock file is no longer useful. Unlinking it does not release our lock;
        // dropping the descriptor does.
        let _ = fs::remove_file(&lock_path);
        let _ = fs::remove_file(hold::activity_path(&activity, &id));
        let _ = fs::remove_file(crate::pin::pin_path(&directory, &id));
        // Open the trash entry and take the exclusive lock the deletion holds,
        // before the area lock is released: a sweep claims only under that lock,
        // so from here neither a claim nor an empty reap can touch a folder this
        // eviction is still walking.
        let tree = crate::os::Tree::open(&trash_path).map_err(|error| {
            Error::io_message(format!(
                "{}; the evicted root remains in trash at {}",
                error.message(),
                trash_path.display()
            ))
        })?;
        if !crate::os::flock(tree.directory(), Mode::Exclusive, false)? {
            return Err(Error::unknown_state(format!(
                "{} is held by another deletion; the evicted root stays in the trash for a sweep",
                trash_path.display()
            )));
        }
        // The rename is done and the hold lock's file is unlinked, so it protects
        // nothing more. The area lock is also released before the tree delete; the
        // entry keeps its own lock until the walk is done.
        drop(hold_lock);
        drop(area_lock);
        let (deleted, kept_reason, size) = match crate::trash::remove_evicted_tree(tree, &key, &id)
        {
            // The walk counted every byte it removed, so the size is exact.
            Ok(removed) if removed.removed => (
                true,
                None,
                TreeSize {
                    bytes: removed.bytes,
                    approximate: false,
                },
            ),
            Ok(removed) => (
                false,
                Some(crate::trash::stayed_reason(&removed)),
                recorded.map_or_else(
                    || tree_bytes(&trash_path),
                    |bytes| TreeSize {
                        bytes,
                        approximate: false,
                    },
                ),
            ),
            Err(error) => {
                return Err(Error::io_message(format!(
                    "{}; evicted root remains in trash at {}",
                    error.message(),
                    trash_path.display()
                )))
            }
        };
        Ok(Eviction {
            area: key,
            id,
            label: root.label,
            path: root.path,
            trash_path,
            bytes: size.bytes,
            bytes_approximate: size.approximate,
            deleted,
            kept_reason,
        })
    }

    /// Evict expired, idle, unpinned roots. Scratch uses this.
    pub fn sweep_expired(&self) -> Result<SweepReport> {
        self.core.ensure_writable()?;
        let now = self.core.clock.now_unix_ms();
        let mut report = SweepReport::default();
        for info in self.list_without_bytes()?.roots {
            if info.expires_unix_ms.is_none_or(|expires| expires > now) {
                continue;
            }
            if !info.idle_and_unpinned() {
                report.kept.push(SkippedRoot {
                    path: info.path,
                    reason: "held or pinned".to_string(),
                });
                continue;
            }
            match self.evict_listed(&info) {
                Ok(eviction) if eviction.deleted => report.deleted.push(SweptRoot {
                    path: eviction.trash_path,
                    bytes: eviction.bytes,
                }),
                Ok(eviction) => report.kept.push(SkippedRoot {
                    path: eviction.trash_path,
                    reason: eviction
                        .kept_reason
                        .unwrap_or_else(|| "trash retained".into()),
                }),
                Err(error)
                    if matches!(
                        error.kind(),
                        crate::ErrorKind::Refused | crate::ErrorKind::NotFound
                    ) =>
                {
                    report.kept.push(SkippedRoot {
                        path: info.path,
                        reason: error.message().to_string(),
                    })
                }
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }

    /// This area's line in the status document.
    ///
    /// Sizes are the ones recorded when holds were released, and a root with no
    /// record is measured once: a status read never walks every root tree.
    pub fn status(&self) -> Result<AreaStatus> {
        let location = self.location()?;
        let (roots, bytes, stragglers) = match location.path() {
            Some(_) => {
                let listing = self.list_without_bytes()?;
                let bytes = listing
                    .roots
                    .iter()
                    .map(|info| self.recorded_or_measure_bytes(info, false).bytes)
                    .fold(0_u64, u64::saturating_add);
                (listing.roots.len(), bytes, listing.stragglers.len())
            }
            None => (0, 0, 0),
        };
        let displaced_at = self.displaced_folder()?;
        let (path, fallback, unavailable) = match &location {
            Location::Volume(path) => (Some(path.clone()), false, None),
            Location::Fallback(path) => (Some(path.clone()), true, None),
            Location::Unavailable(reason) => (None, false, Some(reason.clone())),
        };
        Ok(AreaStatus {
            namespace: self.record.namespace.clone(),
            name: self.record.name.clone(),
            volume: self.record.volume.clone(),
            folder: self.record.effective_folder(),
            absent: self.record.absent.clone(),
            soft_cap_bytes: self.record.soft_cap_bytes,
            counts_toward_reserve: self.record.counts_toward_reserve,
            location: path,
            fallback,
            roots,
            bytes,
            stragglers,
            unavailable,
            displaced_at,
        })
    }

    /// The directory new roots go in, refusing an unavailable area.
    fn available_dir(&self) -> Result<PathBuf> {
        match self.location()? {
            Location::Volume(path) | Location::Fallback(path) => Ok(path),
            Location::Unavailable(reason) => Err(Error::refused(format!(
                "area {} is unavailable: {reason}",
                self.key().label()
            ))),
        }
    }

    /// Where this area's roots may exist: the volume when it is present, and the
    /// declared fallback always.
    pub(crate) fn search_dirs(&self) -> Result<Vec<(PathBuf, bool)>> {
        let mut directories = Vec::new();
        if self.volume.state()? == VolumeState::Present {
            directories.push((
                self.volume.base().join(self.record.effective_folder()),
                true,
            ));
        }
        if let Absent::Fallback { path } = &self.record.absent {
            directories.push((path.clone(), false));
        }
        Ok(directories)
    }

    pub(crate) fn ensure_layout(&self, directory: &Path) -> Result<()> {
        let barge = directory.join(BARGE_DIR);
        for folder in [
            barge.join(LOCKS_DIR),
            barge.join(PINS_DIR),
            barge.join(ACTIVITY_DIR),
        ] {
            fs::create_dir_all(&folder)
                .map_err(|source| Error::io("cannot create", &folder, &source))?;
        }
        Ok(())
    }

    fn record_activity(&self, directory: &Path, id: &str, unix_ms: u64) {
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        if fs::create_dir_all(&activity).is_ok() {
            // Advisory only: a failure makes the root look older, never unsafe.
            let _ = hold::record_activity(&activity, id, unix_ms, hold::RecordedSize::Unchanged);
        }
    }
}

/// One area's line in the status document.
#[derive(Debug, Clone, Serialize)]
pub struct AreaStatus {
    pub namespace: String,
    pub name: String,
    pub volume: String,
    pub folder: String,
    pub absent: Absent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_cap_bytes: Option<u64>,
    pub counts_toward_reserve: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<PathBuf>,
    pub fallback: bool,
    pub roots: usize,
    pub bytes: u64,
    pub stragglers: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// Where this area's roots are when its recorded folder is missing: a
    /// relocation that stopped before its declaration was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub displaced_at: Option<PathBuf>,
}

/// The area lock: pinning, unpinning, eviction and the trash sweep take it, so a
/// pin cannot land between an eviction's pin recheck and its rename.
#[derive(Debug)]
pub(crate) struct AreaLock {
    /// Held only for the lock its descriptor carries, so it is never read.
    _file: File,
}

impl AreaLock {
    pub(crate) fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(BARGE_DIR).join(AREA_LOCK);
        let file = crate::os::open_lock(&path, "cannot open the area lock")?;
        crate::os::flock(&file, Mode::Exclusive, true)?;
        Ok(Self { _file: file })
    }

    /// Take the area lock, waiting no longer than `wait`. `None` means another
    /// holder kept it for that long, so the caller can skip advisory work.
    pub(crate) fn acquire_within(directory: &Path, wait: Duration) -> Result<Option<Self>> {
        let path = directory.join(BARGE_DIR).join(AREA_LOCK);
        let started = Instant::now();
        loop {
            let file = crate::os::open_lock(&path, "cannot open the area lock")?;
            if crate::os::flock(&file, Mode::Exclusive, false)? {
                return Ok(Some(Self { _file: file }));
            }
            if started.elapsed() >= wait {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

pub(crate) fn parent(path: &Path) -> Result<PathBuf> {
    path.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::unknown_state(format!("{} has no parent folder", path.display())))
}

/// Check that a folder is one adoption may record: a real directory, owned by
/// the calling user and writable by neither group nor others.
///
/// Adoption checks this twice on purpose: once to pick the candidate, and again
/// under the area lock, because a racing writer can replace the folder between
/// the two. Both readings ask the same question, so they share one check.
fn check_adoptable(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::refused(format!(
            "root folder {} is not a real directory",
            path.display()
        )));
    }
    if metadata.uid() != crate::os::effective_uid() {
        return Err(Error::refused(format!(
            "root folder {} is not owned by the calling user",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(Error::refused(format!(
            "root folder {} is group- or other-writable",
            path.display()
        )));
    }
    Ok(())
}

fn root_from_marker(path: PathBuf, marker: RootMarker) -> Root {
    Root {
        id: marker.id.clone(),
        label: marker.label.clone(),
        folder: root::marker_folder(&marker),
        path,
        created_unix_ms: marker.created_unix_ms,
        expires_unix_ms: marker.expires_unix_ms,
    }
}

/// The root a listing resolved, without reading the area again.
fn root_from_info(info: &RootInfo) -> Root {
    Root {
        id: info.id.clone(),
        label: info.label.clone(),
        folder: info
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: info.path.clone(),
        created_unix_ms: info.created_unix_ms,
        expires_unix_ms: info.expires_unix_ms,
    }
}

/// The fields in which a marker disagrees with the area, ID, label and folder a
/// caller expects. Empty means the marker names exactly that root.
pub(crate) fn marker_differences(
    marker: &RootMarker,
    area: &str,
    id: &str,
    label: &str,
    folder: &str,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if marker.area != area {
        fields.push("area");
    }
    if marker.id != id {
        fields.push("id");
    }
    if marker.label != label {
        fields.push("label");
    }
    if root::marker_folder(marker) != folder {
        fields.push("folder");
    }
    fields
}

/// Whether a resolved root's marker still names its area, ID, label and folder.
/// Eviction and pinning both hold the area lock when they ask, so a `false` here
/// means the root was replaced since it was resolved.
pub(crate) fn marker_matches(
    marker: &RootMarker,
    area: &str,
    id: &str,
    label: &str,
    folder: &str,
) -> bool {
    marker_differences(marker, area, id, label, folder).is_empty()
}

pub(crate) fn unchanged_marker(root: &Root, area: &str) -> bool {
    matches!(read_marker(&root.path), MarkerRead::Valid(marker) if marker_matches(&marker, area, &root.id, &root.label, &root.folder))
}

#[cfg(test)]
thread_local! {
    static EVICT_PROBE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    pub(crate) static EVICT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    pub(crate) static PIN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static ADOPT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static LIST_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_list_calls() {
    LIST_CALLS.with(|cells| cells.set(0));
}

#[cfg(test)]
fn list_calls() -> usize {
    LIST_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod relocate_tests;
#[cfg(test)]
mod tests;
