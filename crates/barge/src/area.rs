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

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hold::{self, Hold, Mode};
use crate::root::{
    self, folder_name, mint_id, read_marker, tree_bytes, Listing, MarkerRead, Root, RootInfo,
    RootMarker, Straggler, ROOT_SCHEMA,
};
use crate::space::{SkippedRoot, SweepReport, SweptRoot};
use crate::state::{publish, validate_name, validate_relative_path};
use crate::volume::{Volume, VolumeState};
use crate::Core;

/// The schema name of a stored area declaration.
pub const AREA_SCHEMA: &str = "barge.area/1";
/// The schema name of a pin record.
pub const PIN_SCHEMA: &str = "barge.pin/1";

const BARGE_DIR: &str = ".barge";
const TRASH_DIR: &str = ".trash";
const LOCKS_DIR: &str = "locks";
const PINS_DIR: &str = "pins";
const ACTIVITY_DIR: &str = "activity";
const AREA_LOCK: &str = "area.lock";

/// What an area does when its volume is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Absent {
    /// Report the volume to attach; caches and scratch use this.
    Fail,
    /// Use an explicit internal path and say so in the result and in status.
    Fallback { path: PathBuf },
}

/// The `<namespace>/<area>` name of an area.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AreaKey {
    namespace: String,
    name: String,
}

impl AreaKey {
    /// Validate and build a key.
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Result<Self> {
        let key = Self {
            namespace: namespace.into(),
            name: name.into(),
        };
        validate_name("area namespace", &key.namespace)?;
        validate_name("area name", &key.name)?;
        Ok(key)
    }

    /// Parse `<namespace>/<area>`.
    pub fn parse(text: &str) -> Result<Self> {
        let (namespace, name) = text.split_once('/').ok_or_else(|| {
            Error::usage(format!("area {text:?} must be written <namespace>/<area>"))
        })?;
        Self::new(namespace, name)
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `<namespace>/<area>`, the name a consumer and a marker use.
    pub fn label(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// The state file's stem, `<namespace>.<area>`.
    pub(crate) fn file_stem(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
}

/// A consumer's area declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaSpec {
    pub namespace: String,
    pub name: String,
    pub volume: String,
    /// Folder inside the volume base; `None` means `<namespace>/<area>`.
    pub folder: Option<String>,
    pub absent: Absent,
    pub soft_cap_bytes: Option<u64>,
    pub counts_toward_reserve: bool,
}

impl AreaSpec {
    /// A declaration whose path defaults to `<root_dir>/<namespace>/<area>`, that
    /// fails when its volume is absent and counts toward the reserve.
    pub fn new(
        namespace: impl Into<String>,
        name: impl Into<String>,
        volume: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            volume: volume.into(),
            folder: None,
            absent: Absent::Fail,
            soft_cap_bytes: None,
            counts_toward_reserve: true,
        }
    }

    pub fn folder(mut self, folder: impl Into<String>) -> Self {
        self.folder = Some(folder.into());
        self
    }

    pub fn absent(mut self, absent: Absent) -> Self {
        self.absent = absent;
        self
    }

    pub fn soft_cap_bytes(mut self, soft_cap_bytes: Option<u64>) -> Self {
        self.soft_cap_bytes = soft_cap_bytes;
        self
    }

    pub fn counts_toward_reserve(mut self, counts: bool) -> Self {
        self.counts_toward_reserve = counts;
        self
    }

    pub(crate) fn into_record(self) -> AreaRecord {
        AreaRecord {
            schema: AREA_SCHEMA.to_string(),
            namespace: self.namespace,
            name: self.name,
            volume: self.volume,
            folder: self.folder,
            absent: self.absent,
            soft_cap_bytes: self.soft_cap_bytes,
            counts_toward_reserve: self.counts_toward_reserve,
        }
    }
}

/// A stored area declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaRecord {
    pub schema: String,
    pub namespace: String,
    pub name: String,
    pub volume: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub absent: Absent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_cap_bytes: Option<u64>,
    pub counts_toward_reserve: bool,
}

impl AreaRecord {
    /// The area's key.
    pub fn key(&self) -> AreaKey {
        AreaKey {
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        }
    }

    /// The folder this area uses inside the volume base.
    pub fn effective_folder(&self) -> String {
        self.folder
            .clone()
            .unwrap_or_else(|| format!("{}/{}", self.namespace, self.name))
    }

    /// Validate every stored field before the record is used as truth.
    pub fn validate(&self) -> Result<()> {
        if self.schema != AREA_SCHEMA {
            return Err(Error::unknown_state(format!(
                "area {}/{} has schema {}, not {AREA_SCHEMA}",
                self.namespace, self.name, self.schema
            )));
        }
        validate_name("area namespace", &self.namespace)?;
        validate_name("area name", &self.name)?;
        crate::state::validate_volume_name("area volume", &self.volume)?;
        if let Some(folder) = &self.folder {
            validate_relative_path("area folder", folder)?;
        }
        if let Absent::Fallback { path } = &self.absent {
            if !path.is_absolute() {
                return Err(Error::unknown_state(format!(
                    "area {}/{} fallback {} is not absolute",
                    self.namespace,
                    self.name,
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// The fields that differ between this stored declaration and a proposed one,
    /// in a stable order. Empty means the declarations agree.
    pub fn differing_fields(&self, proposed: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.volume != proposed.volume {
            fields.push("volume");
        }
        if self.folder != proposed.folder {
            fields.push("folder");
        }
        if self.absent != proposed.absent {
            fields.push("absent");
        }
        if self.soft_cap_bytes != proposed.soft_cap_bytes {
            fields.push("soft_cap_bytes");
        }
        if self.counts_toward_reserve != proposed.counts_toward_reserve {
            fields.push("counts_toward_reserve");
        }
        fields
    }
}

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
    /// False when the trash copy could not be deleted and is left for a sweep.
    pub deleted: bool,
}

/// One declared area with the volume it belongs to.
#[derive(Debug, Clone)]
pub struct Area {
    core: Core,
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

    /// Create a root that expires at `expires_unix_ms` (scratch uses this).
    pub(crate) fn create_root_with_expiry(
        &self,
        label: &str,
        expires_unix_ms: Option<u64>,
    ) -> Result<Root> {
        root::validate_label(label)?;
        let directory = self.available_dir()?;
        self.ensure_layout(&directory)?;
        let now = self.core.clock.now_unix_ms();
        let mut attempts = 0;
        loop {
            let id = mint_id()?;
            let path = directory.join(folder_name(label, &id));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let marker = RootMarker {
                        schema: ROOT_SCHEMA.to_string(),
                        area: self.key().label(),
                        id: id.clone(),
                        label: label.to_string(),
                        created_unix_ms: now,
                        expires_unix_ms,
                    };
                    if let Err(error) = root::write_marker(&path, &marker) {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    self.record_activity(&directory, &id, now);
                    return Ok(Root {
                        id,
                        label: label.to_string(),
                        path,
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

    /// Everything the area holds: owned roots, and folders Barge will not delete.
    pub fn list(&self) -> Result<Listing> {
        let mut listing = Listing::default();
        let key = self.key().label();
        for (directory, on_volume) in self.search_dirs()? {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(Error::io("cannot list the area at", &directory, &source))
                }
            };
            let barge = directory.join(BARGE_DIR);
            let locks = barge.join(LOCKS_DIR);
            let pins = barge.join(PINS_DIR);
            let activity = barge.join(ACTIVITY_DIR);
            for entry in entries {
                let entry = entry
                    .map_err(|source| Error::io("cannot list the area at", &directory, &source))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => return Err(Error::io("cannot inspect", &path, &source)),
                };
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    continue;
                }
                match read_marker(&path) {
                    MarkerRead::Missing => listing.stragglers.push(Straggler {
                        path,
                        reason: format!("no {}", root::MARKER_NAME),
                    }),
                    MarkerRead::Invalid(reason) => {
                        listing.stragglers.push(Straggler { path, reason })
                    }
                    MarkerRead::Valid(marker) if marker.area != key => {
                        listing.stragglers.push(Straggler {
                            path,
                            reason: format!("its marker belongs to area {}", marker.area),
                        })
                    }
                    MarkerRead::Valid(marker) => {
                        if name != folder_name(&marker.label, &marker.id) {
                            listing.stragglers.push(Straggler {
                                path,
                                reason: "its folder name does not match its marker".to_string(),
                            });
                            continue;
                        }
                        let held = hold::is_locked(&locks.join(format!("{}.lock", marker.id)))?;
                        let pinned = pins.join(format!("{}.json", marker.id)).is_file();
                        let last_used = hold::read_activity(&activity, &marker.id)
                            .unwrap_or(marker.created_unix_ms)
                            .max(marker.created_unix_ms);
                        let bytes = tree_bytes(&path);
                        listing.roots.push(RootInfo {
                            id: marker.id.clone(),
                            label: marker.label.clone(),
                            path,
                            created_unix_ms: marker.created_unix_ms,
                            last_used_unix_ms: last_used,
                            expires_unix_ms: marker.expires_unix_ms,
                            held,
                            pinned,
                            bytes,
                            on_volume,
                        });
                    }
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

    /// Find a root by its ID, on the volume or in the fallback.
    pub fn resolve(&self, id: &str) -> Result<Root> {
        root::validate_id(id)?;
        let key = self.key().label();
        let suffix = format!("-{id}");
        for (directory, _) in self.search_dirs()? {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(Error::io("cannot list the area at", &directory, &source))
                }
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.ends_with(&suffix) {
                    continue;
                }
                let path = entry.path();
                if let MarkerRead::Valid(marker) = read_marker(&path) {
                    if marker.area == key && marker.id == id {
                        return Ok(Root {
                            id: marker.id,
                            label: marker.label,
                            path,
                            created_unix_ms: marker.created_unix_ms,
                            expires_unix_ms: marker.expires_unix_ms,
                        });
                    }
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
        self.acquire(id, mode, true)?.ok_or_else(|| {
            Error::unknown_state(format!("a waiting hold on root {id} was not acquired"))
        })
    }

    /// Try to hold a root without waiting. `None` means something else holds it.
    pub fn try_hold(&self, id: &str, mode: Mode) -> Result<Option<Hold>> {
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| Error::io("cannot open the hold lock", &lock_path, &source))?;
        if !hold::flock(&file, mode, wait)? {
            return Ok(None);
        }
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        self.record_activity(&directory, id, self.core.clock.now_unix_ms());
        Ok(Some(Hold::new(
            id.to_string(),
            mode,
            file,
            activity,
            self.core.clock.clone(),
        )))
    }

    /// Protect a root from prune and eviction until it is unpinned.
    pub fn pin(&self, id: &str, reason: &str) -> Result<()> {
        let root = self.resolve(id)?;
        let directory = parent(&root.path)?;
        self.ensure_layout(&directory)?;
        let _lock = AreaLock::acquire(&directory)?;
        let record = PinRecord {
            schema: PIN_SCHEMA.to_string(),
            root_id: id.to_string(),
            reason: reason.to_string(),
            pinned_unix_ms: self.core.clock.now_unix_ms(),
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|source| Error::unknown_state(format!("cannot encode a pin: {source}")))?;
        publish(&pin_path(&directory, id), &bytes)
    }

    /// Remove a root's pin.
    pub fn unpin(&self, id: &str) -> Result<()> {
        root::validate_id(id)?;
        let mut removed = false;
        for (directory, _) in self.search_dirs()? {
            if !directory.is_dir() {
                continue;
            }
            self.ensure_layout(&directory)?;
            let _lock = AreaLock::acquire(&directory)?;
            match fs::remove_file(pin_path(&directory, id)) {
                Ok(()) => removed = true,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::io(
                        "cannot remove the pin at",
                        &pin_path(&directory, id),
                        &source,
                    ))
                }
            }
        }
        if removed {
            Ok(())
        } else {
            Err(Error::not_found(format!(
                "root {id} in area {} is not pinned",
                self.key().label()
            )))
        }
    }

    /// Evict a root: refuse when it is held or pinned, otherwise rename it into
    /// the area's trash and delete it there.
    pub fn evict(&self, id: &str) -> Result<Eviction> {
        let root = self.resolve(id)?;
        let directory = parent(&root.path)?;
        self.ensure_layout(&directory)?;
        let lock_path = directory
            .join(BARGE_DIR)
            .join(LOCKS_DIR)
            .join(format!("{id}.lock"));
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| Error::io("cannot open the hold lock", &lock_path, &source))?;
        if !hold::flock(&lock_file, Mode::Exclusive, false)? {
            return Err(Error::refused(format!(
                "root {id} is held; refusing to evict it"
            )));
        }
        let _area_lock = AreaLock::acquire(&directory)?;
        if pin_path(&directory, id).is_file() {
            return Err(Error::refused(format!(
                "root {id} is pinned; refusing to evict it"
            )));
        }
        let bytes = tree_bytes(&root.path);
        let trash = directory.join(TRASH_DIR);
        fs::create_dir_all(&trash)
            .map_err(|source| Error::io("cannot create the trash at", &trash, &source))?;
        let trash_path = unique_trash_path(&trash, &root.folder_name());
        fs::rename(&root.path, &trash_path)
            .map_err(|source| Error::io("cannot evict the root to", &trash_path, &source))?;
        let deleted = match crate::trash::remove_owned_tree(&trash_path, &self.key().label()) {
            Ok(deletion) => deletion.skipped.is_empty(),
            Err(_) => false,
        };
        let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
        let _ = fs::remove_file(hold::activity_path(&activity, id));
        let pin = pin_path(&directory, id);
        let _ = fs::remove_file(&pin);
        Ok(Eviction {
            area: self.key().label(),
            id: id.to_string(),
            label: root.label,
            path: root.path,
            trash_path,
            bytes,
            deleted,
        })
    }

    /// Delete every owned trash entry of this area.
    pub fn sweep_trash(&self) -> Result<SweepReport> {
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
                if path.is_dir() {
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

    /// Evict expired, idle, unpinned roots. Scratch uses this.
    pub fn sweep_expired(&self) -> Result<SweepReport> {
        let now = self.core.clock.now_unix_ms();
        let mut report = SweepReport::default();
        for info in self.list()?.roots {
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
    fn search_dirs(&self) -> Result<Vec<(PathBuf, bool)>> {
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

    fn ensure_layout(&self, directory: &Path) -> Result<()> {
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
            let _ = hold::record_activity(&activity, id, unix_ms);
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinRecord {
    schema: String,
    root_id: String,
    reason: String,
    pinned_unix_ms: u64,
}

/// The area lock: pinning, unpinning, eviction and the trash sweep take it, so a
/// pin cannot land between an eviction's pin recheck and its rename.
#[derive(Debug)]
struct AreaLock {
    #[allow(dead_code)]
    file: File,
}

impl AreaLock {
    fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(BARGE_DIR).join(AREA_LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| Error::io("cannot open the area lock", &path, &source))?;
        hold::flock(&file, Mode::Exclusive, true)?;
        Ok(Self { file })
    }
}

fn parent(path: &Path) -> Result<PathBuf> {
    path.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::unknown_state(format!("{} has no parent folder", path.display())))
}

fn pin_path(directory: &Path, id: &str) -> PathBuf {
    directory
        .join(BARGE_DIR)
        .join(PINS_DIR)
        .join(format!("{id}.json"))
}

fn unique_trash_path(trash: &Path, name: &str) -> PathBuf {
    let mut candidate = trash.join(name);
    let mut counter = 0;
    while candidate.exists() {
        counter += 1;
        candidate = trash.join(format!("{name}.{counter}"));
    }
    candidate
}
