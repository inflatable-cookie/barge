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

pub(crate) const BARGE_DIR: &str = ".barge";
pub(crate) const TRASH_DIR: &str = ".trash";
pub(crate) const LOCKS_DIR: &str = "locks";
pub(crate) const PINS_DIR: &str = "pins";
pub(crate) const ACTIVITY_DIR: &str = "activity";
const AREA_LOCK: &str = "area.lock";

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
        self.core.ensure_writable()?;
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
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(Error::refused(format!(
                            "root folder {} is not a real directory",
                            path.display()
                        )));
                    }
                    if candidate.is_none() {
                        candidate = Some((directory, path, metadata));
                    }
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::io("cannot inspect", &path, &source)),
            }
        }
        let (_directory, path, metadata) = candidate.ok_or_else(|| {
            Error::not_found(format!(
                "root folder {folder:?} does not exist directly inside area {}",
                self.key().label()
            ))
        })?;
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
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != crate::os::effective_uid()
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(Error::refused(format!(
                "root folder {} is not a safely owned directory",
                path.display()
            )));
        }
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
                        let _ = fs::remove_dir_all(&path);
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
            if let Ok(_lock) = AreaLock::acquire(&directory) {
                if identity.is_some()
                    && crate::os::path_identity(&info.path).ok().flatten() == identity
                    && matches!(read_marker(&info.path), MarkerRead::Valid(ref marker) if marker.id == info.id && marker.area == self.key().label())
                {
                    let _ = hold::record_activity(
                        &activity,
                        &info.id,
                        info.last_used_unix_ms,
                        recorded,
                    );
                }
            }
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
        let file = lock_root_waiting(&hold_lock_path(&directory, &root.id), mode)?;
        self.begin_hold(root, directory, file, mode)
    }

    /// Try to hold a root without waiting. `None` means something else holds it.
    pub fn try_hold(&self, id: &str, mode: Mode) -> Result<Option<Hold>> {
        self.core.ensure_writable()?;
        for _ in 0..2 {
            let root = self.resolve(id)?;
            let directory = self.hold_directory(&root)?;
            let lock_path = hold_lock_path(&directory, &root.id);
            let Some(file) = lock_root_try(&lock_path, mode)? else {
                // Listing a root probes its lock exclusively to report it as held,
                // so a probe running right now can make one non-waiting attempt
                // fail spuriously. The second attempt covers that window; a real
                // holder still wins.
                continue;
            };
            return self.begin_hold(root, directory, file, mode).map(Some);
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
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
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
            activity,
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
        let lock_path = hold_lock_path(&directory, &id);
        let Some(hold_lock) = lock_root_try(&lock_path, Mode::Exclusive)? else {
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
        let size = tree_bytes(&root.path);
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
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        let _ = fs::remove_file(hold::activity_path(&activity, &id));
        let _ = fs::remove_file(crate::pin::pin_path(&directory, &id));
        // The rename is done and the hold lock's file is unlinked, so it protects
        // nothing more. The area lock is also released before the tree delete.
        drop(hold_lock);
        drop(area_lock);
        let (deleted, kept_reason) = match crate::trash::remove_evicted_tree(&trash_path, &key, &id)
        {
            Ok(removed) if removed.removed => (true, None),
            Ok(removed) => (false, Some(crate::trash::stayed_reason(&removed))),
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
}

pub(crate) fn parent(path: &Path) -> Result<PathBuf> {
    path.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::unknown_state(format!("{} has no parent folder", path.display())))
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

/// The hold lock file a root's ID uses inside an area directory.
pub(crate) fn hold_lock_path(directory: &Path, id: &str) -> PathBuf {
    directory
        .join(BARGE_DIR)
        .join(LOCKS_DIR)
        .join(format!("{id}.lock"))
}

/// Open a hold lock and take it, waiting for every other holder to finish.
///
/// Eviction unlinks `<id>.lock` while it holds it, so a waiting hold can wake
/// holding a file nobody else can see; a caller-chosen folder re-adopted with the
/// same ID would then reopen and lock the same root twice. A descriptor that does
/// not match the path is therefore dropped and the attempt repeated once. A
/// waiting `flock` returns only once the lock is held, so that repeat is the only
/// case that reaches a second attempt.
pub(crate) fn lock_root_waiting(lock_path: &Path, mode: Mode) -> Result<File> {
    let mut attempt = 0;
    while attempt < 2 {
        attempt += 1;
        let file = open_hold_lock(lock_path)?;
        crate::os::flock(&file, mode, true)?;
        run_lock_hook();
        if lock_is_current(&file, lock_path)? {
            return Ok(file);
        }
    }
    Err(replaced_lock(lock_path))
}

/// Open a hold lock and take it only if it is free. `None` means another holder
/// has it.
pub(crate) fn lock_root_try(lock_path: &Path, mode: Mode) -> Result<Option<File>> {
    let mut attempt = 0;
    while attempt < 2 {
        attempt += 1;
        let file = open_hold_lock(lock_path)?;
        if !crate::os::flock(&file, mode, false)? {
            return Ok(None);
        }
        run_lock_hook();
        if lock_is_current(&file, lock_path)? {
            return Ok(Some(file));
        }
    }
    Err(replaced_lock(lock_path))
}

/// Open the hold lock file, past the window an eviction exploits: the file is
/// open, the lock is not yet taken, so an eviction may rename the root, unlink
/// this file and a caller may re-adopt the folder with the same ID before the
/// lock lands.
fn open_hold_lock(lock_path: &Path) -> Result<File> {
    let file = crate::os::open_lock(lock_path, "cannot open the hold lock")?;
    #[cfg(test)]
    LOCK_OPEN_HOOK.with(|hooks| {
        let hook = hooks.borrow_mut().pop();
        if let Some(hook) = hook {
            hook();
        }
    });
    Ok(file)
}

/// The test seam after a lock lands, before the descriptor is verified.
#[cfg(test)]
fn run_lock_hook() {
    LOCK_HOOK.with(|hooks| {
        let hook = hooks.borrow_mut().pop();
        if let Some(hook) = hook {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_lock_hook() {}

fn replaced_lock(lock_path: &Path) -> Error {
    Error::unknown_state(format!(
        "the hold lock {} was replaced while it was being taken; refusing to hold it",
        lock_path.display()
    ))
}

fn lock_is_current(file: &File, path: &Path) -> Result<bool> {
    let held = crate::os::file_identity(file)
        .map_err(|source| Error::io("cannot inspect the hold lock", path, &source))?;
    let now = crate::os::path_identity(path)
        .map_err(|source| Error::io("cannot inspect the hold lock", path, &source))?;
    Ok(now == Some(held))
}

#[cfg(test)]
thread_local! {
    pub(crate) static EVICT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    pub(crate) static PIN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static ADOPT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static LOCK_HOOK: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(Vec::new()) };
    static LOCK_OPEN_HOOK: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(Vec::new()) };
    static LIST_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A test hook that runs between opening a hold lock file and taking its lock.
#[cfg(test)]
fn push_lock_open_hook(hook: impl FnOnce() + 'static) {
    LOCK_OPEN_HOOK.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

/// A test hook that runs after a hold lock is taken, before it is verified.
#[cfg(test)]
fn push_lock_hook(hook: impl FnOnce() + 'static) {
    LOCK_HOOK.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
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
mod tests;
