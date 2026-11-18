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
        let (directory, path, metadata) = candidate.ok_or_else(|| {
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

        self.ensure_layout(&directory)?;
        let _lock = AreaLock::acquire(&directory)?;
        match read_marker(&path) {
            MarkerRead::Valid(marker) => {
                let mut differences = Vec::new();
                if marker.area != key {
                    differences.push("area");
                }
                if marker
                    .folder
                    .as_deref()
                    .unwrap_or(&derived_folder(&marker.label, &marker.id))
                    != folder
                {
                    differences.push("folder");
                }
                if marker.label != label {
                    differences.push("label");
                }
                if marker.id != id {
                    differences.push("id");
                }
                if marker_matches(&marker, &key, id, label, folder) {
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
        let size = tree_bytes(&info.path);
        if persist {
            // A complete measurement is cached; an incomplete one clears any
            // earlier cached size rather than leaving a stale number behind.
            let recorded = if size.approximate {
                hold::RecordedSize::Unknown
            } else {
                hold::RecordedSize::Exact(size.bytes)
            };
            let _ = hold::record_activity(&activity, &info.id, info.last_used_unix_ms, recorded);
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
        self.acquire(id, mode, true)?.ok_or_else(|| {
            Error::unknown_state(format!("a waiting hold on root {id} was not acquired"))
        })
    }

    /// Try to hold a root without waiting. `None` means something else holds it.
    pub fn try_hold(&self, id: &str, mode: Mode) -> Result<Option<Hold>> {
        self.core.ensure_writable()?;
        if let Some(hold) = self.acquire(id, mode, false)? {
            return Ok(Some(hold));
        }
        // Listing a root probes its lock exclusively to report it as held, so a
        // probe running right now can make one non-waiting attempt fail
        // spuriously. One retry covers that window; a real holder still wins.
        self.acquire(id, mode, false)
    }

    fn acquire(&self, id: &str, mode: Mode, wait: bool) -> Result<Option<Hold>> {
        let root = self.resolve(id)?;
        let directory = parent(&root.path)?;
        self.ensure_layout(&directory)?;
        let lock_path = directory
            .join(BARGE_DIR)
            .join(LOCKS_DIR)
            .join(format!("{id}.lock"));
        let Some(file) = lock_root(&lock_path, mode, wait)? else {
            return Ok(None);
        };
        // An eviction holds this same lock across its rename. A waiting hold can
        // therefore wake up on a root that no longer exists; it must not report
        // a hold on a path a later root may use. Dropping `file` releases the
        // lock in every early return.
        let current = self.resolve(id)?;
        if current.path != root.path {
            return Err(Error::not_found(format!(
                "root {id} in area {} moved while it was being held",
                self.key().label()
            )));
        }
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        self.record_activity(&directory, id, self.core.clock.now_unix_ms());
        Ok(Some(Hold::new(
            id.to_string(),
            mode,
            file,
            activity,
            current.path,
            self.core.clock.clone(),
        )))
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
            match self.evict(&info.id) {
                Ok(eviction) => report.deleted.push(SweptRoot {
                    path: eviction.trash_path,
                    bytes: eviction.bytes,
                }),
                Err(error) => report.kept.push(SkippedRoot {
                    path: info.path,
                    reason: error.message().to_string(),
                }),
            }
        }
        Ok(report)
    }

    /// This area's line in the status document.
    pub fn status(&self) -> Result<AreaStatus> {
        let location = self.location()?;
        let (roots, bytes, stragglers) = match location.path() {
            Some(_) => {
                let listing = self.list()?;
                (
                    listing.roots.len(),
                    listing.bytes(),
                    listing.stragglers.len(),
                )
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
    #[allow(dead_code)]
    file: File,
}

impl AreaLock {
    pub(crate) fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(BARGE_DIR).join(AREA_LOCK);
        let file = crate::os::open_lock(&path, "cannot open the area lock")?;
        crate::os::flock(&file, Mode::Exclusive, true)?;
        Ok(Self { file })
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
    marker.area == area
        && marker.id == id
        && marker.label == label
        && root::marker_folder(marker) == folder
}

pub(crate) fn unchanged_marker(root: &Root, area: &str) -> bool {
    matches!(read_marker(&root.path), MarkerRead::Valid(marker) if marker_matches(&marker, area, &root.id, &root.label, &root.folder))
}

/// Open the root's hold lock and take it, then require the descriptor to be the
/// file the path names now.
///
/// Eviction unlinks `<id>.lock` while it holds it, so a waiting hold can wake
/// holding a file nobody else can see; a caller-chosen folder re-adopted with
/// the same ID would then reopen and lock the same root twice. A descriptor that
/// does not match the path is dropped and the attempt repeated once; a second
/// mismatch is refused.
pub(crate) fn lock_root(lock_path: &Path, mode: Mode, wait: bool) -> Result<Option<File>> {
    for attempt in 0..2 {
        let file = crate::os::open_lock(lock_path, "cannot open the hold lock")?;
        // The window an eviction exploits: the file is open, the lock is not yet
        // taken, so an eviction may rename the root, unlink this file and a
        // caller may re-adopt the folder with the same ID before the lock lands.
        #[cfg(test)]
        LOCK_OPEN_HOOK.with(|hooks| {
            let hook = hooks.borrow_mut().pop();
            if let Some(hook) = hook {
                hook();
            }
        });
        if !crate::os::flock(&file, mode, wait)? {
            return Ok(None);
        }
        #[cfg(test)]
        LOCK_HOOK.with(|hooks| {
            let hook = hooks.borrow_mut().pop();
            if let Some(hook) = hook {
                hook();
            }
        });
        if lock_is_current(&file, lock_path)? {
            return Ok(Some(file));
        }
        // `file` falls out of scope at the end of the attempt, releasing the lock
        // on the file the path no longer names.
        if attempt == 1 {
            return Err(Error::unknown_state(format!(
                "the hold lock {} was replaced while it was being taken; refusing to hold it",
                lock_path.display()
            )));
        }
    }
    Err(Error::unknown_state(format!(
        "the hold lock {} could not be taken",
        lock_path.display()
    )))
}

fn lock_is_current(file: &File, path: &Path) -> Result<bool> {
    let held = crate::os::file_identity(file)
        .map_err(|source| Error::io("cannot inspect the hold lock", path, &source))?;
    let now = crate::os::path_identity(path)
        .map_err(|source| Error::io("cannot inspect the hold lock", path, &source))?;
    Ok(now == Some(held))
}

pub(crate) fn unique_trash_path(trash: &Path, name: &str) -> PathBuf {
    let mut candidate = trash.join(name);
    let mut counter = 0;
    while candidate.exists() {
        counter += 1;
        candidate = trash.join(format!("{name}.{counter}"));
    }
    candidate
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

#[cfg(test)]
fn push_lock_hook(hook: impl FnOnce() + 'static) {
    LOCK_HOOK.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

#[cfg(test)]
fn push_lock_open_hook(hook: impl FnOnce() + 'static) {
    LOCK_OPEN_HOOK.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

#[cfg(test)]
fn reset_list_calls() {
    LIST_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn list_calls() -> usize {
    LIST_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod eviction_identity_tests {
    use super::*;
    use crate::{Barge, ManualClock, VolumeRegistration};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;

    fn fixture() -> (crate::testing::TempDir, Barge) {
        fixture_with_space(0, 1)
    }

    fn fixture_with_space(free: u64, reserve: u64) -> (crate::testing::TempDir, Barge) {
        let temp = crate::testing::TempDir::new("identity").unwrap();
        let mount = temp.folder("mount").unwrap();
        let state = temp.folder("state").unwrap();
        let os = Arc::new(crate::os::FakeOs::new());
        os.add_volume(
            mount.clone(),
            Some("11111111-2222-3333-4444-555555555555"),
            free,
        );
        let barge = Barge::open_with(state, os, Arc::new(ManualClock::new(1000))).unwrap();
        barge
            .register_volume(
                VolumeRegistration::new("disk", mount)
                    .reserve_bytes(reserve)
                    .floor_bytes(0),
            )
            .unwrap();
        (temp, barge)
    }

    #[test]
    fn adopting_many_roots_does_not_walk_root_bytes() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let directory = area.location().unwrap().path().unwrap().to_path_buf();
        fs::create_dir_all(&directory).unwrap();
        for index in 0..24 {
            let folder = format!("root-{index}");
            let path = directory.join(&folder);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("data"), b"large enough to expose a walk").unwrap();
        }
        crate::root::reset_tree_bytes_calls();
        for index in 0..24 {
            area.adopt_root(
                &format!("root-{index}"),
                "adopted",
                &format!("root-{index}"),
            )
            .unwrap();
        }
        assert_eq!(crate::root::tree_bytes_calls(), 0);
    }

    #[test]
    fn metadata_listing_matches_full_listing_except_for_bytes() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("data"), b"payload").unwrap();
        let expected_bytes = fs::symlink_metadata(root.path.join(root::MARKER_NAME))
            .unwrap()
            .len()
            + 7;
        let metadata = area.list_without_bytes().unwrap();
        let full = area.list().unwrap();
        assert_eq!(metadata.stragglers, full.stragglers);
        let mut metadata_root = metadata.get(&root.id).unwrap().clone();
        let mut full_root = full.get(&root.id).unwrap().clone();
        metadata_root.bytes = 0;
        full_root.bytes = 0;
        assert_eq!(metadata_root, full_root);
        assert_eq!(full.get(&root.id).unwrap().bytes, expected_bytes);
    }

    #[test]
    fn prune_above_reserve_without_soft_caps_does_not_list_or_walk() {
        let (_temp, barge) = fixture_with_space(10, 1);
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("data"), b"payload").unwrap();
        crate::root::reset_tree_bytes_calls();
        reset_list_calls();
        assert!(barge.prune("disk", false).unwrap().evicted.is_empty());
        assert_eq!(list_calls(), 0);
        assert_eq!(crate::root::tree_bytes_calls(), 0);
    }

    #[test]
    fn reserve_prune_walks_only_the_root_it_evicts() {
        let (_temp, barge) = fixture_with_space(0, 1);
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("data"), b"payload larger than reserve").unwrap();
        crate::root::reset_tree_bytes_calls();
        let report = barge.prune("disk", false).unwrap();
        assert_eq!(report.evicted.len(), 1);
        assert_eq!(crate::root::tree_bytes_calls(), 1);
    }

    #[test]
    fn hold_release_records_root_size_and_soft_cap_reuses_it() {
        let (_temp, barge) = fixture_with_space(10, 1);
        let area = barge
            .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(10_000)))
            .unwrap();
        let recorded = area.create_root("recorded").unwrap();
        fs::write(recorded.path.join("data"), b"known").unwrap();
        let recorded_bytes = fs::symlink_metadata(recorded.path.join(root::MARKER_NAME))
            .unwrap()
            .len()
            + 5;
        drop(area.hold(&recorded.id, Mode::Exclusive).unwrap());
        let activity = recorded
            .path
            .parent()
            .unwrap()
            .join(BARGE_DIR)
            .join(ACTIVITY_DIR);
        assert_eq!(
            hold::read_activity_bytes(&activity, &recorded.id),
            Some(recorded_bytes)
        );

        let unknown = area.create_root("unknown").unwrap();
        fs::write(unknown.path.join("data"), b"unknown").unwrap();
        let unknown_bytes = fs::symlink_metadata(unknown.path.join(root::MARKER_NAME))
            .unwrap()
            .len()
            + 7;
        crate::root::reset_tree_bytes_calls();
        barge.prune("disk", true).unwrap();
        assert_eq!(crate::root::tree_bytes_calls(), 1);
        assert_eq!(hold::read_activity_bytes(&activity, &unknown.id), None);

        crate::root::reset_tree_bytes_calls();
        barge.prune("disk", false).unwrap();
        assert_eq!(crate::root::tree_bytes_calls(), 1);
        assert_eq!(
            hold::read_activity_bytes(&activity, &unknown.id),
            Some(unknown_bytes)
        );
    }

    fn replace_on_resolve(area: &Area, root: &Root) -> Rc<RefCell<Option<crate::hold::Hold>>> {
        let held = Rc::new(RefCell::new(None));
        let saved = held.clone();
        let area = area.clone();
        let root = root.clone();
        EVICT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                area.evict(&root.id).unwrap();
                fs::create_dir(&root.path).unwrap();
                fs::write(root.path.join("contents"), b"keep").unwrap();
                area.adopt_root(&root.folder, "replacement", "replacement-id")
                    .unwrap();
                area.pin("replacement-id", "keep").unwrap();
                *saved.borrow_mut() = Some(area.hold("replacement-id", Mode::Shared).unwrap());
            }))
        });
        held
    }

    fn assert_replacement(area: &Area, root: &Root, held: &Rc<RefCell<Option<crate::hold::Hold>>>) {
        assert_eq!(fs::read(root.path.join("contents")).unwrap(), b"keep");
        let listing = area.list().unwrap();
        let replacement = listing.get("replacement-id").unwrap();
        assert!(replacement.held && replacement.pinned);
        assert!(held.borrow().is_some());
    }

    #[test]
    fn stale_eviction_keeps_replacement_with_hold_and_pin() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        let held = replace_on_resolve(&area, &root);
        assert!(area.evict(&root.id).is_err());
        assert_replacement(&area, &root, &held);
    }

    #[test]
    fn changed_missing_and_symlinked_markers_refuse_eviction() {
        for change in 0..3 {
            let (_temp, barge) = fixture();
            let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
            let root = area.create_root("slot").unwrap();
            let path = root.path.clone();
            EVICT_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    let marker = path.join(root::MARKER_NAME);
                    if change == 0 {
                        let mut value = match read_marker(&path) {
                            MarkerRead::Valid(value) => value,
                            _ => panic!("marker"),
                        };
                        value.label = "changed".to_string();
                        root::write_marker(&path, &value).unwrap();
                    } else if change == 1 {
                        fs::remove_file(marker).unwrap();
                    } else {
                        let target = path.join("marker-copy");
                        fs::rename(&marker, &target).unwrap();
                        std::os::unix::fs::symlink(target, marker).unwrap();
                    }
                }))
            });
            assert!(area.evict(&root.id).is_err());
            assert!(root.path.is_dir());
        }
    }

    #[test]
    fn prune_and_scratch_sweep_keep_replacements() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        let held = replace_on_resolve(&area, &root);
        assert!(barge.prune("disk", false).unwrap().evicted.is_empty());
        assert_replacement(&area, &root, &held);
        drop(held);

        let scratch = barge.scratch("disk", "project").unwrap();
        let root = scratch.create("run", Duration::ZERO).unwrap();
        let held = replace_on_resolve(scratch.area(), &root);
        assert!(scratch.sweep().unwrap().deleted.is_empty());
        assert_replacement(scratch.area(), &root, &held);
    }

    fn lock_file(area_path: &std::path::Path, id: &str) -> PathBuf {
        area_path
            .join(BARGE_DIR)
            .join(LOCKS_DIR)
            .join(format!("{id}.lock"))
    }

    fn directory_of(root: &Root) -> PathBuf {
        root.path.parent().unwrap().to_path_buf()
    }

    #[test]
    fn a_pin_that_resolves_to_an_evicted_root_is_refused() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        let hook_area = area.clone();
        let hook_id = root.id.clone();
        PIN_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                hook_area.evict(&hook_id).unwrap();
            }))
        });

        let error = area.pin(&root.id, "selected output").unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
        assert!(!root.path.exists(), "the eviction in between stands");
        let pin = directory_of(&root)
            .join(BARGE_DIR)
            .join(PINS_DIR)
            .join(format!("{}.json", root.id));
        assert!(!pin.exists(), "no pin names a root that is gone");
    }

    #[test]
    fn a_hold_that_wakes_on_a_replaced_lock_file_takes_the_current_one() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        let path = lock_file(&directory_of(&root), &root.id);
        // An eviction unlinks the lock while it holds it, and a caller-chosen
        // folder re-adopted with the same id makes the path name a new file.
        let replaced = path.clone();
        push_lock_hook(move || {
            fs::remove_file(&replaced).unwrap();
            fs::write(&replaced, b"").unwrap();
        });

        let hold = area.hold(&root.id, Mode::Exclusive).unwrap();

        let probe = File::open(&path).unwrap();
        assert!(
            !crate::os::flock(&probe, Mode::Exclusive, false).unwrap(),
            "the hold is visible on the file the path names"
        );
        drop(hold);
        assert!(
            crate::os::flock(&probe, Mode::Exclusive, false).unwrap(),
            "dropping the hold frees that same file"
        );
    }

    #[test]
    fn a_lock_file_replaced_twice_is_refused() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        let path = lock_file(&directory_of(&root), &root.id);
        for _ in 0..2 {
            let replaced = path.clone();
            push_lock_hook(move || {
                fs::remove_file(&replaced).unwrap();
                fs::write(&replaced, b"").unwrap();
            });
        }

        let error = area.hold(&root.id, Mode::Exclusive).unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::UnknownState);
        assert!(area.try_hold(&root.id, Mode::Exclusive).unwrap().is_some());
    }

    #[test]
    fn two_adoptions_of_one_id_cannot_both_win() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let directory = area.location().unwrap().path().unwrap().to_path_buf();
        fs::create_dir_all(&directory).unwrap();
        for folder in ["first", "second"] {
            fs::create_dir(directory.join(folder)).unwrap();
        }
        let hook_area = area.clone();
        ADOPT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                hook_area.adopt_root("first", "slot", "shared-id").unwrap();
            }))
        });

        let error = area.adopt_root("second", "slot", "shared-id").unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
        assert!(error.message().contains("already used"), "{error}");
        assert!(!directory.join("second").join(root::MARKER_NAME).exists());
        assert!(area
            .list_without_bytes()
            .unwrap()
            .get("shared-id")
            .is_some());
    }

    #[test]
    fn a_hold_that_woke_on_an_evicted_lock_file_holds_the_re_adopted_root() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let directory = area.location().unwrap().path().unwrap().to_path_buf();
        let folder = directory.join("slot-folder");
        fs::create_dir_all(&folder).unwrap();
        // A caller-chosen folder, so eviction can be followed by a re-adoption
        // with the same ID.
        let root = area.adopt_root("slot-folder", "slot", "shared-id").unwrap();
        fs::write(folder.join("contents"), b"first").unwrap();
        // The waiter opens the lock file; while it is open, the eviction renames
        // the root and unlinks that file, and the folder is re-adopted with the
        // same ID. Only then does the waiter take its lock, on the file the path
        // no longer names.
        let hook_area = area.clone();
        let hook_folder = folder.clone();
        push_lock_open_hook(move || {
            hook_area.evict("shared-id").unwrap();
            fs::create_dir(&hook_folder).unwrap();
            fs::write(hook_folder.join("contents"), b"second").unwrap();
            hook_area
                .adopt_root("slot-folder", "slot", "shared-id")
                .unwrap();
        });

        let hold = area.hold("shared-id", Mode::Exclusive).unwrap();

        assert_eq!(hold.root_id(), "shared-id");
        assert_eq!(
            area.resolve("shared-id").unwrap().path,
            folder,
            "the hold is on the re-adopted root"
        );
        assert_eq!(
            fs::read(folder.join("contents")).unwrap(),
            b"second",
            "the re-adopted root is the one that survived"
        );
        assert!(
            area.try_hold("shared-id", Mode::Exclusive)
                .unwrap()
                .is_none(),
            "another holder cannot take the root: the lock is visible"
        );
        let error = area.evict("shared-id").unwrap_err();
        assert!(
            error.message().contains("held"),
            "eviction sees the hold: {error}"
        );
        drop(hold);
        area.evict("shared-id").unwrap();
        assert!(!folder.exists(), "the released root is evictable again");
        assert_eq!(root.id, "shared-id");
    }

    #[test]
    fn a_hold_that_woke_on_an_evicted_lock_file_does_not_hold_a_missing_root() {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        // The same window without the re-adoption: the root is gone by the time
        // the waiter looks at it, so it must not report a hold.
        let hook_area = area.clone();
        let hook_id = root.id.clone();
        push_lock_open_hook(move || {
            hook_area.evict(&hook_id).unwrap();
        });

        let error = area.hold(&root.id, Mode::Exclusive).unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::NotFound);
        assert!(!root.path.exists());
    }

    #[test]
    fn an_incomplete_measurement_is_never_cached_as_exact() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, barge) = fixture_with_space(10, 1);
        let area = barge
            .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(1)))
            .unwrap();
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("readable.bin"), vec![b'x'; 16]).unwrap();
        let hidden = root.path.join("hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("unread.bin"), vec![b'x'; 64]).unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = ModeGuard(hidden);

        // Releasing a hold over an incomplete tree records no size at all.
        drop(area.hold(&root.id, Mode::Exclusive).unwrap());
        let activity = directory_of(&root).join(BARGE_DIR).join(ACTIVITY_DIR);
        assert_eq!(hold::read_activity_bytes(&activity, &root.id), None);

        let info = area
            .list()
            .unwrap()
            .get(&root.id)
            .cloned()
            .expect("the root is listed");
        assert!(info.bytes >= 16, "the readable part is counted");
        assert!(
            info.bytes_approximate,
            "the unreadable part is not called exact"
        );

        // A prune that measures it reports the same, and still caches nothing.
        let report = barge.prune("disk", true).unwrap();
        let measured = report
            .evicted
            .iter()
            .find(|evicted| evicted.id == root.id)
            .expect("the soft cap plans this root");
        assert!(
            measured.bytes_approximate,
            "a lower bound stays approximate"
        );
        assert_eq!(hold::read_activity_bytes(&activity, &root.id), None);
    }

    #[test]
    fn an_incomplete_release_forgets_an_earlier_cached_size() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, barge) = fixture_with_space(10, 1);
        let area = barge
            .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(1)))
            .unwrap();
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("small.bin"), vec![b'x'; 8]).unwrap();
        let activity = directory_of(&root).join(BARGE_DIR).join(ACTIVITY_DIR);

        // A complete release caches the size it measured.
        drop(area.hold(&root.id, Mode::Exclusive).unwrap());
        let cached =
            hold::read_activity_bytes(&activity, &root.id).expect("a complete size is cached");
        assert!(cached >= 8, "cached: {cached}");

        // More data arrives and this release cannot see all of it, so the old
        // number is forgotten instead of being read back as exact.
        let hidden = root.path.join("hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("unread.bin"), vec![b'x'; 64]).unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = ModeGuard(hidden);
        drop(area.hold(&root.id, Mode::Exclusive).unwrap());

        assert_eq!(hold::read_activity_bytes(&activity, &root.id), None);
        let info = area
            .list()
            .unwrap()
            .get(&root.id)
            .cloned()
            .expect("the root is listed");
        assert!(info.bytes >= 8, "the readable part is measured");
        assert!(info.bytes_approximate, "the stale size is not reused");

        let report = barge.prune("disk", true).unwrap();
        let planned = report
            .evicted
            .iter()
            .find(|evicted| evicted.id == root.id)
            .expect("the soft cap plans this root");
        assert!(
            planned.bytes_approximate,
            "prune measures instead of reusing an old number"
        );
    }

    /// Restores a folder's mode so the fixture can still be removed.
    struct ModeGuard(PathBuf);

    impl Drop for ModeGuard {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn an_unreadable_pin_is_never_read_as_unpinned() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        area.pin(&root.id, "selected output").unwrap();
        let pins = directory_of(&root).join(BARGE_DIR).join(PINS_DIR);
        fs::set_permissions(&pins, fs::Permissions::from_mode(0o000)).unwrap();
        let _guard = ModeGuard(pins);

        let listing = area.list_without_bytes().unwrap();
        assert!(
            listing.get(&root.id).unwrap().pinned,
            "a pin that cannot be read still protects the root"
        );
        let error = area.evict(&root.id).unwrap_err();

        assert_eq!(error.kind(), crate::error::ErrorKind::Io);
        assert!(root.path.is_dir(), "the root was not evicted");
    }
}
