//! Test-only fixture helpers: one fresh directory per test.
//!
//! Every fixture lives inside a run root and never at a fixed path, so two tests
//! — or two tasks on one machine — cannot share a name. The run root is the fresh
//! `run.*` directory a caller names in [`SCRATCH_ROOT_ENV`] when it has one (a task
//! confined to one directory, for instance), and otherwise a fresh `run.*` root
//! this process creates and removes for the fixture, under [`scratch_base`]:
//! nothing is ever created directly in the shared scratch parent. Tests that need
//! the state directory use [`crate::state::StatePaths`] over a directory here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The scratch directory AGENTS.md names for scratch writes when the Builds
/// drive is present. Nothing is ever written to a fixed name inside it: every
/// fixture is a fresh directory this process creates and removes, inside a run
/// root of its own unless the caller supplies one.
pub const SCRATCH_BASE: &str = "/Volumes/Builds/scratch/barge";

/// The variable that names the run root one invocation's fixtures live in.
///
/// A caller that wants every fixture of a run inside one directory it owns — a
/// task that must not write in the shared scratch parent, for instance — creates
/// a fresh `mktemp -d /Volumes/Builds/scratch/barge/run.XXXXXX`, names it here and
/// removes it when the run is over. Unset, each fixture gets a `run.*` root of its
/// own under [`scratch_base`], which it removes itself.
pub const SCRATCH_ROOT_ENV: &str = "BARGE_TEST_SCRATCH_ROOT";

/// The base a fixture is created under.
///
/// AGENTS.md asks for scratch inside the Builds drive when it is present. When it
/// is not, the platform's temporary directory is used, which is still a fresh
/// `run.*` root per fixture rather than a fixed name.
pub fn scratch_base() -> PathBuf {
    let scratch = PathBuf::from(SCRATCH_BASE);
    if scratch.is_dir() {
        scratch
    } else {
        std::env::temp_dir()
    }
}

/// The run root one fixture lives in.
///
/// A supplied root belongs to its caller and is never removed here; one this
/// process creates is removed with the fixture that created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRoot {
    /// The directory fixtures are created inside.
    pub path: PathBuf,
    /// Whether this process created the root and must remove it when empty.
    pub owned: bool,
}

/// Resolve the run root for one invocation: the supplied directory when there is
/// one, and otherwise a fresh root under `parent` that this process owns.
pub fn run_root_in(parent: &Path, supplied: Option<&OsStr>) -> RunRoot {
    match supplied {
        Some(path) if !path.is_empty() => RunRoot {
            path: PathBuf::from(path),
            owned: false,
        },
        _ => RunRoot {
            path: fresh_run_root(parent),
            owned: true,
        },
    }
}

/// The run root this test invocation was given, or a fresh one per fixture.
pub fn scratch_root() -> RunRoot {
    run_root_in(
        &scratch_base(),
        std::env::var_os(SCRATCH_ROOT_ENV).as_deref(),
    )
}

/// A fresh, unused `run.*` name under `parent`. The directory is created by the
/// fixture that moves into it, so a name that is somehow taken is retried there.
fn fresh_run_root(parent: &Path) -> PathBuf {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    parent.join(format!(
        "run.barge-test-{}-{sequence}-{nanos}",
        std::process::id()
    ))
}

/// A temporary fixture that removes itself when the test ends.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
    /// The run root this process created for the fixture, removed once the
    /// fixture is gone and the root is empty. `None` for a root a caller owns.
    owned_root: Option<PathBuf>,
}

impl TempDir {
    /// Create a fresh fixture named after `label` and this process, inside this
    /// invocation's run root.
    pub fn new(label: &str) -> Self {
        Self::under_scratch(label)
    }

    /// Create a fresh directory named after `label` and this process under
    /// `base`, which the caller owns.
    pub fn under(base: &Path, label: &str) -> Self {
        Self {
            path: Self::create_under(base, label),
            owned_root: None,
        }
    }

    /// Create a fresh directory inside this invocation's run root: the fresh
    /// `run.*` directory [`SCRATCH_ROOT_ENV`] names when the caller supplied one,
    /// and otherwise a fresh `run.*` root this process creates and removes.
    /// Nothing is created in the shared scratch parent itself.
    pub fn under_scratch(label: &str) -> Self {
        Self::within_run_root(&scratch_root(), label)
    }

    /// Create a fresh directory inside `root`, and remove the root itself after
    /// this fixture when this process created it.
    pub fn within_run_root(root: &RunRoot, label: &str) -> Self {
        Self {
            path: Self::create_under(&root.path, label),
            owned_root: root.owned.then(|| root.path.clone()),
        }
    }

    /// The path one fresh directory under `base` got, creating `base` when it is
    /// not there.
    fn create_under(base: &Path, label: &str) -> PathBuf {
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
                Ok(()) => return path,
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
        if let Some(root) = self.owned_root.take() {
            // Removing the root succeeds only when this fixture was the last
            // thing in it, so a fixture something else still holds is never
            // deleted. A root the caller supplied is never removed here.
            let _ = std::fs::remove_dir(&root);
        }
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
    ProcessObserver, RemoveKind, RenameOutcome, VolumeFacts, VolumeIdentity, VolumeUuid,
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
                retry: crate::os::RetryEvidence::default(),
                open: Vec::new(),
            }),
            ScanMode::Open(open) => Ok(DescriptorScan::Complete {
                processes: 3,
                descriptors: 12,
                foreign: 0,
                retry: crate::os::RetryEvidence::default(),
                open: open.clone(),
            }),
            ScanMode::Bounded => Ok(DescriptorScan::Bounded {
                processes: 4096,
                descriptors: 65_536,
                bound: "descriptors",
                retry: crate::os::RetryEvidence::default(),
            }),
            ScanMode::Unreadable => Err(Error::new(
                ErrorKind::UnknownState,
                "the open-descriptor scan is unreadable",
            )),
        }
    }
}

/// A probe the test installs at one moment of a walk: it is handed the entry
/// name and may change the fixture in that window.
pub type ObserveProbe = Box<dyn Fn(&str)>;

/// A probe the test installs at a put-back: it is handed the private name and
/// answers one fact about the state at that moment.
pub type PutBackProbe = Box<dyn Fn(&str) -> bool>;

/// A probe the test installs at a mode change: it is handed the mode and answers
/// one fact about the state at that moment.
pub type ModeProbe = Box<dyn Fn(u32) -> bool>;

/// The platform's directory operations with the faults a platform cannot be
/// asked to produce: a foreign owner, an entry on another device, an
/// `ENOTEMPTY` that keeps coming back, a racer that replaces an entry after the
/// deletion observed it, a racer that takes a name back while the captured entry
/// is held privately, and a device change after the quarantine rename.
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
    /// Entries a racer replaces after the deletion observed them, which is the
    /// window between a check and the capture that follows it.
    pub races: RefCell<Vec<Race>>,
    /// Names a racer takes back as soon as they are captured, so a put-back finds
    /// its own name occupied.
    pub occupy_after_capture: RefCell<Vec<String>>,
    /// A probe the test runs the moment the walk observes an entry, so it can
    /// change the fixture in the window a real fault would happen in.
    pub observe_probe: RefCell<Option<ObserveProbe>>,
    /// A probe the test runs the moment an entry is put back: it is handed the
    /// private name and answers one fact about the state at that moment, which
    /// [`Faults::put_back_result`] returns.
    pub put_back_probe: RefCell<Option<PutBackProbe>>,
    put_back_result: RefCell<Option<bool>>,
    /// Whether a put-back rename fails: the captured object then stays under its
    /// private name, as it does when the process stops between the record and the
    /// put-back.
    pub fail_put_back: Cell<bool>,
    /// Whether the next private-name removal answers "already gone", as it does
    /// when another process moves the captured object mid-run.
    pub vanishing: Cell<bool>,
    /// Vanish the Nth private-name read (counted from one), as another process
    /// moving the captured object does between a listing and its read.
    pub vanishing_facts: Cell<usize>,
    /// The area folder and the quarantine-name prefix a racer uses to replace a
    /// leaf with a foreign directory just before Barge captures it for removal.
    pub leaf_swap: RefCell<Option<(PathBuf, String)>>,
    /// Whether the next private-name open swaps the name for a foreign directory,
    /// as another process moving the capture does between a read and its open.
    pub swap_on_capture_open: Cell<bool>,
    /// A device the descriptor a capture is verified through reports, so the
    /// parent binding fails and the captured facts cannot be read.
    pub parent_device_shift: Cell<Option<DeviceId>>,
    /// A name whose entry read fails, so the walk aborts where a real read error
    /// would abort it.
    pub failing_facts_name: Option<String>,
    /// Which observation of that name fails, counted from one.
    pub failing_facts_observation: usize,
    /// How many times the wrapper has been asked for that name.
    facts_seen: Cell<usize>,
    /// Every mode this wrapper set on a directory, in order.
    pub mode_changes: RefCell<Vec<u32>>,
    /// Fail a mode change back to a mode without owner write: the restore fails.
    pub fail_mode_restore: Cell<bool>,
    /// A probe run the moment a mode is set, handed the mode and answering one fact
    /// about the state at that moment.
    pub mode_probe: RefCell<Option<ModeProbe>>,
    mode_probe_answers: RefCell<Vec<bool>>,
    /// Fail the listing that follows a mode change: an abort after the chmod.
    pub fail_listing_after_mode: Cell<bool>,
    /// Set once a mode change has happened and a failure is armed for it.
    arm_after_mode: Cell<bool>,
    /// Fail the entry read that follows a mode change.
    pub fail_facts_after_mode: Cell<bool>,
}

/// What a racer puts at a name after the deletion observed the entry there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// A regular file holding these bytes.
    File(Vec<u8>),
    /// A symbolic link to this target, which a deletion must never follow.
    Symlink(PathBuf),
    /// A directory holding one regular file with these bytes.
    Directory {
        /// The name of the file inside the racer's directory.
        name: String,
        /// The bytes of that file.
        bytes: Vec<u8>,
    },
}

/// One entry a racer replaces after the deletion observed it.
///
/// The observation counter is what makes the race land in the delete pass rather
/// than in the classification pass: the classification observes each entry first,
/// so `2` is the deletion's own check and the replacement lands between that
/// check and the capture that follows it.
#[derive(Debug)]
pub struct Race {
    /// The folder holding the entry: the root being deleted, or the area folder
    /// whose quarantine leaf holds it once the root has been renamed.
    pub holder: PathBuf,
    /// The entry's path inside the holder (or inside its quarantine leaf).
    pub relative: PathBuf,
    /// Which observation of the entry's name fires, counted from one.
    pub observation: usize,
    /// What it puts at the name.
    pub replacement: Replacement,
    seen: Cell<usize>,
}

impl Race {
    /// Replace the entry at `relative` after its `observation`-th check.
    pub fn after(
        holder: &Path,
        relative: &str,
        observation: usize,
        replacement: Replacement,
    ) -> Self {
        Self {
            holder: holder.to_path_buf(),
            relative: PathBuf::from(relative),
            observation,
            replacement,
            seen: Cell::new(0),
        }
    }
}

/// Which shape a planted entry has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantKind {
    /// A directory, so a Finder-shaped *name* that is not a regular file is
    /// exercised.
    Directory,
    /// A regular file, which is Finder metadata by class.
    File,
}

/// A directory to create inside the quarantine leaf mid-delete.
pub struct Plant {
    /// The area folder that holds the quarantine leaf.
    pub area: PathBuf,
    /// The name to create, which is why a Finder-shaped name is interesting.
    pub name: String,
    /// After how many successful file removals it is created.
    pub after_removals: usize,
    /// Whether the planted entry is a directory or a regular file.
    pub kind: PlantKind,
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
            kind: PlantKind::Directory,
            removed: 0,
            done: false,
        });
    }

    /// Create a regular file named `name` inside the quarantine leaf after
    /// `after_removals` successful file removals: the Finder metadata Barge
    /// tolerates by class when it is recreated mid-delete.
    pub fn planting_file(&self, area: &Path, name: &str, after_removals: usize) {
        *self.plant.borrow_mut() = Some(Plant {
            area: area.to_path_buf(),
            name: name.to_owned(),
            after_removals,
            kind: PlantKind::File,
            removed: 0,
            done: false,
        });
    }

    /// Replace `relative` inside `holder` - or inside the quarantine leaf
    /// `holder` contains, once the root has been renamed - with `replacement`
    /// after its `observation`-th check, which is the racer the
    /// capture-and-verify rule exists for.
    pub fn racing(
        &self,
        holder: &Path,
        relative: &str,
        observation: usize,
        replacement: Replacement,
    ) {
        self.races
            .borrow_mut()
            .push(Race::after(holder, relative, observation, replacement));
    }

    /// Run `probe` the moment the walk observes an entry.
    pub fn observing(&self, probe: ObserveProbe) {
        *self.observe_probe.borrow_mut() = Some(probe);
    }

    /// Run `probe` the moment an entry is put back, and keep its answer.
    pub fn probing_put_back(&self, probe: PutBackProbe) {
        *self.put_back_probe.borrow_mut() = Some(probe);
    }

    /// What the put-back probe answered, when it ran.
    pub fn put_back_result(&self) -> Option<bool> {
        *self.put_back_result.borrow()
    }

    /// Fail every put-back rename: the captured object never returns to its
    /// ordinary name.
    pub fn failing_put_back(&self) {
        self.fail_put_back.set(true);
    }

    /// Let other processes move the first private name away before Barge unlinks
    /// it: the removal answers "already gone", as it does mid-run.
    pub fn vanishing_capture(&self) {
        self.vanishing.set(true);
    }

    /// Let another process move the `observation`-th private name away between its
    /// listing and its read, so the read answers "not there".
    pub fn vanishing_capture_at(&self, observation: usize) {
        self.vanishing_facts.set(observation);
    }

    /// Let another process put its own directory at the quarantine leaf's path just
    /// before Barge captures the leaf for its final removal. `prefix` is the leaf's
    /// name prefix, `.barge-prune-`, because the intent id is chosen per run.
    pub fn swapping_leaf(&self, area: &Path, prefix: &str) {
        *self.leaf_swap.borrow_mut() = Some((area.to_path_buf(), prefix.to_owned()));
    }

    /// Swap the next private name for a foreign directory at its open.
    pub fn swapping_capture_on_open(&self) {
        self.swap_on_capture_open.set(true);
    }

    /// Run `probe` the moment a mode is set, and keep its answer.
    pub fn probing_mode(&self, probe: ModeProbe) {
        *self.mode_probe.borrow_mut() = Some(probe);
    }

    /// What the mode probe answered on each mode it saw, in order.
    pub fn mode_probe_answers(&self) -> Vec<bool> {
        self.mode_probe_answers.borrow().clone()
    }

    /// Replace the quarantine leaf with a foreign directory, once.
    fn maybe_swap_leaf(&self, from: &str) {
        let mut swap = self.leaf_swap.borrow_mut();
        let Some((area, prefix)) = swap.as_ref() else {
            return;
        };
        if !from.starts_with(prefix.as_str()) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(area) else {
            return;
        };
        let Some(path) = entries
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(from))
        else {
            return;
        };
        // The leaf is emptied before this step, so removing it and putting a foreign
        // directory in its place is what a racer can do at the area level.
        // An *empty* foreign directory is the hazardous case: a removal by the
        // visible name would succeed and record a completion Barge never earned.
        if std::fs::remove_dir(&path).is_ok() && std::fs::create_dir(&path).is_ok() {
            *swap = None;
        }
    }

    /// Fail the `observation`-th entry read of `name`, aborting the walk where a
    /// real read error would. Counting from one is how a test lands the abort in
    /// the delete pass rather than in the classification.
    pub fn failing_facts_at(&mut self, name: &str, observation: usize) {
        self.failing_facts_name = Some(name.to_owned());
        self.failing_facts_observation = observation;
    }

    /// Report another device for the descriptor a capture is verified through.
    pub fn shifting_parent(&self, device: DeviceId) {
        self.parent_device_shift.set(Some(device));
    }

    /// The modes this wrapper set on directories, in order.
    pub fn modes_set(&self) -> Vec<u32> {
        self.mode_changes.borrow().clone()
    }

    /// Fail every mode restore, so the recorded mode is never put back.
    pub fn failing_mode_restore(&self) {
        self.fail_mode_restore.set(true);
    }

    /// Fail the listing after a mode change: an abort after the chmod.
    pub fn failing_listing_after_mode(&self) {
        self.fail_listing_after_mode.set(true);
    }

    /// Fail the entry read after a mode change: an abort after the chmod.
    pub fn failing_facts_after_mode(&self) {
        self.fail_facts_after_mode.set(true);
    }

    /// Take the entry named `name` back as soon as it is captured away from its
    /// name, so a put-back finds its original name occupied.
    pub fn occupying_after_capture(&self, name: &str) {
        self.occupy_after_capture.borrow_mut().push(name.to_owned());
    }

    /// Run the racers whose name was just observed.
    ///
    /// A file and a directory are replaced through the handle the check read, so a
    /// race lands in the directory the deletion is looking at at any depth: the
    /// deletion renames the root into its quarantine leaf, and then renames each
    /// captured directory, so no fixed path stays valid. A symlink can only be
    /// made by path, so that replacement follows the root into its quarantine leaf
    /// and the tests that use it race a top-level entry.
    fn maybe_race(&self, parent: &DirHandle, name: &str) {
        let mut races = self.races.borrow_mut();
        for race in races.iter_mut() {
            if race.relative.file_name().and_then(|part| part.to_str()) != Some(name) {
                continue;
            }
            let seen = race.seen.get().saturating_add(1);
            race.seen.set(seen);
            if seen != race.observation {
                continue;
            }
            self.replace_entry(parent, name, race);
        }
    }

    /// Put `race.replacement` where the checked entry was.
    fn replace_entry(&self, parent: &DirHandle, name: &str, race: &Race) {
        match &race.replacement {
            Replacement::File(bytes) => {
                // A racer publishes over the name it observed, which is one atomic
                // rename onto a fresh inode: the check it raced no longer describes
                // what is there.
                let _ = self.inner.publish_child_file(parent, name, bytes, 0o644);
            }
            Replacement::Directory { name: held, bytes } => {
                let _ = self.inner.remove_child(parent, name, RemoveKind::Directory);
                if self.inner.create_child_dir(parent, name, 0o755).is_ok() {
                    if let Ok(child) = self.inner.open_child_dir(parent, name) {
                        let _ = self.inner.publish_child_file(&child, held, bytes, 0o644);
                    }
                }
            }
            Replacement::Symlink(target) => {
                let path = raced_path(&race.holder, &race.relative);
                let _ = std::fs::remove_file(&path);
                let _ = std::os::unix::fs::symlink(target, &path);
            }
        }
    }

    /// Take back a captured entry's original name, in the directory the capture
    /// happened in, so a put-back there finds it occupied at any depth.
    fn maybe_occupy(&self, parent: &DirHandle, from: &str, to: &str) {
        if !to.starts_with(crate::state::CAPTURE_PREFIX) {
            return;
        }
        for name in self.occupy_after_capture.borrow().iter() {
            if name != from {
                continue;
            }
            let _ =
                self.inner
                    .publish_child_file(parent, from, b"a racer took this name back", 0o644);
        }
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
                let target = entry.path().join(&plant.name);
                let planted = match plant.kind {
                    PlantKind::Directory => std::fs::create_dir(&target),
                    PlantKind::File => std::fs::write(&target, b"finder"),
                };
                if planted.is_ok() {
                    plant.done = true;
                }
                break;
            }
        }
    }
}

/// Resolve a raced entry: the quarantine leaf `holder` contains when it holds
/// one, otherwise `holder` itself.
///
/// A deletion renames the root into `.barge-prune-<intent-id>` before it walks it,
/// so a racer that only knew the root's path has to follow it in.
fn raced_path(holder: &Path, relative: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(holder) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(crate::state::QUARANTINE_PREFIX)
            {
                return entry.path().join(relative);
            }
        }
    }
    holder.join(relative)
}

impl DirOps for Faults {
    fn open_path(&self, path: &Path) -> Result<DirHandle> {
        self.inner.open_path(path)
    }

    fn path_facts(&self, path: &Path) -> Result<Option<EntryFacts>> {
        self.inner.path_facts(path)
    }

    fn open_child_dir(&self, parent: &DirHandle, name: &str) -> Result<DirHandle> {
        if self.swap_on_capture_open.get() && name.starts_with(crate::state::CAPTURE_PREFIX) {
            self.swap_on_capture_open.set(false);
            // Another process moved the captured object away and put its own
            // directory at the private name, between the read and this open.
            let moved = format!("{name}.moved");
            let _ = self.inner.rename_child(parent, name, &moved);
            let _ = self.inner.create_child_dir(parent, name, 0o755);
        }
        self.inner.open_child_dir(parent, name)
    }

    fn duplicate_dir(&self, handle: &DirHandle) -> Result<DirHandle> {
        self.inner.duplicate_dir(handle)
    }

    fn child_facts(&self, parent: &DirHandle, name: &str) -> Result<Option<EntryFacts>> {
        if name.starts_with(crate::state::CAPTURE_PREFIX) && self.vanishing_facts.get() > 0 {
            let remaining = self.vanishing_facts.get().saturating_sub(1);
            self.vanishing_facts.set(remaining);
            if remaining == 0 {
                // Another process moved the captured object away: the entry is
                // listed, and by the time it is read it is not there.
                let _ = self.inner.remove_child(parent, name, RemoveKind::File);
                let _ = self.inner.remove_child(parent, name, RemoveKind::Directory);
                return Ok(None);
            }
        }
        if self.failing_facts_name.as_deref() == Some(name) {
            let seen = self.facts_seen.get().saturating_add(1);
            self.facts_seen.set(seen);
            if seen == self.failing_facts_observation {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("the entry read of `{name}` did not happen"),
                ));
            }
        }
        if self.arm_after_mode.get() && self.fail_facts_after_mode.get() {
            self.arm_after_mode.set(false);
            return Err(Error::new(
                ErrorKind::Io,
                "the entry read after the mode change did not happen",
            ));
        }
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
        // The racer replaces the entry after the check that just read these
        // facts, so the returned observation describes an object that is no
        // longer at the name: the exact window a check-then-unlink by name loses.
        self.maybe_race(parent, name);
        if let Some(probe) = self.observe_probe.borrow().as_ref() {
            probe(name);
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
        if self.arm_after_mode.get() && self.fail_listing_after_mode.get() {
            self.arm_after_mode.set(false);
            return Err(Error::new(
                ErrorKind::Io,
                "the listing after the mode change did not happen",
            ));
        }
        self.inner.list_children(parent, max_names)
    }

    fn remove_child(
        &self,
        parent: &DirHandle,
        name: &str,
        kind: RemoveKind,
    ) -> Result<ChildRemoval> {
        if self.vanishing.get() && name.starts_with(crate::state::CAPTURE_PREFIX) {
            self.vanishing.set(false);
            // Another process moved the captured object away between Barge's check
            // and its unlink: the removal answers "already gone".
            let _ = self.inner.remove_child(parent, name, kind);
            return Ok(ChildRemoval::Absent);
        }
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

    fn rename_child(&self, parent: &DirHandle, from: &str, to: &str) -> Result<RenameOutcome> {
        if to.starts_with(crate::state::CAPTURE_PREFIX) {
            self.maybe_swap_leaf(from);
        }
        if from.starts_with(crate::state::CAPTURE_PREFIX) {
            if self.fail_put_back.get() {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("the put-back of `{from}` did not happen"),
                ));
            }
            if let Some(probe) = self.put_back_probe.borrow().as_ref() {
                let answer = probe(from);
                *self.put_back_result.borrow_mut() = Some(answer);
            }
        }
        let outcome = self.inner.rename_child(parent, from, to)?;
        if outcome == RenameOutcome::Renamed {
            self.maybe_occupy(parent, from, to);
            if let Some(volume) = &self.volume {
                volume.shift_device();
            }
        }
        Ok(outcome)
    }

    fn dir_facts(&self, handle: &DirHandle) -> Result<EntryFacts> {
        self.inner.dir_facts(handle)
    }

    fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> Result<()> {
        if let Some(probe) = self.mode_probe.borrow().as_ref() {
            let answer = probe(mode);
            self.mode_probe_answers.borrow_mut().push(answer);
        }
        if self.fail_mode_restore.get() && mode & 0o200 == 0 {
            // The restore never reaches the platform: the mode stays changed.
            return Err(Error::new(ErrorKind::Io, "the mode restore did not happen"));
        }
        self.inner.set_dir_mode(handle, mode)?;
        self.mode_changes.borrow_mut().push(mode);
        if self.fail_listing_after_mode.get() || self.fail_facts_after_mode.get() {
            self.arm_after_mode.set(true);
        }
        Ok(())
    }

    fn recheck_dir(&self, handle: &DirHandle) -> Result<DirId> {
        let identity = self.inner.recheck_dir(handle)?;
        if let Some(device) = self.parent_device_shift.get() {
            return Ok(DirId {
                device,
                inode: identity.inode,
            });
        }
        Ok(identity)
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
        Self::within(TempDir::under_scratch(label))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::Path;

    use super::{run_root_in, scratch_base, scratch_root, RunRoot, TempDir, SCRATCH_ROOT_ENV};

    /// How many entries a fresh fixture parent holds.
    fn entries(directory: &Path) -> usize {
        std::fs::read_dir(directory)
            .expect("the directory is listed")
            .count()
    }

    #[test]
    fn a_fixture_stays_inside_the_run_root_it_is_given() {
        // The parent stands in for the run scratch area, and `run.supplied` for
        // the fresh `run.*` directory a test invocation names in
        // `SCRATCH_ROOT_ENV`. The fixture must land inside that root, nothing may
        // appear beside it, and the root must survive the fixture: the caller
        // owns it.
        let parent = TempDir::new("run-root-supplied");
        let supplied = parent.join("run.supplied");
        let root = run_root_in(parent.path(), Some(supplied.as_os_str()));
        assert_eq!(
            root,
            RunRoot {
                path: supplied.clone(),
                owned: false,
            }
        );
        let fixture = TempDir::within_run_root(&root, "contained");
        assert!(
            fixture.path().starts_with(&supplied),
            "the fixture is inside the supplied run root: {}",
            fixture.path().display()
        );
        assert_eq!(fixture.path().parent(), Some(supplied.as_path()));
        assert_eq!(
            entries(parent.path()),
            1,
            "the run root is the only entry in the scratch area"
        );
        let path = fixture.path().to_path_buf();
        drop(fixture);
        assert!(!path.exists(), "the fixture removes itself");
        assert!(
            supplied.is_dir(),
            "a supplied run root is left for its owner"
        );
        assert_eq!(entries(&supplied), 0, "the run root is left empty");
        assert_eq!(entries(parent.path()), 1);
    }

    #[test]
    fn a_fixture_that_creates_its_own_run_root_removes_it() {
        // Without a supplied root, the fixture gets a fresh `run.*` root of its
        // own under the scratch base, and removes that root with itself: nothing
        // is created beside it and nothing is left behind.
        let parent = TempDir::new("run-root-owned");
        let root = run_root_in(parent.path(), None);
        assert!(root.owned);
        assert_eq!(root.path.parent(), Some(parent.path()));
        assert!(
            root.path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run.")),
            "the root is a fresh `run.*` name: {}",
            root.path.display()
        );
        let fixture = TempDir::within_run_root(&root, "owned");
        assert!(fixture.path().starts_with(&root.path));
        assert_eq!(entries(parent.path()), 1);
        drop(fixture);
        assert!(!root.path.exists(), "the owned run root is removed");
        assert_eq!(entries(parent.path()), 0, "nothing is left beside it");
    }

    #[test]
    fn the_invocations_run_root_is_what_a_fixture_lives_in() {
        // The wiring, read-only: a supplied root is used as it is and belongs to
        // the caller; without one, the fixture base is a fresh `run.*` directory
        // under the scratch base rather than the shared parent itself.
        let root = scratch_root();
        match std::env::var_os(SCRATCH_ROOT_ENV) {
            Some(supplied) => {
                assert_eq!(root.path, std::path::PathBuf::from(supplied));
                assert!(!root.owned, "a supplied run root is never removed here");
            }
            None => {
                assert!(root.owned);
                assert_eq!(root.path.parent(), Some(scratch_base().as_path()));
                assert!(
                    root.path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("run.")),
                    "the fixture base is a run root, not the shared parent: {}",
                    root.path.display()
                );
            }
        }
    }
}
