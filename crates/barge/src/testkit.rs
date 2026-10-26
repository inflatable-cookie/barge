//! Test-only fixture helpers: one fresh temporary directory per test.
//!
//! Every fixture lives inside a directory this process creates and removes, and
//! never at a fixed path, so two tests — or two tasks on one machine — cannot
//! share a name. Tests that need the state directory use
//! [`crate::state::StatePaths`] over a directory here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The scratch directory AGENTS.md names for scratch writes when the Builds
/// drive is present. Nothing is ever written to a fixed name inside it: every
/// fixture is a fresh directory this process creates and removes.
pub const SCRATCH_BASE: &str = "/Volumes/Builds/scratch/barge";

/// The base a fixture that needs the real OS layer is created under.
///
/// AGENTS.md asks for scratch inside the Builds drive when it is present. When
/// it is not, the platform's temporary directory is used, which is still a fresh
/// directory per test rather than a fixed name.
pub fn scratch_base() -> PathBuf {
    let scratch = PathBuf::from(SCRATCH_BASE);
    if scratch.is_dir() {
        scratch
    } else {
        std::env::temp_dir()
    }
}

/// A temporary directory that removes itself when the test ends.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory named after `label` and this process.
    pub fn new(label: &str) -> Self {
        Self::under(&std::env::temp_dir(), label)
    }

    /// Create a fresh directory named after `label` and this process under
    /// `base`, creating `base` when it is not there.
    pub fn under(base: &Path, label: &str) -> Self {
        if !base.is_dir() {
            std::fs::create_dir_all(base).expect("the fixture base is created");
        }
        for _ in 0..32 {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let path = base.join(format!(
                "barge-test-{label}-{}-{sequence}-{nanos}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create the fixture directory: {error}"),
            }
        }
        panic!(
            "could not find a fresh fixture directory name under {}",
            base.display()
        );
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory. Nothing is created.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Write one fixture file, creating its parent directories.
    pub fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the fixture parent is created");
        }
        std::fs::write(&path, bytes).expect("the fixture file is written");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---- the S5 fixture: one owned root on one injected volume ------------------

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use crate::area::{self, AreaDeclaration, OwnedRoot, ResolvedArea, RootSpec};
use crate::contract::HOLD_SCHEMA;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    AccountFacts, ChildListing, ChildRemoval, DescriptorScan, DescriptorScanRequest, DeviceId,
    DirHandle, DirId, DirOps, EntryFacts, FileSystem, MacOs, MountPoint, MountRoot, OpenDescriptor,
    ProcessObserver, RemoveKind, VolumeFacts, VolumeIdentity, VolumeUuid,
};
use crate::registry::{
    Availability, Durability, FolderOverrides, RecordOrigin, RegistryDocument, RegistryWriter,
    ScratchGrants, VolumeRecord,
};
use crate::state::{Deadline, LockSet, StatePaths};

/// The volume UUID every S5 fixture uses unless it asks for another.
pub const FIXTURE_UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";
/// A second volume UUID, for the mismatched-volume cases.
pub const OTHER_UUID: &str = "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D";
/// The boot identity every fixture hold records.
pub const FIXTURE_BOOT: &str = "1700000000.0";

/// Injected account facts: the account home is a fixture directory.
pub struct FakeAccount {
    /// The fixture's home.
    pub home: PathBuf,
}

impl AccountFacts for FakeAccount {
    fn account_home(&self) -> Result<PathBuf> {
        Ok(self.home.clone())
    }

    fn state_root(&self) -> Result<PathBuf> {
        Ok(self.home.join("Library/Application Support/barge/state"))
    }

    fn tool_state_dir(&self, tool: &str) -> Result<PathBuf> {
        Ok(self
            .home
            .join("Library/Application Support")
            .join(tool)
            .join("state"))
    }

    fn tool_cache_dir(&self, tool: &str) -> Result<PathBuf> {
        Ok(self.home.join("Library/Caches").join(tool))
    }
}

/// Injected volume facts: a fixture directory stands in for a mounted volume, and
/// a test decides whether it is present, which UUID it has, and whether its
/// device changed under a running deletion.
pub struct FakeVolume {
    /// The fixture mount root.
    pub root: PathBuf,
    /// The UUID the volume reports.
    pub uuid: VolumeUuid,
    /// The device the volume reports.
    pub device: DeviceId,
    /// Whether the volume is attached.
    pub present: Cell<bool>,
    /// When set, the next identification reports another device, which is how a
    /// device change is injected at an exact step.
    pub shifted: Cell<bool>,
}

impl FakeVolume {
    /// The same volume, reporting a different device from now on.
    pub fn shift_device(&self) {
        self.shifted.set(true);
    }
}

impl VolumeFacts for FakeVolume {
    fn identify(&self, _path: &Path) -> Result<VolumeIdentity> {
        if !self.present.get() {
            return Err(Error::new(
                ErrorKind::VolumeAbsent,
                "the fixture volume is not attached",
            ));
        }
        let device = if self.shifted.get() {
            DeviceId::new(self.device.get().saturating_add(1))
        } else {
            self.device
        };
        Ok(VolumeIdentity {
            mount_point: self.root.clone(),
            filesystem: FileSystem::Apfs,
            device,
            volume_uuid: Some(self.uuid.clone()),
            boot_volume: false,
        })
    }
}

/// Injected mount-root proof over the fixture volume.
pub struct FakeMounts {
    /// The fixture mount root.
    pub root: PathBuf,
}

impl MountRoot for FakeMounts {
    fn mount_root(&self, path: &Path) -> Result<MountPoint> {
        if path != self.root {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!("{} is not the fixture mount root", path.display()),
            ));
        }
        Ok(MountPoint::new(self.root.clone()))
    }
}

/// How the injected descriptor scan answers.
#[derive(Clone, Default)]
pub enum ScanMode {
    /// Complete, with these open descriptors.
    #[default]
    Complete,
    /// Complete, finding this descriptor open under the root.
    Open(Vec<OpenDescriptor>),
    /// Bounded before it finished.
    Bounded,
    /// Unreadable.
    Unreadable,
}

/// Injected process facts: uid, boot, process starts, and the descriptor scan.
pub struct FakeObserver {
    /// The effective uid, so a foreign-owned record can be simulated.
    pub uid: u32,
    /// The boot identity.
    pub boot: String,
    /// The processes that are alive, with their start identities.
    pub processes: BTreeMap<u32, Option<u64>>,
    /// Pids whose start identity cannot be read.
    pub unreadable: BTreeSet<u32>,
    /// How the descriptor scan answers.
    pub scan: ScanMode,
}

impl FakeObserver {
    /// The fixture observer for this account.
    pub fn new() -> Self {
        Self {
            uid: crate::os::platform()
                .effective_uid()
                .expect("the effective uid"),
            boot: FIXTURE_BOOT.to_owned(),
            processes: BTreeMap::from([(std::process::id(), Some(7_000_000))]),
            unreadable: BTreeSet::new(),
            scan: ScanMode::Complete,
        }
    }
}

impl Default for FakeObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessObserver for FakeObserver {
    fn effective_uid(&self) -> Result<u32> {
        Ok(self.uid)
    }

    fn boot_identity(&self) -> Result<String> {
        Ok(self.boot.clone())
    }

    fn process_start(&self, pid: u32) -> Result<Option<u64>> {
        if self.unreadable.contains(&pid) {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!("the start identity of pid {pid} is unreadable"),
            ));
        }
        Ok(self.processes.get(&pid).copied().flatten())
    }

    fn scan_open_descriptors(
        &self,
        _request: &DescriptorScanRequest<'_>,
    ) -> Result<DescriptorScan> {
        match &self.scan {
            ScanMode::Complete => Ok(DescriptorScan::Complete {
                processes: 3,
                descriptors: 12,
                foreign: 0,
                retried_unreadable: 0,
                retried: Vec::new(),
                open: Vec::new(),
            }),
            ScanMode::Open(open) => Ok(DescriptorScan::Complete {
                processes: 3,
                descriptors: 12,
                foreign: 0,
                retried_unreadable: 0,
                retried: Vec::new(),
                open: open.clone(),
            }),
            ScanMode::Bounded => Ok(DescriptorScan::Bounded {
                processes: 4096,
                descriptors: 65_536,
                bound: "descriptors",
            }),
            ScanMode::Unreadable => Err(Error::new(
                ErrorKind::UnknownState,
                "the open-descriptor scan is unreadable",
            )),
        }
    }
}

/// The platform's directory operations with the faults a platform cannot be
/// asked to produce: a foreign owner, an entry on another device, an
/// `ENOTEMPTY` that keeps coming back, and a device change after the quarantine
/// rename.
#[derive(Default)]
pub struct Faults {
    inner: MacOs,
    /// A name whose facts report another owner.
    pub foreign_uid_name: Option<String>,
    /// A name whose facts report another device.
    pub shifted_device_name: Option<String>,
    /// The device that name reports.
    pub shifted_device: Option<DeviceId>,
    /// Names whose directory removal reports `ENOTEMPTY` this many more times.
    pub not_empty: RefCell<HashMap<String, usize>>,
    /// Name prefixes whose directory removal reports `ENOTEMPTY` this many more
    /// times, so a caller can fail a name it does not know in advance.
    pub not_empty_prefix: RefCell<Vec<(String, usize)>>,
    /// A volume to shift when its rename succeeds.
    pub volume: Option<std::rc::Rc<FakeVolume>>,
    /// A directory planted inside the quarantine leaf after a number of file
    /// removals, so a test can prove what happens to Finder-shaped metadata that
    /// appears after the classification.
    pub plant: RefCell<Option<Plant>>,
}

/// A directory to create inside the quarantine leaf mid-delete.
pub struct Plant {
    /// The area folder that holds the quarantine leaf.
    pub area: PathBuf,
    /// The name to create, which is why a Finder-shaped name is interesting.
    pub name: String,
    /// After how many successful file removals it is created.
    pub after_removals: usize,
    removed: usize,
    done: bool,
}

impl Faults {
    /// A wrapper over the platform implementation.
    pub fn new() -> Self {
        Self {
            inner: crate::os::platform(),
            ..Self::default()
        }
    }

    /// Report a foreign owner for one name.
    pub fn with_foreign_owner(mut self, name: &str) -> Self {
        self.foreign_uid_name = Some(name.to_owned());
        self
    }

    /// Report another device for one name.
    pub fn with_device_shift(mut self, name: &str, device: DeviceId) -> Self {
        self.shifted_device_name = Some(name.to_owned());
        self.shifted_device = Some(device);
        self
    }

    /// Refuse one directory removal with `ENOTEMPTY` `times` times.
    pub fn failing_remove(&self, name: &str, times: usize) {
        self.not_empty.borrow_mut().insert(name.to_owned(), times);
    }

    /// Refuse directory removals whose name starts with `prefix` `times` times.
    pub fn failing_remove_prefix(&self, prefix: &str, times: usize) {
        self.not_empty_prefix
            .borrow_mut()
            .push((prefix.to_owned(), times));
    }

    /// Shift the injected volume's device when a rename succeeds.
    pub fn shifting_volume(mut self, volume: std::rc::Rc<FakeVolume>) -> Self {
        self.volume = Some(volume);
        self
    }

    /// Create a directory named `name` inside the quarantine leaf after
    /// `after_removals` successful file removals.
    pub fn planting_directory(&self, area: &Path, name: &str, after_removals: usize) {
        *self.plant.borrow_mut() = Some(Plant {
            area: area.to_path_buf(),
            name: name.to_owned(),
            after_removals,
            removed: 0,
            done: false,
        });
    }

    /// Create the planted entry once enough removals have happened.
    fn maybe_plant(&self) {
        let mut guard = self.plant.borrow_mut();
        let Some(plant) = guard.as_mut() else {
            return;
        };
        if plant.done {
            return;
        }
        plant.removed = plant.removed.saturating_add(1);
        if plant.removed < plant.after_removals {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&plant.area) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(crate::state::QUARANTINE_PREFIX)
            {
                if std::fs::create_dir(entry.path().join(&plant.name)).is_ok() {
                    plant.done = true;
                }
                break;
            }
        }
    }
}

impl DirOps for Faults {
    fn open_path(&self, path: &Path) -> Result<DirHandle> {
        self.inner.open_path(path)
    }

    fn path_facts(&self, path: &Path) -> Result<Option<EntryFacts>> {
        self.inner.path_facts(path)
    }

    fn open_child_dir(&self, parent: &DirHandle, name: &str) -> Result<DirHandle> {
        self.inner.open_child_dir(parent, name)
    }

    fn duplicate_dir(&self, handle: &DirHandle) -> Result<DirHandle> {
        self.inner.duplicate_dir(handle)
    }

    fn child_facts(&self, parent: &DirHandle, name: &str) -> Result<Option<EntryFacts>> {
        let Some(mut facts) = self.inner.child_facts(parent, name)? else {
            return Ok(None);
        };
        if self.foreign_uid_name.as_deref() == Some(name) {
            facts.uid = facts.uid.saturating_add(1);
        }
        if self.shifted_device_name.as_deref() == Some(name) {
            if let Some(device) = self.shifted_device {
                facts.device = device;
            }
        }
        Ok(Some(facts))
    }

    fn create_child_dir(&self, parent: &DirHandle, name: &str, mode: u32) -> Result<DirId> {
        self.inner.create_child_dir(parent, name, mode)
    }

    fn publish_child_file(
        &self,
        parent: &DirHandle,
        name: &str,
        bytes: &[u8],
        mode: u32,
    ) -> Result<()> {
        self.inner.publish_child_file(parent, name, bytes, mode)
    }

    fn read_child_file(
        &self,
        parent: &DirHandle,
        name: &str,
        cap: usize,
    ) -> Result<Option<Vec<u8>>> {
        self.inner.read_child_file(parent, name, cap)
    }

    fn list_children(&self, parent: &DirHandle, max_names: usize) -> Result<ChildListing> {
        self.inner.list_children(parent, max_names)
    }

    fn remove_child(
        &self,
        parent: &DirHandle,
        name: &str,
        kind: RemoveKind,
    ) -> Result<ChildRemoval> {
        if kind == RemoveKind::Directory {
            let mut prefixes = self.not_empty_prefix.borrow_mut();
            for (prefix, remaining) in prefixes.iter_mut() {
                if *remaining > 0 && name.starts_with(prefix.as_str()) {
                    *remaining -= 1;
                    return Ok(ChildRemoval::NotEmpty);
                }
            }
        }
        let mut remaining = self.not_empty.borrow_mut();
        if let Some(count) = remaining.get_mut(name) {
            if *count > 0 {
                *count -= 1;
                return Ok(ChildRemoval::NotEmpty);
            }
        }
        drop(remaining);
        let removal = self.inner.remove_child(parent, name, kind);
        if matches!(removal, Ok(ChildRemoval::Removed)) {
            self.maybe_plant();
        }
        removal
    }

    fn rename_child(&self, parent: &DirHandle, from: &str, to: &str) -> Result<()> {
        self.inner.rename_child(parent, from, to)?;
        if let Some(volume) = &self.volume {
            volume.shift_device();
        }
        Ok(())
    }

    fn random_bytes(&self, length: usize) -> Result<Vec<u8>> {
        self.inner.random_bytes(length)
    }
}

/// One fixture volume, its registered record, its account state and one area to
/// create owned roots in.
pub struct VolumeFixture {
    /// The temporary directory everything lives in.
    pub dir: TempDir,
    /// The fixture mount root.
    pub root: PathBuf,
    /// The volume's UUID.
    pub uuid: VolumeUuid,
    /// The volume's device, as the platform reports it.
    pub device: DeviceId,
    /// The volume's registered record.
    pub record: VolumeRecord,
    /// The fixture account state directory.
    pub state: StatePaths,
}

impl VolumeFixture {
    /// A fresh fixture volume with an expendable record and no state yet.
    pub fn new(label: &str) -> Self {
        Self::within(TempDir::new(label))
    }

    /// A fresh fixture volume under the scratch base (AGENTS.md), for a test
    /// that exercises the real OS layer instead of injected facts.
    pub fn under_scratch(label: &str) -> Self {
        Self::within(TempDir::under(&scratch_base(), label))
    }

    fn within(dir: TempDir) -> Self {
        let root = dir.join("volume");
        std::fs::create_dir(&root).expect("the fixture volume root is created");
        let platform = crate::os::platform();
        let device = platform
            .open_path(&root)
            .expect("the fixture root opens")
            .identity()
            .device;
        let uuid = VolumeUuid::parse(FIXTURE_UUID).expect("a test UUID");
        let record = VolumeRecord {
            volume_uuid: uuid.clone(),
            mount_hint: Some(root.clone()),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            root_dir: String::new(),
            folders: FolderOverrides::none(),
            origin: RecordOrigin::Declared,
            scratch_grants: ScratchGrants::none(),
            generation: 1,
            registered_ms: 1_792_525_019_847,
        };
        let state = StatePaths::resolve(
            &FakeAccount {
                home: dir.path().to_path_buf(),
            },
            Some(&dir.join("state")),
        )
        .expect("the fixture state directory resolves");
        Self {
            dir,
            root,
            uuid,
            device,
            record,
            state,
        }
    }

    /// The platform implementation.
    pub fn platform(&self) -> MacOs {
        crate::os::platform()
    }

    /// The volume facts, sharing a device-shift cell with a [`Faults`] wrapper.
    pub fn facts_shared(&self) -> std::rc::Rc<FakeVolume> {
        std::rc::Rc::new(FakeVolume {
            root: self.root.clone(),
            uuid: self.uuid.clone(),
            device: self.device,
            present: Cell::new(true),
            shifted: Cell::new(false),
        })
    }

    /// The volume facts.
    pub fn facts(&self) -> FakeVolume {
        FakeVolume {
            root: self.root.clone(),
            uuid: self.uuid.clone(),
            device: self.device,
            present: Cell::new(true),
            shifted: Cell::new(false),
        }
    }

    /// The mount-root proof.
    pub fn mounts(&self) -> FakeMounts {
        FakeMounts {
            root: self.root.clone(),
        }
    }

    /// The process facts.
    pub fn observer(&self) -> FakeObserver {
        FakeObserver::new()
    }

    /// Publish the volume's registry record.
    pub fn register(&self) {
        self.register_record(&self.record);
    }

    /// Publish one registry record.
    pub fn register_record(&self, record: &VolumeRecord) {
        self.state.initialize().expect("the layout exists");
        let mut locks =
            LockSet::with_deadline(self.state.clone(), Deadline::after(Duration::from_secs(5)));
        let lock = locks.lock_registry().expect("the registry lock");
        RegistryWriter::new(&self.state)
            .publish(&lock, &RegistryDocument::new(vec![record.clone()]))
            .expect("the registry is written");
    }

    /// A declaration whose folder is `folder`.
    pub fn declaration(&self, folder: &str) -> AreaDeclaration {
        AreaDeclaration {
            namespace: "example".to_owned(),
            area: "pool".to_owned(),
            folder: folder.to_owned(),
            overrides: BTreeMap::new(),
            absent: area::AbsentPolicy::Fail {
                attach: "the build volume".to_owned(),
            },
            prune: area::PrunePolicy::OldestIdleAfter {
                age_seconds: 86_400,
            },
            reserve: area::ReserveParticipation::Counts,
        }
    }

    /// Resolve one declaration on the fixture volume.
    pub fn resolved(&self, record: &VolumeRecord, declaration: &AreaDeclaration) -> ResolvedArea {
        let platform = self.platform();
        let facts = self.facts();
        let mounts = self.mounts();
        let observer = self.observer();
        area::resolve(
            &platform,
            &facts,
            &mounts,
            &observer,
            record,
            declaration,
            None,
        )
        .expect("the area resolves")
    }

    /// Create one owned root in the declared folder.
    pub fn create_root(
        &self,
        record: &VolumeRecord,
        declaration: &AreaDeclaration,
        kind: area::RootKind,
    ) -> (OwnedRoot, ResolvedArea) {
        let platform = self.platform();
        let observer = self.observer();
        let resolved = self.resolved(record, declaration);
        let root = area::create_root(&platform, &observer, &resolved, RootSpec { kind })
            .expect("the root is created");
        let resolved = self.resolved(record, declaration);
        (root, resolved)
    }

    /// The resolved area and its folder string for a fresh declaration.
    pub fn area(&self, folder: &str) -> (AreaDeclaration, ResolvedArea) {
        let declaration = self.declaration(folder);
        let resolved = self.resolved(&self.record, &declaration);
        (declaration, resolved)
    }

    /// Write one payload file inside a path.
    pub fn payload(path: &Path, name: &str, bytes: &[u8]) {
        let target = path.join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("the payload parent is created");
        }
        std::fs::write(target, bytes).expect("the payload is written");
    }

    /// A hold record's JSON bytes, as the given facts would write them.
    pub fn hold_record(
        root_id: &str,
        holder: &str,
        pid: u32,
        process_start: u64,
        boot: &str,
    ) -> Vec<u8> {
        let value = serde_json::json!({
            "schema": HOLD_SCHEMA,
            "root_id": root_id,
            "holder": holder,
            "pid": pid,
            "process_start": process_start,
            "boot_identity": boot,
            "scope": "write",
            "purpose": "a fixture holder",
            "held_ms": 1_792_525_019_847_u64,
        });
        let mut bytes = serde_json::to_vec_pretty(&value).expect("the hold encodes");
        bytes.push(b'\n');
        bytes
    }
}
