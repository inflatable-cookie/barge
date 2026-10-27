//! Package caches: the cache kinds, the three placement forms (`setting`,
//! `held-root`, `link`) and their undo (stage 1 design, "Package-cache
//! placements").
//!
//! Four kinds are designed: the npm cache, redirected by `cache=` in
//! `<account home>/.npmrc`; the bun cache, redirected by `[install.cache] dir`
//! in `<account home>/.bunfig.toml`; and Cargo's `registry` and `git` caches
//! under `<account home>/.cargo/`. The first two can only be redirected by their
//! own setting (Q-003), the last two by a link at the tool's path or by
//! adopting the directory they already use.
//!
//! Three placement forms, recorded in one `barge.placement/1` document per kind:
//!
//! - **`setting`** writes the tool's own setting once, during an explicit adopt,
//!   and records the file's identity and its exact before and after bytes. No
//!   filesystem link is involved. Undo restores the recorded bytes only when the
//!   file still holds the exact bytes Barge wrote, so a file someone edited
//!   after adopt is refused rather than overwritten.
//! - **`link`** publishes an owned root under the volume's `caches/` folder as a
//!   symbolic link at the tool's path, through a temporary name, so the path is
//!   never briefly missing. It is mechanically available only where no
//!   directory is in the way: an absent path, or an empty real directory, which
//!   is removed only after an empty-directory custody proof.
//! - **`held-root`** adopts a directory that is already where the data lives, on
//!   the registered volume, by writing `.barge-root.json`
//!   (`origin: adopted_existing`) into it. Nothing is renamed, linked, copied or
//!   moved. This is the form today's hand-made Cargo placement takes: its
//!   directory already sits under `caches/cargo/`.
//!
//! A non-empty real directory is never replaced by a link: a symlink cannot be
//! renamed over a directory, and deleting its contents would be the data move
//! this slice forbids. Adopt refuses with `refused_boundary` and a plan naming
//! the directory, what it holds and the forms that are available.
//!
//! Every identity a later undo proves is recorded: the exact bytes and digests
//! for a setting, the recorded link text for a link, and the directory's device,
//! inode and marker digest for a held root. Undo changes nothing it cannot
//! prove, and it never deletes a cache's data.
//!
//! This module mutates nothing the real account uses unless a command is given
//! explicitly: its unit and counterfactual tests run on fixture directories,
//! fixture settings files and fixture volumes.

mod undo_capture;

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::area::{
    self, Feature, FeatureFolder, MarkerExpectation, MarkerState, MarkerWrite, RootKind, RootOrigin,
};
use crate::contract::{PLACEMENT_CAP_BYTES, PLACEMENT_ROWS, PLACEMENT_SCHEMA, SETTING_CAP_BYTES};
use crate::custody::{
    self, CustodyBounds, PendingIntent, SCAN_DESCRIPTORS, SCAN_PROCESSES, SCAN_TIMEOUT,
};
use crate::describe::{AbsentPolicy as RowAbsentPolicy, LinkState, PlacementKind, PlacementRow};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    AccountFacts, DescriptorScan, DescriptorScanRequest, DeviceId, DirHandle, DirOps, EntryFacts,
    EntryKind, MountRoot, ProcessObserver, RemoveKind, RenameOutcome, SelfExclusion, VolumeFacts,
    VolumeUuid,
};
use crate::registry::VolumeRecord;
use crate::state::{self, Bounds, Deadline, LockSet, ReadOutcome, StatePaths, READ_DEADLINE};

/// The namespace and area a cache root's marker records.
///
/// `caches/` is one of Barge's own folders rather than a consumer area, so the
/// marker names Barge itself and carries no effective consumer declaration.
pub const CACHE_AREA_NAMESPACE: &str = "barge";
/// The area name a cache root's marker records.
pub const CACHE_AREA_NAME: &str = "caches";

/// The suffix of the backup copy adopt writes beside a tool's settings file
/// before it changes it.
pub const SETTING_BACKUP_SUFFIX: &str = ".barge-backup";

/// How many of a refused directory's entry names a plan reports. The count is
/// always the real one; this only bounds the names.
const PLAN_ENTRY_NAMES: usize = 16;

/// The message a cache command gives when its volume is away (Q-001): package
/// caches fail clearly and never fall back to the internal drive.
fn absent_volume(error: Error, record: &VolumeRecord) -> Error {
    if error.kind() != ErrorKind::VolumeAbsent {
        return error;
    }
    let attach = record
        .mount_hint
        .as_ref()
        .map(|hint| hint.display().to_string())
        .unwrap_or_else(|| record.volume_uuid.as_str().to_owned());
    Error::new(
        ErrorKind::VolumeAbsent,
        format!(
            "the cache's volume is not attached: attach {attach}. A package cache never falls back to the internal drive",
        ),
    )
    .with_detail("volume_uuid", record.volume_uuid.as_str().into())
    .with_detail("attach", attach.into())
}

/// A cache kind: what the placement serves (stage 1 design, "Kinds and
/// placement forms").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    /// The npm cache, redirected by `cache=` in `~/.npmrc`.
    NpmCache,
    /// The bun cache, redirected by `[install.cache] dir` in `~/.bunfig.toml`.
    BunCache,
    /// Cargo's registry cache, `<account home>/.cargo/registry`.
    CargoRegistry,
    /// Cargo's git cache, `<account home>/.cargo/git`.
    CargoGit,
}

impl CacheKind {
    /// Every kind, in the design's order.
    pub const ALL: [CacheKind; 4] = [
        CacheKind::NpmCache,
        CacheKind::BunCache,
        CacheKind::CargoRegistry,
        CacheKind::CargoGit,
    ];

    /// Parse the command-line spelling, hyphenated or underscored.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.replace('_', "-").as_str() {
            "npm-cache" => Ok(Self::NpmCache),
            "bun-cache" => Ok(Self::BunCache),
            "cargo-registry" => Ok(Self::CargoRegistry),
            "cargo-git" => Ok(Self::CargoGit),
            other => Err(Error::new(
                ErrorKind::Usage,
                format!(
                    "`{other}` is not a cache kind; use npm-cache, bun-cache, cargo-registry or cargo-git"
                ),
            )),
        }
    }

    /// The name a record and a document use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NpmCache => "npm_cache",
            Self::BunCache => "bun_cache",
            Self::CargoRegistry => "cargo_registry",
            Self::CargoGit => "cargo_git",
        }
    }

    /// The path the tool uses today, relative to the account home.
    ///
    /// The account home is the password database's, never `$HOME`, so a fixture
    /// or a stray environment cannot re-point a placement.
    pub const fn original_relative_path(self) -> &'static str {
        match self {
            Self::NpmCache => ".npmrc",
            Self::BunCache => ".bunfig.toml",
            Self::CargoRegistry => ".cargo/registry",
            Self::CargoGit => ".cargo/git",
        }
    }

    /// The folder a new placement creates inside `caches/`, when it has to name
    /// one. It matches today's hand-made layout (`caches/npm`, `caches/bun`).
    pub const fn folder_name(self) -> &'static str {
        match self {
            Self::NpmCache => "npm",
            Self::BunCache => "bun",
            Self::CargoRegistry => "cargo-registry",
            Self::CargoGit => "cargo-git",
        }
    }

    /// The setting that redirects this kind, or `None` when it is redirected by
    /// a link.
    pub fn setting_key(self) -> Option<SettingKey> {
        match self {
            Self::NpmCache => Some(SettingKey {
                section: None,
                key: "cache".to_owned(),
            }),
            Self::BunCache => Some(SettingKey {
                section: Some("install.cache".to_owned()),
                key: "dir".to_owned(),
            }),
            Self::CargoRegistry | Self::CargoGit => None,
        }
    }

    /// The row shape `describe` reports for this kind.
    pub const fn placement_kind(self) -> PlacementKind {
        match self {
            Self::NpmCache => PlacementKind::NpmCache,
            Self::BunCache => PlacementKind::BunCache,
            Self::CargoRegistry => PlacementKind::CargoRegistry,
            Self::CargoGit => PlacementKind::CargoGit,
        }
    }
}

/// One setting's address: an optional section and one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingKey {
    /// The section header without brackets, for a format with sections.
    pub section: Option<String>,
    /// The key inside that section, or at the file's top level.
    pub key: String,
}

/// One settings file's identity and exact bytes, at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    /// The file's device.
    pub device: u64,
    /// The file's inode on that device.
    pub inode: u64,
    /// Its size in bytes.
    pub size: u64,
    /// Its permission bits, so a restore keeps them.
    pub mode: u32,
    /// Its modification time, when the platform reports one.
    pub modified_ms: Option<u64>,
    /// The SHA-256 of exactly these bytes.
    pub digest: String,
    /// The exact bytes, hexadecimal, so an undo restores them byte for byte.
    pub bytes: String,
}

/// A directory's own facts as they were before Barge adopted it in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreAdoption {
    /// The directory's device.
    pub device: u64,
    /// The directory's inode.
    pub inode: u64,
    /// How many entries the directory held.
    pub entries: u64,
    /// The newest modification time among those entries, when one was readable.
    pub newest_modified_ms: Option<u64>,
    /// The directory's own allocated bytes. It is not the tree's size: this
    /// observation claims nothing about the contents' allocation.
    pub allocated_bytes: u64,
}

/// A directory Barge adopted in place, with every identity an undo needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldRoot {
    /// The directory the marker was written into.
    pub path: PathBuf,
    /// The fresh root id the marker records.
    pub root_id: String,
    /// The marker's fresh nonce.
    pub nonce: String,
    /// The directory's device.
    pub device: u64,
    /// The directory's inode.
    pub inode: u64,
    /// The SHA-256 of the exact marker bytes written.
    pub marker_digest: String,
    /// The facts observed immediately before the marker was written.
    pub observed: PreAdoption,
}

/// A link that already published a held root before Barge adopted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldRootLink {
    /// The tool's path the link lives at.
    pub link_path: PathBuf,
    /// The exact link text recorded at adopt.
    pub target: PathBuf,
}

/// The `setting` form's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingForm {
    /// The tool's settings file. It is also the placement's original path.
    pub settings_path: PathBuf,
    /// The key that redirects the tool.
    pub key: SettingKey,
    /// Whether adopt changed a byte.
    pub changed: bool,
    /// The file before adopt, or `None` when it did not exist.
    pub before: Option<SettingsSnapshot>,
    /// The file as adopt left it.
    pub after: SettingsSnapshot,
    /// The backup copy adopt wrote before changing the file, when it changed it.
    pub backup_path: Option<PathBuf>,
    /// The directory the setting now names.
    pub data_path: PathBuf,
    /// Whether adopt created `data_path` as a plain directory.
    pub created_directory: bool,
    /// A directory adopted in place at `data_path`, when the caller asked.
    pub held_root: Option<HeldRoot>,
}

/// What was at the original path before a link was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkRestore {
    /// Nothing was there: undo removes the link.
    Absent,
    /// An empty directory was there: undo removes the link and restores it.
    EmptyDirectory,
}

/// The `link` form's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkForm {
    /// The tool's path the link was published at.
    pub link_path: PathBuf,
    /// The exact link text Barge published.
    pub target: PathBuf,
    /// What the original path was before the link.
    pub restore: LinkRestore,
    /// Original empty-directory permission bits; absent on old records (undo refuses).
    #[serde(default)]
    pub original_mode: Option<u32>,
    /// Device and inode of the link actually published, never inferred from text.
    #[serde(default)]
    pub published_identity: Option<(u64, u64)>,
    /// The owned root's id, so the target's name is never trusted alone.
    pub root_id: String,
    /// The SHA-256 of the owned root's marker bytes.
    pub marker_digest: String,
}

/// The `held-root` form's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldRootForm {
    /// The directory that was adopted in place.
    pub path: PathBuf,
    /// The link that published it before adopt, when there was one. Adopt never
    /// swaps such a link; undo verifies it and leaves it alone.
    pub publication: Option<HeldRootLink>,
    /// The adopted root's identity.
    pub held_root: HeldRoot,
}

/// One of the three placement forms, and what its undo must restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum PlacementForm {
    /// The tool's own setting publishes the placement.
    Setting(Box<SettingForm>),
    /// A symbolic link at the tool's path publishes an owned root.
    Link(LinkForm),
    /// A directory on the volume was adopted in place.
    HeldRoot(HeldRootForm),
}

impl PlacementForm {
    /// The form's name, as a document reports it.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Setting(_) => "setting",
            Self::Link(_) => "link",
            Self::HeldRoot(_) => "held_root",
        }
    }
}

/// The form a caller asks for, when it wants a specific mechanic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormRequest {
    /// The tool's own setting.
    Setting,
    /// A directory adopted in place.
    HeldRoot,
    /// A symbolic link at the tool's path.
    Link,
}

impl FormRequest {
    /// Parse the command-line spelling.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.replace('_', "-").as_str() {
            "setting" => Ok(Self::Setting),
            "held-root" => Ok(Self::HeldRoot),
            "link" => Ok(Self::Link),
            other => Err(Error::new(
                ErrorKind::Usage,
                format!("`{other}` is not a placement form; use setting, held-root or link"),
            )),
        }
    }

    /// The name a document reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Setting => "setting",
            Self::HeldRoot => "held_root",
            Self::Link => "link",
        }
    }
}

/// One `barge.placement/1` record: one adopted kind and how to undo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRecord {
    /// Always `barge.placement/1`.
    pub schema: String,
    /// The record's fresh 128-bit id, which is also its file name.
    pub placement_id: String,
    /// What the placement serves.
    pub kind: CacheKind,
    /// The tool's own path. It is the settings file for a `setting` form.
    pub original_path: PathBuf,
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The volume's `root_dir` at adopt.
    pub root_dir: String,
    /// The volume's `caches/` folder name at adopt.
    pub folder: String,
    /// Where the data lives: the owned root, the adopted directory or the
    /// directory the setting names.
    pub recorded_path: PathBuf,
    /// When it was adopted.
    pub adopted_ms: u64,
    /// Increases on every change; a record starts at 1.
    pub generation: u64,
    /// The form, with every identity its undo proves.
    pub form: PlacementForm,
}

impl PlacementRecord {
    /// Validate the record strictly against its own schema.
    ///
    /// A record that does not validate is unknown state: it is never read as
    /// absent, never repaired and never used to undo anything.
    pub fn validate(&self) -> Result<()> {
        if self.schema != PLACEMENT_SCHEMA {
            return Err(unknown(format!(
                "`{}` is not the `{PLACEMENT_SCHEMA}` major this build reads",
                self.schema
            )));
        }
        state::validate_identifier("placement_id", &self.placement_id)?;
        if self.generation == 0 {
            return Err(unknown("a placement record's generation starts at 1"));
        }
        for (field, path) in [
            ("original_path", &self.original_path),
            ("recorded_path", &self.recorded_path),
        ] {
            if !path.is_absolute() {
                return Err(unknown(format!(
                    "`{field}` is not an absolute path: {}",
                    path.display()
                )));
            }
        }
        match (&self.form, self.kind) {
            (PlacementForm::Setting(form), CacheKind::NpmCache | CacheKind::BunCache) => {
                form.validate(&self.original_path)?;
                if self.recorded_path != form.data_path {
                    return Err(unknown(
                        "a setting placement's recorded path is not the directory it names",
                    ));
                }
            }
            (PlacementForm::Link(form), CacheKind::CargoRegistry | CacheKind::CargoGit) => {
                form.validate(&self.original_path)?;
                if self.recorded_path != form.target {
                    return Err(unknown(
                        "a link placement's recorded path is not its link text",
                    ));
                }
            }
            (PlacementForm::HeldRoot(form), CacheKind::CargoRegistry | CacheKind::CargoGit) => {
                form.validate(&self.original_path)?;
                if self.recorded_path != form.path {
                    return Err(unknown(
                        "a held-root placement's recorded path is not the adopted directory",
                    ));
                }
            }
            (form, kind) => {
                return Err(unknown(format!(
                    "the `{}` form does not serve `{}`",
                    form.name(),
                    kind.as_str()
                )))
            }
        }
        Ok(())
    }
}

impl SettingForm {
    fn validate(&self, original_path: &Path) -> Result<()> {
        if self.settings_path.as_path() != original_path {
            return Err(unknown(
                "a setting placement's settings file is not its original path",
            ));
        }
        if !self.data_path.is_absolute() {
            return Err(unknown("the setting's data path is not absolute"));
        }
        validate_digest("after.digest", &self.after.digest)?;
        let after_bytes = decode_hex(&self.after.bytes)?;
        if sha256_hex(&after_bytes) != self.after.digest {
            return Err(unknown(
                "the recorded after bytes do not match their digest",
            ));
        }
        if after_bytes.len() as u64 != self.after.size {
            return Err(unknown("the recorded after size is not the byte count"));
        }
        match (&self.before, self.changed) {
            (Some(before), _) => {
                validate_digest("before.digest", &before.digest)?;
                let before_bytes = decode_hex(&before.bytes)?;
                if sha256_hex(&before_bytes) != before.digest {
                    return Err(unknown(
                        "the recorded before bytes do not match their digest",
                    ));
                }
                if !self.changed && before.digest != self.after.digest {
                    return Err(unknown(
                        "a setting placement that changed nothing records different before and after bytes",
                    ));
                }
            }
            (None, false) => {
                return Err(unknown(
                    "a setting placement that changed nothing must record the file it found",
                ))
            }
            (None, true) => {}
        }
        if self.held_root.is_some() && !self.data_path.is_absolute() {
            return Err(unknown("an adopted root lives at an absolute path"));
        }
        if let Some(held) = &self.held_root {
            held.validate()?;
            if held.path != self.data_path {
                return Err(unknown(
                    "the adopted root is not the directory the setting names",
                ));
            }
        }
        Ok(())
    }
}

impl LinkForm {
    fn validate(&self, original_path: &Path) -> Result<()> {
        if self.link_path.as_path() != original_path {
            return Err(unknown("a link placement's link is not its original path"));
        }
        if !self.target.is_absolute() || self.target == self.link_path {
            return Err(unknown(
                "the link's target is not a different absolute path",
            ));
        }
        validate_digest("marker_digest", &self.marker_digest)?;
        state::validate_identifier("root_id", &self.root_id)?;
        Ok(())
    }
}

impl HeldRootForm {
    fn validate(&self, original_path: &Path) -> Result<()> {
        if !self.path.is_absolute() {
            return Err(unknown("an adopted root lives at an absolute path"));
        }
        self.held_root.validate()?;
        if self.held_root.path != self.path {
            return Err(unknown("the adopted root records a different path"));
        }
        if let Some(publication) = &self.publication {
            if publication.link_path.as_path() != original_path {
                return Err(unknown(
                    "the recorded publication is not at the original path",
                ));
            }
            if publication.target != self.path {
                return Err(unknown(
                    "the recorded publication does not point at the adopted root",
                ));
            }
        }
        Ok(())
    }
}

impl HeldRoot {
    fn validate(&self) -> Result<()> {
        if !self.path.is_absolute() {
            return Err(unknown("an adopted root's path is not absolute"));
        }
        state::validate_identifier("root_id", &self.root_id)?;
        state::validate_identifier("nonce", &self.nonce)?;
        validate_digest("marker_digest", &self.marker_digest)?;
        Ok(())
    }
}

fn validate_digest(field: &str, value: &str) -> Result<()> {
    if value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(unknown(format!("`{value}` is not a 64-hex-digit {field}")))
}

fn unknown(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::UnknownState, message)
}

// ---- the store ------------------------------------------------------------

/// The bounded set of stored placement records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementSet {
    /// The records kept, sorted by kind and id.
    pub records: Vec<PlacementRecord>,
    /// How many the state directory held, at least `records.len()`.
    pub total: usize,
    /// Whether records were left out.
    pub truncated: bool,
}

impl PlacementSet {
    /// No records.
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
            total: 0,
            truncated: false,
        }
    }

    /// The record for one kind, if that kind is adopted.
    pub fn find_kind(&self, kind: CacheKind) -> Option<&PlacementRecord> {
        self.records.iter().find(|record| record.kind == kind)
    }

    /// The record with one id, if it is stored.
    pub fn find_id(&self, placement_id: &str) -> Option<&PlacementRecord> {
        self.records
            .iter()
            .find(|record| record.placement_id == placement_id)
    }

    /// The rows a document reports.
    pub fn rows(&self) -> crate::contract::RowSet<PlacementRecord> {
        crate::contract::RowSet::bounded(self.records.clone(), self.total, PLACEMENT_ROWS)
    }
}

/// Where one placement record lives in the state directory.
pub fn placement_path(state: &StatePaths, placement_id: &str) -> Result<PathBuf> {
    state::validate_identifier("placement_id", placement_id)?;
    Ok(state.placements_dir().join(format!("{placement_id}.json")))
}

/// Read every stored placement record strictly.
///
/// A stray name, an unreadable or oversized file, an unknown schema or an
/// invalid record is unknown state naming the path: a silently short list would
/// read as "not adopted", and a wrong record would authorize a wrong undo.
pub fn read_placements(state: &StatePaths) -> Result<PlacementSet> {
    let Some(names) = area::read_directory(&state.placements_dir())? else {
        return Ok(PlacementSet::empty());
    };
    let mut records = Vec::new();
    let mut total = 0usize;
    for name in names {
        let Some(id) = name.strip_suffix(".json") else {
            return Err(unknown(format!(
                "the placements directory holds `{name}`, which is not a placement record"
            ))
            .with_detail(
                "path",
                state
                    .placements_dir()
                    .join(&name)
                    .display()
                    .to_string()
                    .into(),
            ));
        };
        state::validate_identifier("placement_id", id)?;
        let path = placement_path(state, id)?;
        let bytes = match state::read_bounded(
            &path,
            &Bounds::new(
                PLACEMENT_CAP_BYTES,
                PLACEMENT_ROWS,
                Deadline::after(READ_DEADLINE),
            ),
        ) {
            ReadOutcome::Present(bytes) => bytes,
            ReadOutcome::Absent => continue,
            ReadOutcome::Refused(error) => {
                return Err(error.with_detail("path", path.display().to_string().into()))
            }
        };
        let record: PlacementRecord = serde_json::from_slice(&bytes).map_err(|error| {
            unknown(format!(
                "`{PLACEMENT_SCHEMA}` refused the placement record: {error}"
            ))
            .with_detail("path", path.display().to_string().into())
        })?;
        record.validate().map_err(|error| {
            unknown(format!(
                "{} is not a valid placement record: {}",
                path.display(),
                error.message()
            ))
            .with_detail("path", path.display().to_string().into())
        })?;
        if record.placement_id != id {
            return Err(unknown(format!(
                "{} records placement {}",
                path.display(),
                record.placement_id
            ))
            .with_detail("path", path.display().to_string().into()));
        }
        total = total.saturating_add(1);
        if records.len() < PLACEMENT_ROWS {
            records.push(record);
        }
    }
    records.sort_by(|left, right| {
        (left.kind.as_str(), &left.placement_id).cmp(&(right.kind.as_str(), &right.placement_id))
    });
    Ok(PlacementSet {
        records,
        total,
        truncated: total > PLACEMENT_ROWS,
    })
}

/// Publish one placement record under a fresh id.
fn publish_record(state: &StatePaths, record: &PlacementRecord) -> Result<PathBuf> {
    record.validate()?;
    let path = placement_path(state, &record.placement_id)?;
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the placement record: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > PLACEMENT_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the placement record would be {} bytes, over the {PLACEMENT_CAP_BYTES}-byte cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", PLACEMENT_CAP_BYTES.into()));
    }
    state::publish_atomic(&path, &bytes)?;
    Ok(path)
}

/// Remove one placement record. Only undo removes a record, and only after the
/// effect it reverses has been proved and applied.
fn remove_record(state: &StatePaths, placement_id: &str) -> Result<bool> {
    let path = placement_path(state, placement_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("cannot remove {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())),
    }
}

// ---- adopt ----------------------------------------------------------------

/// What one adopt is asked for.
#[derive(Debug, Clone)]
pub struct AdoptRequest<'a> {
    /// What the placement serves.
    pub kind: CacheKind,
    /// The registered volume the data belongs on.
    pub volume_uuid: &'a VolumeUuid,
    /// The form the caller asks for; `None` lets the kind and the current state
    /// decide.
    pub form: Option<FormRequest>,
    /// Write the tool's own setting when it does not already name the target.
    /// Without it a setting that must change is refused with a plan.
    pub write_setting: bool,
    /// Write a marker into a directory that already exists on the volume. It is
    /// the in-place adoption of today's hand-made placements, and it moves
    /// nothing.
    pub adopt_existing_root: bool,
    /// Inspect and report the effects without making any.
    pub plan: bool,
}

/// One adopt's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptReport {
    /// Whether this was a preview.
    pub plan: bool,
    /// The record that was written, or `None` in a preview.
    pub placement: Option<PlacementRecord>,
    /// What was done, or what a preview would do, in order.
    pub changes: Vec<String>,
    /// What a caller should know that is not a change.
    pub notes: Vec<String>,
}

/// The state one adopt found at the original path.
enum Inspection {
    /// Nothing is there.
    Absent,
    /// A symbolic link with its recorded text.
    Symlink(PathBuf),
    /// A real directory's facts.
    Directory(EntryFacts),
    /// A regular file's facts.
    File(EntryFacts),
    /// Anything else.
    Other(EntryFacts),
}

/// Durable, bounded recovery authority for one cache adoption. It is separate
/// from deletion intents: replay never interprets a cache change as a delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdoptIntent {
    schema: String,
    id: String,
    kind: CacheKind,
    volume: VolumeRecord,
    original: PathBuf,
    before: OriginalState,
    expected_setting: Option<String>,
    #[serde(default)]
    setting_after: Option<SettingsSnapshot>,
    target: Option<PathBuf>,
    marker_before: Option<SettingsSnapshot>,
    marker_plan: Option<MarkerPlan>,
    link: Option<LinkForm>,
    held: Option<HeldRoot>,
    candidate: Option<PlacementRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerPlan {
    path: PathBuf,
    root_id: String,
    observed: PreAdoption,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OriginalState {
    Absent,
    File {
        snapshot: SettingsSnapshot,
    },
    Directory {
        device: u64,
        inode: u64,
        mode: u32,
        entries: u64,
    },
    Link {
        device: u64,
        inode: u64,
        target: PathBuf,
    },
}

fn original_state(dirs: &dyn DirOps, path: &Path) -> Result<OriginalState> {
    match inspect(dirs, path)? {
        Inspection::Absent => Ok(OriginalState::Absent),
        Inspection::File(_) => {
            let (facts, bytes) =
                read_settings(dirs, path)?.ok_or_else(|| unknown("settings vanished"))?;
            Ok(OriginalState::File {
                snapshot: snapshot_of(&facts, &bytes)?,
            })
        }
        Inspection::Directory(facts) => Ok(OriginalState::Directory {
            device: facts.device.get(),
            inode: facts.inode,
            mode: facts.permission_mode(),
            entries: bounded_entries(dirs, path)?.0,
        }),
        Inspection::Symlink(target) => {
            let (device, inode) = entry_identity(dirs, path)?;
            Ok(OriginalState::Link {
                device,
                inode,
                target,
            })
        }
        Inspection::Other(_) => Err(unknown("unsupported original entry")),
    }
}

fn adopt_intent_path(state: &StatePaths, kind: CacheKind) -> PathBuf {
    state
        .root()
        .join(format!("cache-adopt-{}.json", kind.as_str()))
}

fn read_adopt_intent(state: &StatePaths, kind: CacheKind) -> Result<Option<AdoptIntent>> {
    let path = adopt_intent_path(state, kind);
    match state::read_bounded(
        &path,
        &Bounds::new(PLACEMENT_CAP_BYTES, 1, Deadline::after(READ_DEADLINE)),
    ) {
        ReadOutcome::Absent => Ok(None),
        ReadOutcome::Refused(error) => Err(error),
        ReadOutcome::Present(bytes) => {
            let intent: AdoptIntent = serde_json::from_slice(&bytes)
                .map_err(|e| unknown(format!("invalid cache adopt intent: {e}")))?;
            if intent.schema != "barge.cache-adopt/1"
                || intent.kind != kind
                || !intent.original.is_absolute()
            {
                return Err(unknown("cache adopt intent identity is invalid"));
            }
            state::validate_identifier("intent_id", &intent.id)?;
            if let Some(candidate) = &intent.candidate {
                candidate.validate()?;
            }
            Ok(Some(intent))
        }
    }
}

fn write_adopt_intent(state: &StatePaths, intent: &AdoptIntent) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(intent).map_err(|e| unknown(e.to_string()))?;
    if bytes.len() > PLACEMENT_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "cache adoption intent exceeds its cap",
        ));
    }
    state::publish_atomic(&adopt_intent_path(state, intent.kind), &bytes)
}

/// Atomically claim a kind without replacing another volume's pending adopt.
/// The private staged file is synced first; hard-link publication is exclusive.
fn begin_adopt_intent(state: &StatePaths, intent: &AdoptIntent) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(intent).map_err(|e| unknown(e.to_string()))?;
    if bytes.len() > PLACEMENT_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            "cache adoption intent exceeds its cap",
        ));
    }
    let staged = state
        .root()
        .join(format!(".cache-adopt-{}.pending", intent.id));
    state::publish_atomic(&staged, &bytes)?;
    let publication = std::fs::hard_link(&staged, adopt_intent_path(state, intent.kind));
    let _ = std::fs::remove_file(&staged);
    publication.map_err(|e| {
        Error::new(
            ErrorKind::Locked,
            format!("cache adoption intent was already claimed or cannot be published: {e}"),
        )
    })?;
    std::fs::File::open(state.root())
        .and_then(|dir| dir.sync_all())
        .map_err(|e| unknown(format!("cannot sync adoption claim: {e}")))
}

fn clear_adopt_intent(state: &StatePaths, kind: CacheKind) -> Result<()> {
    let path = adopt_intent_path(state, kind);
    std::fs::remove_file(&path)
        .map_err(|e| unknown(format!("cannot settle cache adoption intent: {e}")))?;
    std::fs::File::open(state.root())
        .and_then(|dir| dir.sync_all())
        .map_err(|e| unknown(format!("cannot sync intent settlement: {e}")))
}

fn marker_snapshot(dirs: &dyn DirOps, target: Option<&Path>) -> Result<Option<SettingsSnapshot>> {
    let Some(target) = target else {
        return Ok(None);
    };
    if dirs.path_facts(target)?.is_none() {
        return Ok(None);
    }
    read_settings(dirs, &target.join(area::ROOT_MARKER_FILE))?
        .map(|(facts, bytes)| snapshot_of(&facts, &bytes))
        .transpose()
}

fn adoption_restored(dirs: &dyn DirOps, intent: &AdoptIntent) -> Result<bool> {
    let current = original_state(dirs, &intent.original)?;
    let original_matches = current == intent.before
        || matches!((&intent.before, &current), (OriginalState::File { snapshot: a }, OriginalState::File { snapshot: b }) if a.bytes == b.bytes && a.mode == b.mode)
        || matches!((&intent.before, &current),
        (OriginalState::Directory { mode: a, entries: 0, .. }, OriginalState::Directory { mode: b, entries: 0, .. }) if a == b);
    Ok(
        original_matches
            && marker_snapshot(dirs, intent.target.as_deref())? == intent.marker_before,
    )
}

#[allow(clippy::too_many_arguments)]
fn recover_marker_observation(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    intent: &mut AdoptIntent,
) -> Result<()> {
    if intent.held.is_some() {
        return Ok(());
    }
    let Some(plan) = &intent.marker_plan else {
        return Ok(());
    };
    let folder = re_resolve(dirs, facts, mounts, observer, &intent.volume)?;
    let handle = prove_cache_path(dirs, &folder, &plan.path)?
        .ok_or_else(|| unknown("pending marker directory is absent"))?;
    if handle.identity().device.get() != plan.observed.device
        || handle.identity().inode != plan.observed.inode
    {
        return Err(unknown("pending marker directory identity changed"));
    }
    let expectation = MarkerExpectation {
        volume_uuid: &intent.volume.volume_uuid,
        resolved_path: &plan.path,
        effective_uid: observer.effective_uid()?,
        now_ms: state::now_ms(),
    };
    match area::read_marker_with_digest(dirs, &handle, &expectation) {
        (MarkerState::Absent, _) => Ok(()),
        (MarkerState::Owned(marker), Some(digest)) if marker.root_id == plan.root_id => {
            intent.held = Some(HeldRoot {
                path: plan.path.clone(),
                root_id: marker.root_id,
                nonce: marker.nonce,
                device: plan.observed.device,
                inode: plan.observed.inode,
                marker_digest: digest,
                observed: plan.observed.clone(),
            });
            write_adopt_intent(state, intent)
        }
        _ => Err(unknown(
            "pending marker is not the planned publication; keep it and the intent",
        )),
    }
}

fn pending_record(dirs: &dyn DirOps, intent: &AdoptIntent) -> Result<PlacementRecord> {
    if let Some(candidate) = &intent.candidate {
        return Ok(candidate.clone());
    }
    let (path, form) = if let Some(link) = &intent.link {
        if link.published_identity.is_none() {
            return Err(unknown("link publication identity was not durably observed; keep intent and publication for operator recovery"));
        }
        (link.target.clone(), PlacementForm::Link(link.clone()))
    } else if let Some(key) = intent.kind.setting_key() {
        let (facts, bytes) = read_settings(dirs, &intent.original)?
            .ok_or_else(|| unknown("pending setting is absent"))?;
        let expected = intent.setting_after.as_ref().ok_or_else(|| unknown("pending setting has no observed publication identity; preserve intent for inspection"))?;
        if snapshot_of(&facts, &bytes)? != *expected {
            return Err(unknown(
                "pending setting diverged; recovery changes nothing",
            ));
        }
        let before = match &intent.before {
            OriginalState::File { snapshot } => Some(snapshot.clone()),
            OriginalState::Absent => None,
            _ => return Err(unknown("invalid setting intent")),
        };
        let path = intent
            .target
            .clone()
            .ok_or_else(|| unknown("pending setting target missing"))?;
        (
            path.clone(),
            PlacementForm::Setting(Box::new(SettingForm {
                settings_path: intent.original.clone(),
                key,
                changed: before
                    .as_ref()
                    .is_none_or(|before| before.bytes != hex_encode(&bytes)),
                before,
                after: snapshot_of(&facts, &bytes)?,
                backup_path: None,
                data_path: path,
                created_directory: false,
                held_root: intent.held.clone(),
            })),
        )
    } else if let Some(held) = &intent.held {
        let publication = match &intent.before {
            OriginalState::Link { target, .. } => Some(HeldRootLink {
                link_path: intent.original.clone(),
                target: target.clone(),
            }),
            _ => None,
        };
        (
            held.path.clone(),
            PlacementForm::HeldRoot(HeldRootForm {
                path: held.path.clone(),
                publication,
                held_root: held.clone(),
            }),
        )
    } else {
        return Err(unknown("adopt effects lack a complete observed identity; intent and before state are retained for operator recovery"));
    };
    let record = PlacementRecord {
        schema: PLACEMENT_SCHEMA.to_owned(),
        placement_id: intent.id.clone(),
        kind: intent.kind,
        original_path: intent.original.clone(),
        volume_uuid: intent.volume.volume_uuid.clone(),
        root_dir: intent.volume.root_dir.clone(),
        folder: intent
            .volume
            .folders
            .caches
            .clone()
            .unwrap_or_else(|| "caches".to_owned()),
        recorded_path: path,
        adopted_ms: state::now_ms(),
        generation: 1,
        form,
    };
    record.validate()?;
    Ok(record)
}

/// The readonly preview runs first. The volume lock and synced recovery intent
/// precede every cache effect, including folder creation and marker publication.
#[allow(clippy::too_many_arguments)]
pub fn adopt(
    state: &StatePaths,
    account: &dyn AccountFacts,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &AdoptRequest<'_>,
) -> Result<AdoptReport> {
    undo_capture::require_no_pending(state, request.kind)?;
    if read_adopt_intent(state, request.kind)?.is_some() {
        return Err(unknown(
            "cache adoption is pending; use cache undo --kind to recover before another adopt",
        ));
    }
    let mut preview = request.clone();
    preview.plan = true;
    let plan = adopt_impl(state, account, dirs, facts, mounts, observer, &preview)?;
    if request.plan {
        return Ok(plan);
    }
    state.initialize()?;
    let mut locks = LockSet::new(state.clone());
    let _volume_lock = locks.lock_volume(request.volume_uuid)?;
    undo_capture::require_no_pending(state, request.kind)?;
    if read_adopt_intent(state, request.kind)?.is_some() {
        return Err(unknown("another cache adoption is pending"));
    }
    let volume = area::registered_volume(state, request.volume_uuid)?;
    let folder = re_resolve(dirs, facts, mounts, observer, &volume)?;
    let original = account
        .account_home()?
        .join(request.kind.original_relative_path());
    let before = original_state(dirs, &original)?;
    let (target, expected_setting) = if let Some(key) = request.kind.setting_key() {
        let found = read_settings(dirs, &original)?;
        let current = match &found {
            Some((facts, bytes)) => setting_value(&key, bytes, facts)?,
            None => None,
        };
        let target = current
            .filter(|value| is_under(value, &folder.path))
            .map_or_else(
                || folder.path.join(request.kind.folder_name()),
                PathBuf::from,
            );
        let bytes = write_setting_value(
            &key,
            found
                .as_ref()
                .map_or(b"".as_slice(), |(_, bytes)| bytes.as_slice()),
            &target,
        )?;
        (Some(target), Some(hex_encode(&bytes)))
    } else {
        (
            match &before {
                OriginalState::Link { target, .. } => Some(target.clone()),
                OriginalState::Directory { .. } if is_under_path(&original, &folder.path) => {
                    Some(original.clone())
                }
                _ => None,
            },
            None,
        )
    };
    let marker_before = marker_snapshot(dirs, target.as_deref())?;
    let intent = AdoptIntent {
        schema: "barge.cache-adopt/1".to_owned(),
        id: hex_encode(&dirs.random_bytes(16)?),
        kind: request.kind,
        volume,
        original,
        before,
        expected_setting,
        setting_after: None,
        target,
        marker_before,
        marker_plan: None,
        link: None,
        held: None,
        candidate: None,
    };
    begin_adopt_intent(state, &intent)?;
    let result = adopt_impl(state, account, dirs, facts, mounts, observer, request);
    match result {
        Ok(report) => {
            clear_adopt_intent(state, request.kind)?;
            Ok(report)
        }
        Err(error) => {
            if let Some(pending) = read_adopt_intent(state, request.kind)? {
                if original_state(dirs, &pending.original)? == pending.before {
                    if let Some(held) = &pending.held {
                        if let Ok(uid) = observer.effective_uid() {
                            let form = HeldRootForm {
                                path: held.path.clone(),
                                publication: None,
                                held_root: held.clone(),
                            };
                            let _ = rollback_publication(
                                state,
                                dirs,
                                pending.kind,
                                uid,
                                &PlacementForm::HeldRoot(form),
                            );
                        }
                    }
                }
                if adoption_restored(dirs, &pending)? && pending.candidate.is_none() {
                    clear_adopt_intent(state, request.kind)?;
                    return Err(error);
                }
            }
            Err(error
                .with_detail(
                    "pending_adopt_intent",
                    adopt_intent_path(state, request.kind)
                        .display()
                        .to_string()
                        .into(),
                )
                .with_detail(
                    "recovery",
                    "cache undo --kind; divergence is kept and refused".into(),
                ))
        }
    }
}

/// Adopt one cache kind onto one registered volume, mutating nothing a refusal
/// or a preview cannot prove.
///
/// Refusals happen before any effect: the registry is read, the volume must be
/// registered, present and of declared durability, and the placement must not
/// already be adopted. A non-empty real directory, a setting that names another
/// path and a link into an unregistered place are refused with a plan and
/// nothing is renamed, linked, written or deleted.
#[allow(clippy::too_many_arguments)]
fn adopt_impl(
    state: &StatePaths,
    account: &dyn AccountFacts,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &AdoptRequest<'_>,
) -> Result<AdoptReport> {
    let existing = read_placements(state)?;
    if let Some(record) = existing.find_kind(request.kind) {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is already adopted as placement {}; undo it first",
                request.kind.as_str(),
                record.placement_id
            ),
        )
        .with_detail("placement_id", record.placement_id.clone().into()));
    }
    let record = area::registered_volume(state, request.volume_uuid)?;
    record.declared_durability()?;
    let home = account.account_home()?;
    let original = home.join(request.kind.original_relative_path());
    let folder = area::resolve_feature(dirs, facts, mounts, observer, &record, Feature::Caches)
        .map_err(|error| absent_volume(error, &record))?;
    match request.kind.setting_key() {
        Some(key) => {
            if matches!(
                request.form,
                Some(FormRequest::HeldRoot | FormRequest::Link)
            ) {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!(
                        "{} can only be redirected by its own setting, so its form is `setting`",
                        request.kind.as_str()
                    ),
                ));
            }
            adopt_setting(
                state, dirs, facts, mounts, observer, request, &record, &folder, &original, &key,
            )
        }
        None => {
            if matches!(request.form, Some(FormRequest::Setting)) {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!(
                        "{} is published by a link or adopted in place, not by a setting",
                        request.kind.as_str()
                    ),
                ));
            }
            adopt_directory(
                state, dirs, facts, mounts, observer, request, &record, &folder, &original,
            )
        }
    }
}

/// Inspect the original path without following a link.
fn inspect(dirs: &dyn DirOps, original: &Path) -> Result<Inspection> {
    let Some(facts) = dirs.path_facts(original)? else {
        return Ok(Inspection::Absent);
    };
    match facts.kind {
        EntryKind::Symlink => match dirs.read_link(original)? {
            Some(target) => Ok(Inspection::Symlink(target)),
            None => Ok(Inspection::Absent),
        },
        EntryKind::Directory => Ok(Inspection::Directory(facts)),
        EntryKind::File => Ok(Inspection::File(facts)),
        EntryKind::Other => Ok(Inspection::Other(facts)),
    }
}

fn plan_error(message: String, plan: Vec<String>) -> Error {
    Error::new(ErrorKind::RefusedBoundary, message)
        .with_detail("plan", plan.into())
        .with_detail("changed", false.into())
}

/// The refusal for a non-empty real directory: nothing is renamed, linked,
/// copied or deleted, and the plan names the two real options.
fn non_empty_directory_error(
    dirs: &dyn DirOps,
    original: &Path,
    facts: &EntryFacts,
    kind: CacheKind,
    on_volume: bool,
) -> Error {
    let (entries, names) = bounded_entries(dirs, original).unwrap_or((0, Vec::new()));
    let mut plan = vec![
        format!(
            "adopt the directory in place as a `held-root` by writing its marker (nothing is moved)"
        ),
        "or relocate its contents with the separate, operator-authorized relocation command"
            .to_owned(),
    ];
    if on_volume {
        plan.push(
            "the directory is on the registered volume, so `--form held-root --adopt-existing-root` adopts it where it is"
                .to_owned(),
        );
    } else {
        plan.push(
            "the directory is not on the registered volume, so relocate it first: adopt never moves data"
                .to_owned(),
        );
    }
    Error::new(
        ErrorKind::RefusedBoundary,
        format!(
            "{} is a non-empty real directory with {entries} entries; a link cannot be renamed over it and adopt never moves its contents",
            original.display()
        ),
    )
    .with_detail("path", original.display().to_string().into())
    .with_detail("kind", kind.as_str().into())
    .with_detail("entries", entries.into())
    .with_detail("observed_entries", names.into())
    .with_detail("device", facts.device.to_string().into())
    .with_detail("changed", false.into())
    .with_detail("plan", plan.into())
}

/// The entry count and at most [`PLAN_ENTRY_NAMES`] names of one directory.
fn bounded_entries(dirs: &dyn DirOps, path: &Path) -> Result<(u64, Vec<String>)> {
    let handle = dirs.open_path(path)?;
    let listing = dirs.list_children(&handle, PLAN_ENTRY_NAMES)?;
    let names = listing
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    Ok((u64::try_from(listing.total).unwrap_or(u64::MAX), names))
}

/// Adopt a `setting` kind: record the setting, and change it only when the
/// explicit adopt asks for that.
#[allow(clippy::too_many_arguments)]
fn adopt_setting(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &AdoptRequest<'_>,
    record: &VolumeRecord,
    folder: &FeatureFolder,
    original: &Path,
    key: &SettingKey,
) -> Result<AdoptReport> {
    let effective_uid = observer.effective_uid()?;
    let found = read_settings(dirs, original)?;
    let current = match &found {
        Some((facts, bytes)) => setting_value(key, bytes, facts)?,
        None => None,
    };
    let intended = folder.path.join(request.kind.folder_name());
    let already_names_target = current
        .as_deref()
        .is_some_and(|value| is_under(value, &folder.path));
    if !already_names_target && !request.write_setting {
        return Err(match current.as_deref() {
            Some(value) => plan_error(
                format!(
                    "{} sets `{}` to {value}, not to the cache's folder on the registered volume",
                    original.display(),
                    key.key
                ),
                vec![
                    format!(
                        "record the setting as it is: {} -> {value}",
                        original.display()
                    ),
                    format!(
                        "or write the setting to {} with `--write-setting` (the exact change is reported and undo restores the file)",
                        intended.display()
                    ),
                ],
            )
            .with_detail("current_value", value.into())
            .with_detail("intended_value", intended.display().to_string().into())
            .with_detail("settings_path", original.display().to_string().into()),
            None => plan_error(
                format!(
                    "{} does not set `{}`, so adopt would have to write the tool's setting",
                    original.display(),
                    key.key
                ),
                vec![format!(
                    "add `{}` to {} pointing at {} with `--write-setting` (the exact change is reported and undo restores the file)",
                    key.key,
                    original.display(),
                    intended.display()
                )],
            )
            .with_detail("settings_path", original.display().to_string().into()),
        });
    }
    let target = match current.as_deref() {
        Some(value) if already_names_target => PathBuf::from(value),
        _ => intended,
    };

    prove_cache_path(dirs, folder, &target)?;

    // A preview reports the intended effects and changes nothing.
    let mut changes = Vec::new();
    let mut notes = Vec::new();
    let changed = current.as_deref() != Some(target.to_string_lossy().as_ref());
    if changed {
        changes.push(format!(
            "write `{}` in {}: {} -> {}",
            key.key,
            original.display(),
            current.as_deref().unwrap_or("(not set)"),
            target.display()
        ));
    } else {
        notes.push(format!(
            "{} already sets `{}` to {}; adopt changes no byte of it",
            original.display(),
            key.key,
            target.display()
        ));
    }
    if dirs.path_facts(&target)?.is_none() {
        changes.push(format!(
            "create the directory {} as a plain cache directory",
            target.display()
        ));
    }
    if request.adopt_existing_root {
        if dirs.path_facts(&target)?.is_some() {
            changes.push(format!(
                "write `.barge-root.json` into {} with `origin: adopted_existing` (nothing is moved)",
                target.display()
            ));
        } else {
            changes.push(format!(
                "create {} and write its `.barge-root.json` with `origin: adopted_existing`",
                target.display()
            ));
        }
    }
    if request.plan {
        return Ok(AdoptReport {
            plan: true,
            placement: None,
            changes,
            notes,
        });
    }

    // Every effect from here is authorized: the state layout and the volume lock
    // are taken before anything on the volume or in the settings file is
    // touched, and the folder is re-resolved under that lock.
    let folder = re_resolve(dirs, facts, mounts, observer, record)?;
    let created_directory = create_cache_directory(dirs, observer, record, Some(&folder), &target)?;
    let folder = re_resolve(dirs, facts, mounts, observer, record)?;
    let mut data_held_root = None;
    if request.adopt_existing_root && dirs.path_facts(&target)?.is_some() {
        data_held_root = Some(adopt_in_place(
            state,
            request.kind,
            dirs,
            record,
            &folder,
            &target,
            effective_uid,
        )?);
    }
    let after_bytes = match &found {
        Some((_, bytes)) => write_setting_value(key, bytes, &target)?,
        None => write_setting_value(key, b"", &target)?,
    };
    let file_parent = parent_of(original)?;
    let file_name = file_name_of(original)?;
    let before = match &found {
        Some((facts, bytes)) => Some(snapshot_of(facts, bytes)?),
        None => None,
    };
    let backup_name = format!(".{file_name}{SETTING_BACKUP_SUFFIX}");
    let backup_path = found.as_ref().map(|_| file_parent.join(&backup_name));
    let published = if changed {
        let mut captured = undo_capture::Captures::for_adopt(
            state,
            dirs,
            request.kind,
            effective_uid,
            "adopt-setting",
        )?;
        let setting_index = match &before {
            Some(snapshot) => captured.file(original, snapshot.clone())?,
            None => captured.absent_file(original)?,
        };
        let backup_index = if backup_path.is_some() {
            Some(captured.absent_file(&file_parent.join(&backup_name))?)
        } else {
            None
        };
        captured.capture_all()?;
        if let (Some(index), Some((backup_facts, bytes))) = (backup_index, &found) {
            captured.restore_file(index, bytes, backup_facts.mode & 0o777)?;
        }
        let mode = found
            .as_ref()
            .map_or(0o600, |(facts, _)| facts.mode & 0o777);
        let snapshot = captured.restore_file(setting_index, &after_bytes, mode)?;
        let mut intent = read_adopt_intent(state, request.kind)?
            .ok_or_else(|| unknown("adopt intent missing after settings publication"))?;
        intent.setting_after = Some(snapshot.clone());
        intent.expected_setting = None;
        write_adopt_intent(state, &intent)?;
        if before.is_some() {
            captured.remove(setting_index)?;
        }
        captured.settle()?;
        Some(snapshot)
    } else {
        None
    };
    // Read the file back: the record holds what is on disk, not what was meant
    // to be written.
    let (after_facts, after_written) = read_settings(dirs, original)?.ok_or_else(|| {
        unknown(format!(
            "{} disappeared while its setting was adopted",
            original.display()
        ))
    })?;
    let after_snapshot = snapshot_of(&after_facts, &after_written)?;
    if published
        .as_ref()
        .is_some_and(|expected| *expected != after_snapshot)
        || (changed && after_written != after_bytes)
    {
        return Err(unknown(format!(
            "{} does not hold the bytes that were just written",
            original.display()
        )));
    }
    let after = snapshot_of(&after_facts, &after_written)?;

    let form = PlacementForm::Setting(Box::new(SettingForm {
        settings_path: original.to_path_buf(),
        key: key.clone(),
        changed,
        before,
        after,
        backup_path: if changed { backup_path } else { None },
        data_path: target.clone(),
        created_directory,
        held_root: data_held_root,
    }));
    let finished = finish_record(
        state,
        dirs,
        request.kind,
        original,
        record,
        &folder,
        target,
        form.clone(),
    );
    let record = match finished {
        Ok(record) => record,
        Err(error) => {
            let rolled_back = rollback_publication(state, dirs, request.kind, effective_uid, &form);
            return Err(error.with_detail("rolled_back", rolled_back.into()));
        }
    };
    Ok(AdoptReport {
        plan: false,
        placement: Some(record),
        changes,
        notes,
    })
}

/// Adopt a directory kind (`link` or `held-root`).
#[allow(clippy::too_many_arguments)]
fn adopt_directory(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &AdoptRequest<'_>,
    record: &VolumeRecord,
    folder: &FeatureFolder,
    original: &Path,
) -> Result<AdoptReport> {
    let effective_uid = observer.effective_uid()?;
    let inspection = inspect(dirs, original)?;
    match inspection {
        Inspection::Absent => {
            if matches!(request.form, Some(FormRequest::HeldRoot)) {
                return Err(plan_error(
                    format!(
                        "{} is absent, so there is no directory to adopt in place",
                        original.display()
                    ),
                    vec![
                        "record a `link` placement: an owned root under the volume's caches/ folder, published through a temporary name"
                            .to_owned(),
                    ],
                ));
            }
            let changes = vec![
                format!(
                    "create an owned root under {} with a `.barge-root.json` marker",
                    folder.path.display()
                ),
                format!(
                    "publish a symbolic link at {} through a temporary name, pointing at the new root",
                    original.display()
                ),
            ];
            if request.plan {
                return Ok(AdoptReport {
                    plan: true,
                    placement: None,
                    changes,
                    notes: Vec::new(),
                });
            }
            let folder = re_resolve(dirs, facts, mounts, observer, record)?;
            let root = area::create_root_in_feature(
                dirs,
                observer,
                &folder,
                RootKind::CacheRoot,
                cache_area(),
            )?;
            let mut link_form = LinkForm {
                link_path: original.to_path_buf(),
                target: root.path.clone(),
                restore: LinkRestore::Absent,
                original_mode: None,
                published_identity: None,
                root_id: root.root_id,
                marker_digest: root.marker_digest,
            };
            let mut intent = read_adopt_intent(state, request.kind)?
                .ok_or_else(|| unknown("adopt intent missing"))?;
            intent.link = Some(link_form.clone());
            write_adopt_intent(state, &intent)?;
            link_form.published_identity = Some(publish_link(
                state,
                request.kind,
                dirs,
                original,
                &root.path,
            )?);
            let form = PlacementForm::Link(link_form.clone());
            let record = match finish_record(
                state,
                dirs,
                request.kind,
                original,
                record,
                &folder,
                root.path,
                form,
            ) {
                Ok(record) => record,
                Err(error) => {
                    let rolled_back = rollback_publication(
                        state,
                        dirs,
                        request.kind,
                        effective_uid,
                        &PlacementForm::Link(link_form.clone()),
                    );
                    return Err(error.with_detail("rolled_back", rolled_back.into()));
                }
            };
            Ok(AdoptReport {
                plan: false,
                placement: Some(record),
                changes,
                notes: Vec::new(),
            })
        }
        Inspection::Symlink(target) => {
            if matches!(request.form, Some(FormRequest::Link)) {
                return Err(plan_error(
                    format!(
                        "{} is already a symbolic link to {}; adopt never publishes a second link over one",
                        original.display(),
                        target.display()
                    ),
                    vec![format!(
                        "record the existing link and, with `--adopt-existing-root`, write a marker into its target (nothing is moved)"
                    )],
                ));
            }
            if !target.is_absolute() || !is_under_path(&target, &folder.path) {
                return Err(plan_error(
                    format!(
                        "{} is a link to {}, which is not inside the registered volume's caches folder {}",
                        original.display(),
                        target.display(),
                        folder.path.display()
                    ),
                    vec![
                        format!(
                            "point the link at {} yourself, then adopt it (adopt never swaps a link)",
                            folder.path.display()
                        ),
                    ],
                )
                .with_detail("current_value", target.display().to_string().into())
                .with_detail("intended_value", folder.path.display().to_string().into())
                .with_detail("link_path", original.display().to_string().into()));
            }
            prove_cache_path(dirs, folder, &target)?;
            let target_facts = dirs.path_facts(&target)?;
            let Some(target_facts) = target_facts else {
                return Err(unknown(format!(
                    "{} is a link to {}, which does not exist",
                    original.display(),
                    target.display()
                ))
                .with_detail("link_path", original.display().to_string().into())
                .with_detail("target", target.display().to_string().into()));
            };
            if target_facts.kind != EntryKind::Directory {
                return Err(plan_error(
                    format!(
                        "{} is a link to {}, which is a {} and not a cache directory",
                        original.display(),
                        target.display(),
                        target_facts.kind.as_str()
                    ),
                    vec!["a cache placement's target is a directory".to_owned()],
                ));
            }
            if !request.adopt_existing_root {
                return Err(plan_error(
                    format!(
                        "{} is already a link to {}, and its target carries no Barge marker",
                        original.display(),
                        target.display()
                    ),
                    vec![
                        format!(
                            "record the existing link and write a marker into {} with `--adopt-existing-root`, which moves nothing",
                            target.display()
                        ),
                    ],
                )
                .with_detail("link_path", original.display().to_string().into())
                .with_detail("target", target.display().to_string().into())
                .with_detail("changed", false.into()));
            }
            let changes = vec![
                format!(
                    "record the existing link {} -> {}; adopt never swaps it",
                    original.display(),
                    target.display()
                ),
                format!(
                    "write `.barge-root.json` into {} with `origin: adopted_existing` (nothing is moved)",
                    target.display()
                ),
            ];
            if request.plan {
                return Ok(AdoptReport {
                    plan: true,
                    placement: None,
                    changes,
                    notes: Vec::new(),
                });
            }
            let folder = re_resolve(dirs, facts, mounts, observer, record)?;
            let held = adopt_in_place(
                state,
                request.kind,
                dirs,
                record,
                &folder,
                &target,
                effective_uid,
            )?;
            let held_form = HeldRootForm {
                path: target.clone(),
                publication: Some(HeldRootLink {
                    link_path: original.to_path_buf(),
                    target: target.clone(),
                }),
                held_root: held,
            };
            let form = PlacementForm::HeldRoot(held_form.clone());
            let record = match finish_record(
                state,
                dirs,
                request.kind,
                original,
                record,
                &folder,
                target,
                form,
            ) {
                Ok(record) => record,
                Err(error) => {
                    let rolled_back = rollback_publication(
                        state,
                        dirs,
                        request.kind,
                        effective_uid,
                        &PlacementForm::HeldRoot(held_form.clone()),
                    );
                    return Err(error.with_detail("rolled_back", rolled_back.into()));
                }
            };
            Ok(AdoptReport {
                plan: false,
                placement: Some(record),
                changes,
                notes: Vec::new(),
            })
        }
        Inspection::Directory(dir_facts) => {
            let on_volume = on_registered_volume(dirs, record, original)?;
            let (entries, names) = bounded_entries(dirs, original)?;
            if entries > 0 {
                if on_volume && request.adopt_existing_root && is_under_path(original, &folder.path)
                {
                    let changes = vec![format!(
                        "write `.barge-root.json` into {} with `origin: adopted_existing` (nothing is moved)",
                        original.display()
                    )];
                    if request.plan {
                        return Ok(AdoptReport {
                            plan: true,
                            placement: None,
                            changes,
                            notes: Vec::new(),
                        });
                    }
                    let folder = re_resolve(dirs, facts, mounts, observer, record)?;
                    let held = adopt_in_place(
                        state,
                        request.kind,
                        dirs,
                        record,
                        &folder,
                        original,
                        effective_uid,
                    )?;
                    let held_form = HeldRootForm {
                        path: original.to_path_buf(),
                        publication: None,
                        held_root: held,
                    };
                    let form = PlacementForm::HeldRoot(held_form.clone());
                    let record = match finish_record(
                        state,
                        dirs,
                        request.kind,
                        original,
                        record,
                        &folder,
                        original.to_path_buf(),
                        form,
                    ) {
                        Ok(record) => record,
                        Err(error) => {
                            let rolled_back = rollback_publication(
                                state,
                                dirs,
                                request.kind,
                                effective_uid,
                                &PlacementForm::HeldRoot(held_form.clone()),
                            );
                            return Err(error.with_detail("rolled_back", rolled_back.into()));
                        }
                    };
                    return Ok(AdoptReport {
                        plan: false,
                        placement: Some(record),
                        changes,
                        notes: vec![format!(
                            "the directory holds {entries} entries, observed but never touched: {:?}",
                            names
                        )],
                    });
                }
                return Err(non_empty_directory_error(
                    dirs,
                    original,
                    &dir_facts,
                    request.kind,
                    on_volume,
                ));
            }
            // An empty real directory: on the volume it is adopted in place, off
            // it a link may replace it after an empty-directory proof.
            if on_volume {
                prove_cache_path(dirs, folder, original)?;
                if !request.adopt_existing_root {
                    return Err(plan_error(
                        format!(
                            "{} is an empty directory on the registered volume",
                            original.display()
                        ),
                        vec![format!(
                            "adopt it in place with `--adopt-existing-root`: its marker is written and nothing is moved"
                        )],
                    ));
                }
                let changes = vec![format!(
                    "write `.barge-root.json` into {} with `origin: adopted_existing` (nothing is moved)",
                    original.display()
                )];
                if request.plan {
                    return Ok(AdoptReport {
                        plan: true,
                        placement: None,
                        changes,
                        notes: Vec::new(),
                    });
                }
                let folder = re_resolve(dirs, facts, mounts, observer, record)?;
                let held = adopt_in_place(
                    state,
                    request.kind,
                    dirs,
                    record,
                    &folder,
                    original,
                    effective_uid,
                )?;
                let held_form = HeldRootForm {
                    path: original.to_path_buf(),
                    publication: None,
                    held_root: held,
                };
                let form = PlacementForm::HeldRoot(held_form.clone());
                let record = match finish_record(
                    state,
                    dirs,
                    request.kind,
                    original,
                    record,
                    &folder,
                    original.to_path_buf(),
                    form,
                ) {
                    Ok(record) => record,
                    Err(error) => {
                        let rolled_back = rollback_publication(
                            state,
                            dirs,
                            request.kind,
                            effective_uid,
                            &PlacementForm::HeldRoot(held_form.clone()),
                        );
                        return Err(error.with_detail("rolled_back", rolled_back.into()));
                    }
                };
                return Ok(AdoptReport {
                    plan: false,
                    placement: Some(record),
                    changes,
                    notes: Vec::new(),
                });
            }
            if matches!(request.form, Some(FormRequest::HeldRoot)) {
                return Err(plan_error(
                    format!(
                        "{} is an empty directory on the internal drive, so it cannot be adopted in place on the volume",
                        original.display()
                    ),
                    vec![
                        "record a `link` placement: the empty directory is removed after an empty-directory custody proof and the link is published in its place"
                            .to_owned(),
                    ],
                ));
            }
            let changes = vec![
                format!(
                    "take custody of the empty directory {}, then remove it",
                    original.display()
                ),
                format!(
                    "create an owned root under {} with a `.barge-root.json` marker",
                    folder.path.display()
                ),
                format!(
                    "publish a symbolic link at {} through a temporary name",
                    original.display()
                ),
                "undo removes the link and restores the empty directory".to_owned(),
            ];
            if request.plan {
                return Ok(AdoptReport {
                    plan: true,
                    placement: None,
                    changes,
                    notes: Vec::new(),
                });
            }
            let folder = re_resolve(dirs, facts, mounts, observer, record)?;
            // The empty directory is proved and removed before anything is
            // created on the volume, so a refusal here changes nothing at all.
            let (parent, name) = open_parent(dirs, original)?;
            let mode = dir_facts.permission_mode();
            confirm_empty_directory(dirs, observer, original, &dir_facts)?;
            match dirs.remove_child(&parent, &name, RemoveKind::Directory)? {
                crate::os::ChildRemoval::Removed => {}
                crate::os::ChildRemoval::NotEmpty => {
                    return Err(Error::new(
                        ErrorKind::RefusedBoundary,
                        format!(
                            "{} became non-empty while it was adopted; nothing was replaced",
                            original.display()
                        ),
                    )
                    .with_detail("path", original.display().to_string().into()))
                }
                crate::os::ChildRemoval::Absent => {
                    return Err(unknown(format!(
                        "{} disappeared while it was adopted",
                        original.display()
                    )))
                }
            }
            // From here the path is briefly absent by design: every failure puts
            // the empty directory back, so a failed adopt never leaves the tool's
            // path missing.
            let root = match area::create_root_in_feature(
                dirs,
                observer,
                &folder,
                RootKind::CacheRoot,
                cache_area(),
            ) {
                Ok(root) => root,
                Err(error) => {
                    let _ = restore_empty_directory(dirs, &parent, &name, mode);
                    return Err(error);
                }
            };
            let mut link_form = LinkForm {
                link_path: original.to_path_buf(),
                target: root.path.clone(),
                restore: LinkRestore::EmptyDirectory,
                original_mode: Some(mode),
                published_identity: None,
                root_id: root.root_id,
                marker_digest: root.marker_digest,
            };
            let mut intent = read_adopt_intent(state, request.kind)?
                .ok_or_else(|| unknown("adopt intent missing"))?;
            intent.link = Some(link_form.clone());
            write_adopt_intent(state, &intent)?;
            link_form.published_identity = Some(
                match publish_link(state, request.kind, dirs, original, &root.path) {
                    Ok(identity) => identity,
                    Err(error) => {
                        let _ = restore_empty_directory(dirs, &parent, &name, mode);
                        return Err(error);
                    }
                },
            );
            let form = PlacementForm::Link(link_form.clone());
            let record = match finish_record(
                state,
                dirs,
                request.kind,
                original,
                record,
                &folder,
                root.path,
                form,
            ) {
                Ok(record) => record,
                Err(error) => {
                    let rolled_back = rollback_publication(
                        state,
                        dirs,
                        request.kind,
                        effective_uid,
                        &PlacementForm::Link(link_form.clone()),
                    );
                    return Err(error.with_detail("rolled_back", rolled_back.into()));
                }
            };
            Ok(AdoptReport {
                plan: false,
                placement: Some(record),
                changes,
                notes: Vec::new(),
            })
        }
        Inspection::File(facts) | Inspection::Other(facts) => Err(plan_error(
            format!(
                "{} is a {}; a cache placement's original path is a directory or absent",
                original.display(),
                facts.kind.as_str()
            ),
            vec!["move or remove it yourself, then adopt".to_owned()],
        )
        .with_detail("path", original.display().to_string().into())),
    }
}

/// Re-resolve the volume's caches folder under the volume lock, so the effect
/// runs against the walk the lock protects.
fn re_resolve(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
) -> Result<FeatureFolder> {
    area::resolve_feature(dirs, facts, mounts, observer, record, Feature::Caches)
        .map_err(|error| absent_volume(error, record))
}

/// Whether one path is a directory on the registered volume: its device matches
/// and it sits under the recorded mount point.
///
/// Both facts are needed: the device alone would accept another directory of the
/// same machine, and the path alone would accept a stale mount-point directory.
fn on_registered_volume(dirs: &dyn DirOps, record: &VolumeRecord, path: &Path) -> Result<bool> {
    let Some(mount) = record.mount_hint.as_deref() else {
        return Ok(false);
    };
    if !is_under_path(path, mount) {
        return Ok(false);
    }
    let Some(facts) = dirs.path_facts(path)? else {
        return Ok(false);
    };
    Ok(facts.device == volume_device(dirs, record)?)
}

/// The identity a marker for a cache root records.
fn cache_area() -> area::MarkerArea {
    area::MarkerArea::identity(CACHE_AREA_NAMESPACE, CACHE_AREA_NAME)
}

/// The device the registered volume reports, through a directory it holds.
fn volume_device(dirs: &dyn DirOps, record: &VolumeRecord) -> Result<DeviceId> {
    let mount = record.mount_hint.as_ref().ok_or_else(|| {
        unknown(format!(
            "volume {} has no recorded mount point",
            record.volume_uuid
        ))
    })?;
    Ok(dirs.open_path(mount)?.identity().device)
}

/// Create the cache's directory when it is missing, through the containment
/// proof a root creation uses.
fn create_cache_directory(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    feature: Option<&FeatureFolder>,
    target: &Path,
) -> Result<bool> {
    let Some(feature) = feature else {
        return Ok(false);
    };
    if !is_under_path(target, &feature.path) {
        return Ok(false);
    }
    if prove_cache_path(dirs, feature, target)?.is_some() {
        return Ok(false);
    }
    let (mut parent, _) = area::open_or_create_feature(dirs, observer, feature)?;
    let device = parent.identity().device;
    for component in target
        .strip_prefix(&feature.path)
        .map_err(|_| unknown("cache path escaped"))?
        .components()
    {
        let Component::Normal(name) = component else {
            return Err(unknown("invalid cache component"));
        };
        let name = name
            .to_str()
            .ok_or_else(|| unknown("non-UTF8 cache component"))?;
        parent = area::ensure_child_directory(dirs, observer, &parent, name, device)?;
    }
    let _ = record;
    Ok(true)
}

/// Adopt one directory in place: observe it, then write its marker. Nothing is
/// renamed, linked, copied or moved.
#[allow(clippy::too_many_arguments)]
fn adopt_in_place(
    state: &StatePaths,
    kind: CacheKind,
    dirs: &dyn DirOps,
    record: &VolumeRecord,
    folder: &FeatureFolder,
    path: &Path,
    effective_uid: u32,
) -> Result<HeldRoot> {
    let facts = dirs.path_facts(path)?.ok_or_else(|| {
        unknown(format!(
            "{} is absent, so there is nothing to adopt in place",
            path.display()
        ))
    })?;
    if facts.kind != EntryKind::Directory {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is a {}, and only a directory is adopted in place",
                path.display(),
                facts.kind.as_str()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if facts.uid != effective_uid {
        return Err(Error::new(
            ErrorKind::ForeignOwner,
            format!(
                "{} is owned by uid {}, not this account's {effective_uid}",
                path.display(),
                facts.uid
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if facts.device != volume_device(dirs, record)? {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is not on the registered volume {}",
                path.display(),
                record.volume_uuid
            ),
        )
        .with_detail("path", path.display().to_string().into())
        .with_detail("volume_uuid", record.volume_uuid.as_str().into()));
    }
    let handle =
        prove_cache_path(dirs, folder, path)?.ok_or_else(|| unknown("cache target disappeared"))?;
    if handle.identity()
        != (crate::os::DirId {
            device: facts.device,
            inode: facts.inode,
        })
    {
        return Err(Error::new(
            ErrorKind::RefusedContainment,
            format!("{} changed between the check and the open", path.display()),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    let expectation = MarkerExpectation {
        volume_uuid: &record.volume_uuid,
        resolved_path: path,
        effective_uid,
        now_ms: state::now_ms(),
    };
    match area::read_marker(dirs, &handle, &expectation) {
        MarkerState::Absent => {}
        MarkerState::Owned(marker) => {
            return Err(Error::new(
                ErrorKind::RefusedBoundary,
                format!(
                    "{} already carries a Barge marker (root {}); it is already adopted",
                    path.display(),
                    marker.root_id
                ),
            )
            .with_detail("path", path.display().to_string().into())
            .with_detail("root_id", marker.root_id.into()))
        }
        MarkerState::Unknown { reason } => {
            return Err(unknown(format!(
                "{} carries a marker that is not owned: {reason}",
                path.display()
            ))
            .with_detail("path", path.display().to_string().into()))
        }
    }
    let listing = dirs.list_children(&handle, crate::contract::LISTING_NAME_ROWS)?;
    let newest = listing
        .entries
        .iter()
        .filter_map(|entry| entry.facts.modified_ms)
        .max();
    let observed = PreAdoption {
        device: facts.device.get(),
        inode: facts.inode,
        entries: u64::try_from(listing.total).unwrap_or(u64::MAX),
        newest_modified_ms: newest,
        allocated_bytes: facts.allocated_bytes,
    };
    let root_id = hex_encode(&dirs.random_bytes(16)?);
    let mut intent =
        read_adopt_intent(state, kind)?.ok_or_else(|| unknown("marker intent missing"))?;
    intent.marker_plan = Some(MarkerPlan {
        path: path.to_path_buf(),
        root_id: root_id.clone(),
        observed: observed.clone(),
    });
    write_adopt_intent(state, &intent)?;
    let write = MarkerWrite {
        root_id: Some(&root_id),
        volume_uuid: &record.volume_uuid,
        resolved_path: path,
        area: cache_area(),
        kind: RootKind::CacheRoot,
        origin: RootOrigin::AdoptedExisting,
    };
    let marker = area::write_marker(dirs, &handle, &write, effective_uid)?;
    let held = HeldRoot {
        path: path.to_path_buf(),
        root_id: marker.root_id,
        nonce: marker.nonce,
        device: facts.device.get(),
        inode: facts.inode,
        marker_digest: marker.marker_digest,
        observed,
    };
    let mut intent =
        read_adopt_intent(state, kind)?.ok_or_else(|| unknown("marker intent missing"))?;
    intent.held = Some(held.clone());
    write_adopt_intent(state, &intent)?;
    Ok(held)
}

fn restore_empty_directory(
    dirs: &dyn DirOps,
    parent: &DirHandle,
    name: &str,
    mode: u32,
) -> Result<()> {
    let identity = dirs.create_child_dir(parent, name, mode)?;
    let handle = dirs.open_child_dir(parent, name)?;
    if handle.identity() != identity {
        return Err(unknown(
            "restored directory was replaced before mode restoration",
        ));
    }
    dirs.set_dir_mode(&handle, mode)
}

/// Rollback owns no public name merely because adopt once wrote it. Capture
/// every effect and verify it against the recorded publication before mutation.
fn rollback_publication(
    state: &StatePaths,
    dirs: &dyn DirOps,
    kind: CacheKind,
    uid: u32,
    form: &PlacementForm,
) -> Vec<String> {
    match rollback_captured(state, dirs, kind, uid, form) {
        Ok(()) => vec!["rolled back only verified captured adoption effects".to_owned()],
        Err(error) => vec![format!(
            "rollback refused; preserve publication, adoption intent and capture journal: {error}"
        )],
    }
}

fn rollback_captured(
    state: &StatePaths,
    dirs: &dyn DirOps,
    kind: CacheKind,
    uid: u32,
    form: &PlacementForm,
) -> Result<()> {
    let mut captured = undo_capture::Captures::for_adopt(state, dirs, kind, uid, "adopt-rollback")?;
    let held = match form {
        PlacementForm::Setting(setting) => setting.held_root.as_ref(),
        PlacementForm::HeldRoot(held) => Some(&held.held_root),
        PlacementForm::Link(_) => None,
    };
    let publication = match form {
        PlacementForm::Setting(setting) => {
            Some(captured.file(&setting.settings_path, setting.after.clone())?)
        }
        PlacementForm::Link(link) => Some(
            captured.link(
                &link.link_path,
                link.published_identity
                    .ok_or_else(|| unknown("rollback has no published link identity"))?,
            )?,
        ),
        PlacementForm::HeldRoot(_) => None,
    };
    let marker = held
        .map(|held| {
            let snapshot = marker_snapshot(dirs, Some(&held.path))?
                .ok_or_else(|| unknown("rollback marker is absent"))?;
            if snapshot.digest != held.marker_digest {
                return Err(unknown("rollback marker diverged"));
            }
            captured.marker(held, snapshot)
        })
        .transpose()?;
    captured.capture_all()?;
    if let Some(index) = publication {
        match form {
            PlacementForm::Setting(setting) if !setting.changed => {
                captured.return_unchanged(index)?
            }
            PlacementForm::Setting(setting) => {
                if let Some(before) = &setting.before {
                    captured.restore_file(index, &decode_hex(&before.bytes)?, before.mode)?;
                }
                captured.remove(index)?;
            }
            PlacementForm::Link(link) => {
                if link.restore == LinkRestore::EmptyDirectory {
                    let (parent, name) = open_parent(dirs, &link.link_path)?;
                    restore_empty_directory(
                        dirs,
                        &parent,
                        &name,
                        link.original_mode
                            .ok_or_else(|| unknown("rollback lacks original mode"))?,
                    )?;
                }
                captured.remove(index)?;
            }
            PlacementForm::HeldRoot(_) => return Err(unknown("unexpected held-root publication")),
        }
    }
    if let Some(index) = marker {
        captured.remove(index)?;
    }
    captured.settle()
}

#[allow(clippy::too_many_arguments)]
fn finish_record(
    state: &StatePaths,
    dirs: &dyn DirOps,
    kind: CacheKind,
    original: &Path,
    volume: &VolumeRecord,
    folder: &FeatureFolder,
    recorded_path: PathBuf,
    form: PlacementForm,
) -> Result<PlacementRecord> {
    let record = PlacementRecord {
        schema: PLACEMENT_SCHEMA.to_owned(),
        placement_id: hex_encode(&dirs.random_bytes(16)?),
        kind,
        original_path: original.to_path_buf(),
        volume_uuid: volume.volume_uuid.clone(),
        root_dir: volume.root_dir.clone(),
        folder: folder.folder.clone(),
        recorded_path,
        adopted_ms: state::now_ms(),
        generation: 1,
        form,
    };
    record.validate()?;
    let mut intent =
        read_adopt_intent(state, kind)?.ok_or_else(|| unknown("adopt intent is missing"))?;
    intent.candidate = Some(record.clone());
    intent.expected_setting = None; // candidate carries the exact after bytes; keep the journal bounded.
    intent.setting_after = None;
    write_adopt_intent(state, &intent)?;
    publish_record(state, &record)?;
    let path = placement_path(state, &record.placement_id)?;
    match state::read_bounded(
        &path,
        &Bounds::new(PLACEMENT_CAP_BYTES, 1, Deadline::after(READ_DEADLINE)),
    ) {
        ReadOutcome::Present(bytes)
            if serde_json::from_slice::<PlacementRecord>(&bytes)
                .ok()
                .as_ref()
                == Some(&record) => {}
        _ => {
            return Err(unknown(
                "placement publication readback is uncertain; keep the recovery intent",
            ))
        }
    }
    Ok(record)
}

/// Publish a symbolic link at `original` through a temporary name, then prove
/// the text that is published.
fn publish_link(
    state: &StatePaths,
    kind: CacheKind,
    dirs: &dyn DirOps,
    original: &Path,
    target: &Path,
) -> Result<(u64, u64)> {
    let (parent, name) = open_parent(dirs, original)?;
    let facts = dirs.path_facts(original)?;
    if let Some(facts) = facts {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is a {} and a link cannot be published over it",
                original.display(),
                facts.kind.as_str()
            ),
        )
        .with_detail("path", original.display().to_string().into()));
    }
    let temporary = format!(".{name}.barge-link-{}", hex_encode(&dirs.random_bytes(8)?));
    dirs.create_child_symlink(&parent, &temporary, target)?;
    let identity = dirs
        .child_facts(&parent, &temporary)?
        .ok_or_else(|| unknown("temporary link vanished"))?;
    if identity.kind != EntryKind::Symlink {
        return Err(unknown("temporary link was replaced"));
    }
    let identity = (identity.device.get(), identity.inode);
    let mut intent =
        read_adopt_intent(state, kind)?.ok_or_else(|| unknown("publication intent missing"))?;
    if let Some(link) = &mut intent.link {
        link.published_identity = Some(identity);
    }
    write_adopt_intent(state, &intent)?;
    let publication = match dirs.rename_child(&parent, &temporary, &name) {
        Ok(RenameOutcome::Renamed) => Ok(()),
        Ok(RenameOutcome::TargetExists) => Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} appeared before link publication; nothing was replaced",
                original.display()
            ),
        )
        .with_detail("path", original.display().to_string().into())),
        Err(error) => Err(error),
    };
    if let Err(error) = publication {
        let _ = dirs.remove_child(&parent, &temporary, RemoveKind::File);
        return Err(error);
    }
    if entry_identity(dirs, original)? != identity {
        return Err(unknown("published link was concurrently replaced"));
    }
    match dirs.read_link(original)? {
        Some(text) if text == target => Ok(identity),
        Some(text) => Err(unknown(format!(
            "{} publishes {}, not the recorded {}",
            original.display(),
            text.display(),
            target.display()
        ))
        .with_detail("path", original.display().to_string().into())),
        None => Err(unknown(format!(
            "{} is absent just after its link was published",
            original.display()
        ))
        .with_detail("path", original.display().to_string().into())),
    }
}

/// Prove an empty real directory holds nothing and nobody has it open, then
/// leave it to the caller's `rmdir`.
fn confirm_empty_directory(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    original: &Path,
    expected: &EntryFacts,
) -> Result<()> {
    let (parent, name) = open_parent(dirs, original)?;
    let checked = dirs.child_facts(&parent, &name)?.ok_or_else(|| {
        unknown(format!(
            "{} disappeared while it was inspected",
            original.display()
        ))
    })?;
    if checked.kind != EntryKind::Directory
        || checked.uid != observer.effective_uid()?
        || checked.device != expected.device
        || checked.inode != expected.inode
    {
        return Err(Error::new(
            ErrorKind::RefusedContainment,
            format!(
                "{} is no longer the directory that was inspected",
                original.display()
            ),
        )
        .with_detail("path", original.display().to_string().into()));
    }
    let handle = dirs.open_child_dir(&parent, &name)?;
    let listing = dirs.list_children(&handle, 1)?;
    if listing.total != 0 {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} holds {} entries and is only replaced when it is empty",
                original.display(),
                listing.total
            ),
        )
        .with_detail("path", original.display().to_string().into())
        .with_detail("entries", listing.total.into()));
    }
    // The calling process holds this directory open for the listing above, so
    // its own pid is excluded from the evidence: read once here, with the start
    // identity the kernel reports for it, and verified by the platform as it
    // scans. Nothing else is excluded, and an identity that cannot be read or is
    // absent is a refusal, never a skip.
    let self_pid = std::process::id();
    let self_start = observer
        .process_start(self_pid)
        .map_err(|error| {
            Error::new(
                ErrorKind::CustodyNotProven,
                format!(
                    "the calling process's start identity is unreadable, so its own descriptors cannot be excluded: {error}"
                ),
            )
        })?
        .ok_or_else(|| {
            Error::new(
                ErrorKind::CustodyNotProven,
                format!(
                    "the calling process pid {self_pid} has no start identity, so its own descriptors cannot be excluded"
                ),
            )
        })?;
    let scan = observer.scan_open_descriptors(&DescriptorScanRequest {
        root: original,
        root_device: checked.device,
        root_inode: checked.inode,
        max_processes: SCAN_PROCESSES,
        max_descriptors: SCAN_DESCRIPTORS,
        timeout: SCAN_TIMEOUT,
        self_exclusion: Some(SelfExclusion {
            pid: self_pid,
            start: self_start,
        }),
    })?;
    match scan {
        DescriptorScan::Complete { open, .. } if open.is_empty() => Ok(()),
        DescriptorScan::Complete { open, .. } => Err(Error::new(
            ErrorKind::CustodyNotProven,
            format!(
                "{} is held open by pid {} descriptor {}",
                original.display(),
                open.first().map_or(0, |descriptor| descriptor.pid),
                open.first().map_or(0, |descriptor| descriptor.descriptor)
            ),
        )
        .with_detail("path", original.display().to_string().into())),
        DescriptorScan::Bounded { bound, retry, .. } => Err(Error::new(
            ErrorKind::CustodyNotProven,
            format!(
                "the descriptor observation of {} reached its {bound} bound",
                original.display()
            ),
        )
        .with_detail("path", original.display().to_string().into())
        .with_detail(
            "retry",
            serde_json::to_value(retry)
                .map_err(|error| Error::new(ErrorKind::Io, error.to_string()))?,
        )),
        DescriptorScan::Unreadable { reason, retry } => {
            Err(Error::new(ErrorKind::UnknownState, reason).with_detail(
                "retry",
                serde_json::to_value(retry)
                    .map_err(|error| Error::new(ErrorKind::Io, error.to_string()))?,
            ))
        }
    }
}

/// The parent of one path, opened, and the path's last name.
fn open_parent(dirs: &dyn DirOps, path: &Path) -> Result<(DirHandle, String)> {
    let parent = parent_of(path)?;
    let name = file_name_of(path)?;
    let mut handle = dirs.open_path(Path::new("/"))?;
    for component in parent.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| unknown("non-UTF8 parent component"))?;
                let entry = dirs
                    .child_facts(&handle, part)?
                    .ok_or_else(|| unknown("placement parent is absent"))?;
                if entry.kind != EntryKind::Directory {
                    return Err(Error::new(
                        ErrorKind::RefusedContainment,
                        "placement parent is a link or non-directory",
                    ));
                }
                let child = dirs.open_child_dir(&handle, part)?;
                if child.identity().device != entry.device || child.identity().inode != entry.inode
                {
                    return Err(unknown("placement parent changed during walk"));
                }
                handle = child;
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::RefusedContainment,
                    "invalid parent component",
                ))
            }
        }
    }
    Ok((handle, name))
}

fn parent_of(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        Error::new(
            ErrorKind::Usage,
            format!("{} has no parent directory", path.display()),
        )
    })
}

fn file_name_of(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Usage,
                format!("{} is not a path with a file name", path.display()),
            )
        })
}

fn entry_identity(dirs: &dyn DirOps, path: &Path) -> Result<(u64, u64)> {
    let entry = dirs
        .path_facts(path)?
        .ok_or_else(|| unknown("publication disappeared"))?;
    Ok((entry.device.get(), entry.inode))
}

/// Read-only descriptor walk from the verified feature folder. No ancestor may
/// be a symlink, cross device, or change identity between check and open.
fn prove_cache_path(
    dirs: &dyn DirOps,
    folder: &FeatureFolder,
    path: &Path,
) -> Result<Option<DirHandle>> {
    if !is_under_path(path, &folder.path) {
        return Err(Error::new(
            ErrorKind::RefusedContainment,
            "cache path escaped its verified folder",
        ));
    }
    let Some(mut handle) = area::open_existing_feature(dirs, folder)? else {
        return Ok(None);
    };
    let device = handle.identity().device;
    for part in path
        .strip_prefix(&folder.path)
        .map_err(|_| unknown("cache prefix changed"))?
        .components()
    {
        let Component::Normal(name) = part else {
            return Err(unknown("invalid cache path component"));
        };
        let name = name
            .to_str()
            .ok_or_else(|| unknown("non-UTF8 cache path component"))?;
        let Some(entry) = dirs.child_facts(&handle, name)? else {
            return Ok(None);
        };
        if entry.kind != EntryKind::Directory || entry.device != device {
            return Err(Error::new(
                ErrorKind::RefusedContainment,
                "cache ancestor is not a real directory on the verified device",
            ));
        }
        let child = dirs.open_child_dir(&handle, name)?;
        if child.identity().device != entry.device || child.identity().inode != entry.inode {
            return Err(unknown("cache component changed during walk"));
        }
        handle = child;
    }
    Ok(Some(handle))
}

/// Whether `path` is at or under `root`, by whole components.
pub fn is_under_path(path: &Path, root: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        && (path == root || path.starts_with(root))
}

fn is_under(value: &str, root: &Path) -> bool {
    let path = Path::new(value);
    path.is_absolute() && is_under_path(path, root)
}

// ---- settings ------------------------------------------------------------

/// Read one settings file: its facts and exact bytes, or `None` when absent.
fn read_settings(dirs: &dyn DirOps, path: &Path) -> Result<Option<(EntryFacts, Vec<u8>)>> {
    let (handle, name) = open_parent(dirs, path)?;
    let Some(facts) = dirs.child_facts(&handle, &name)? else {
        return Ok(None);
    };
    if facts.kind != EntryKind::File {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is a {}, and only a regular settings file is edited",
                path.display(),
                facts.kind.as_str()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    if facts.size > SETTING_CAP_BYTES as u64 {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "{} is {} bytes, over the {SETTING_CAP_BYTES}-byte settings cap; it is not edited",
                path.display(),
                facts.size
            ),
        )
        .with_detail("path", path.display().to_string().into())
        .with_detail("cap", SETTING_CAP_BYTES.into()));
    }
    let bytes = dirs
        .read_child_file(&handle, &name, SETTING_CAP_BYTES)?
        .ok_or_else(|| unknown(format!("{} disappeared while it was read", path.display())))?;
    if dirs.child_facts(&handle, &name)?.as_ref() != Some(&facts) {
        return Err(unknown("settings identity changed during read"));
    }
    Ok(Some((facts, bytes)))
}

fn confirm_setting_at(
    dirs: &dyn DirOps,
    parent: &DirHandle,
    name: &str,
    expected: &SettingsSnapshot,
) -> Result<()> {
    let facts = dirs
        .child_facts(parent, name)?
        .ok_or_else(|| unknown("settings vanished before mutation"))?;
    let bytes = dirs
        .read_child_file(parent, name, SETTING_CAP_BYTES)?
        .ok_or_else(|| unknown("settings vanished before mutation"))?;
    if snapshot_of(&facts, &bytes)? != *expected
        || dirs.child_facts(parent, name)?.as_ref() != Some(&facts)
    {
        return Err(unknown(
            "settings identity or bytes changed immediately before mutation",
        ));
    }
    Ok(())
}

fn snapshot_of(facts: &EntryFacts, bytes: &[u8]) -> Result<SettingsSnapshot> {
    Ok(SettingsSnapshot {
        device: facts.device.get(),
        inode: facts.inode,
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        mode: facts.mode & 0o777,
        modified_ms: facts.modified_ms,
        digest: sha256_hex(bytes),
        bytes: hex_encode(bytes),
    })
}

/// The value one setting currently states, or `None` when the key is not set.
///
/// A file with more than one candidate key, a malformed section header or an
/// unterminated assignment is unknown state: it is refused rather than edited.
fn setting_value(key: &SettingKey, bytes: &[u8], facts: &EntryFacts) -> Result<Option<String>> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        unknown(format!(
            "the settings file is not UTF-8 text, so its keys cannot be read: {error}"
        ))
    })?;
    let lines = lines_of(text);
    let indices = candidate_lines(key, &lines)?;
    match indices.as_slice() {
        [] => Ok(None),
        [index] => Ok(Some(lines[*index].value.clone())),
        _ => Err(unknown(format!(
            "the settings file states `{}` {} times, so which one is the key is unknown",
            key.key,
            indices.len()
        ))
        .with_detail("key", key.key.clone().into())
        .with_detail("candidates", indices.len().into())
        .with_detail("device", facts.device.to_string().into())),
    }
}

/// Return `bytes` with the setting written to `value` (the exact change adopt
/// reports and undo reverses).
fn write_setting_value(key: &SettingKey, bytes: &[u8], value: &Path) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        unknown(format!(
            "the settings file is not UTF-8 text, so its keys cannot be written: {error}"
        ))
    })?;
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let lines: Vec<Line> = raw_lines.iter().map(|line| Line::parse(line)).collect();
    let indices = candidate_lines(key, &lines)?;
    if indices.len() > 1 {
        return Err(unknown(format!(
            "the settings file states `{}` {} times, so it is not edited",
            key.key,
            indices.len()
        )));
    }
    let rendered = match &key.section {
        None => format!("{}={}", key.key, value.display()),
        Some(_) => format!("{} = \"{}\"", key.key, value.display()),
    };
    let mut output: Vec<String> = raw_lines.iter().map(|line| (*line).to_owned()).collect();
    match indices.as_slice() {
        [index] => output[*index] = rendered,
        [] => match &key.section {
            None => match output.len() {
                1 if output[0].is_empty() => output[0] = rendered,
                _ => {
                    if !output.last().is_some_and(|line| line.is_empty()) {
                        output.push(String::new());
                    }
                    let last = output.len().saturating_sub(1);
                    output.insert(last, rendered);
                }
            },
            Some(section) => {
                let header = format!("[{section}]");
                let Some(index) = lines
                    .iter()
                    .position(|line| line.header.as_deref() == Some(&header))
                else {
                    if !output.last().is_some_and(|line| line.is_empty()) {
                        output.push(String::new());
                    }
                    let at = output.len().saturating_sub(1);
                    output.insert(at, header);
                    output.insert(at.saturating_add(1), rendered);
                    return Ok(output.join("\n").into_bytes());
                };
                output.insert(index + 1, rendered);
            }
        },
        _ => {
            return Err(unknown(format!(
                "the settings file states `{}` more than once, so it is not edited",
                key.key
            )))
        }
    }
    Ok(output.join("\n").into_bytes())
}

/// One parsed settings line.
struct Line {
    /// The section header this line is, brackets included.
    header: Option<String>,
    /// The assignment's key, when the line is one.
    key: Option<String>,
    /// The assignment's value, unquoted.
    value: String,
}

impl Line {
    fn parse(text: &str) -> Self {
        let trimmed = text.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            return Self::blank();
        }
        if trimmed.starts_with('[') {
            return Self {
                header: Some(trimmed.to_owned()),
                key: None,
                value: String::new(),
            };
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Self::blank();
        };
        Self {
            header: None,
            key: Some(key.trim().to_owned()),
            value: unquoted(value.trim()),
        }
    }

    fn blank() -> Self {
        Self {
            header: None,
            key: None,
            value: String::new(),
        }
    }
}

fn unquoted(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

fn lines_of(text: &str) -> Vec<Line> {
    text.split('\n').map(Line::parse).collect()
}

/// The line indices that state one setting's key, refusing a malformed shape.
fn candidate_lines(key: &SettingKey, lines: &[Line]) -> Result<Vec<usize>> {
    let mut indices = Vec::new();
    match &key.section {
        None => {
            for (index, line) in lines.iter().enumerate() {
                if line.header.is_some() {
                    return Err(unknown(
                        "the settings file has a section header where a top-level key belongs, so its shape is unknown",
                    ));
                }
                if line.key.as_deref() == Some(key.key.as_str()) {
                    indices.push(index);
                }
            }
        }
        Some(section) => {
            let header = format!("[{section}]");
            let headers = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.header.as_deref() == Some(header.as_str()))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if headers.len() > 1 {
                return Err(unknown(format!(
                    "the settings file states the section {header} {} times",
                    headers.len()
                )));
            }
            let mut in_section = false;
            for (index, line) in lines.iter().enumerate() {
                match &line.header {
                    Some(actual) if actual == &header => in_section = true,
                    Some(actual) => {
                        if !actual.ends_with(']') {
                            return Err(unknown(format!(
                                "the settings file has a malformed section header `{actual}`"
                            )));
                        }
                        in_section = false;
                    }
                    None => {
                        if in_section && line.key.as_deref() == Some(key.key.as_str()) {
                            indices.push(index);
                        }
                    }
                }
            }
        }
    }
    Ok(indices)
}

// ---- undo ----------------------------------------------------------------

/// What one undo is asked to reverse.
#[derive(Debug, Clone)]
pub struct UndoRequest<'a> {
    /// The record's id, when the caller knows it.
    pub placement_id: Option<&'a str>,
    /// The kind, when the caller names the placement that way.
    pub kind: Option<CacheKind>,
    /// Inspect and report without making any effect.
    pub plan: bool,
}

/// One undo's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoReport {
    /// Whether this was a preview.
    pub plan: bool,
    /// The record this undo reversed.
    pub placement_id: String,
    /// The kind it served.
    pub kind: CacheKind,
    /// What was done, or what a preview would do, in order.
    pub changes: Vec<String>,
    /// Whether the record was removed (always false in a preview).
    pub removed: bool,
}

/// Reverse exactly the recorded form.
///
/// A `setting` restores the recorded before-bytes only when the file still holds
/// the exact after-bytes; a `link` removes the link it published and restores an
/// empty directory where one was replaced; a `held-root` removes only the marker
/// it wrote. Nothing a caller cannot prove is changed, and no cache data is ever
/// deleted.
#[allow(clippy::too_many_arguments)]
pub fn undo(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &UndoRequest<'_>,
) -> Result<UndoReport> {
    for kind in [
        CacheKind::NpmCache,
        CacheKind::BunCache,
        CacheKind::CargoRegistry,
        CacheKind::CargoGit,
    ] {
        if request.kind.is_none_or(|requested| requested == kind) {
            undo_capture::require_no_pending(state, kind)?;
        }
    }
    let mut pending = if let Some(kind) = request.kind {
        read_adopt_intent(state, kind)?
    } else {
        let mut found = None;
        for kind in [
            CacheKind::NpmCache,
            CacheKind::BunCache,
            CacheKind::CargoRegistry,
            CacheKind::CargoGit,
        ] {
            if let Some(intent) = read_adopt_intent(state, kind)? {
                if request.placement_id == Some(intent.id.as_str())
                    || intent.candidate.as_ref().is_some_and(|record| {
                        request.placement_id == Some(record.placement_id.as_str())
                    })
                {
                    found = Some(intent);
                    break;
                }
            }
        }
        found
    };
    let mut recovery_locks = LockSet::new(state.clone());
    let recovery_volume = pending
        .as_ref()
        .map(|intent| intent.volume.volume_uuid.clone());
    let _recovery_lock = if !request.plan {
        recovery_volume
            .as_ref()
            .map(|uuid| recovery_locks.lock_volume(uuid))
            .transpose()?
    } else {
        None
    };
    if !request.plan {
        if let Some(intent) = &pending {
            pending = read_adopt_intent(state, intent.kind)?;
        }
    }
    if let Some(intent) = &mut pending {
        if !request.plan {
            let _verified = re_resolve(dirs, facts, mounts, observer, &intent.volume)?;
            recover_marker_observation(state, dirs, facts, mounts, observer, intent)?;
            if original_state(dirs, &intent.original)? == intent.before {
                if let Some(held) = &intent.held {
                    let form = HeldRootForm {
                        path: held.path.clone(),
                        publication: None,
                        held_root: held.clone(),
                    };
                    let _ = rollback_publication(
                        state,
                        dirs,
                        intent.kind,
                        observer.effective_uid()?,
                        &PlacementForm::HeldRoot(form),
                    );
                }
            }
        }
    }
    if let Some(intent) = &pending {
        if !request.plan
            && matches!(intent.before, OriginalState::Directory { entries: 0, .. })
            && matches!(
                original_state(dirs, &intent.original)?,
                OriginalState::Absent
            )
        {
            let OriginalState::Directory { mode, .. } = intent.before else {
                return Err(unknown("invalid directory intent"));
            };
            let (parent, name) = open_parent(dirs, &intent.original)?;
            restore_empty_directory(dirs, &parent, &name, mode)?;
        }
        if adoption_restored(dirs, intent)? {
            if !request.plan {
                if let Some(candidate) = &intent.candidate {
                    let existing = read_placements(state)?;
                    if let Some(stored) = existing.find_id(&candidate.placement_id) {
                        if stored != candidate {
                            return Err(unknown(
                                "pending publication differs from recovery candidate",
                            ));
                        }
                        remove_record(state, &candidate.placement_id)?;
                    }
                }
                clear_adopt_intent(state, intent.kind)?;
            }
            return Ok(UndoReport {
                plan: request.plan,
                placement_id: intent.id.clone(),
                kind: intent.kind,
                changes: vec!["settle the proved rolled-back adoption intent".to_owned()],
                removed: !request.plan,
            });
        }
    }
    let set = read_placements(state)?;
    let recovering = pending
        .as_ref()
        .map(|intent| pending_record(dirs, intent))
        .transpose()?;
    let record = match (request.placement_id, request.kind) {
        (Some(id), _) => set.find_id(id).cloned(),
        (None, Some(kind)) => set.find_kind(kind).cloned(),
        (None, None) => {
            return Err(Error::new(
                ErrorKind::Usage,
                "undo takes a placement id or a kind",
            ))
        }
    }
    .or(recovering)
    .ok_or_else(|| {
        unknown(match (request.placement_id, request.kind) {
            (Some(id), _) => format!("placement {id} is not stored"),
            (None, Some(kind)) => format!("{} is not adopted", kind.as_str()),
            (None, None) => "no placement named".to_owned(),
        })
    })?;
    let _undo_lock = if !request.plan && recovery_volume.is_none() {
        Some(recovery_locks.lock_volume(&record.volume_uuid)?)
    } else {
        None
    };
    undo_capture::require_no_pending(state, record.kind)?;
    let effective_uid = observer.effective_uid()?;
    let result = match &record.form {
        PlacementForm::Setting(form) => undo_setting(
            state,
            dirs,
            facts,
            mounts,
            request,
            &record,
            form,
            effective_uid,
        ),
        PlacementForm::Link(form) => {
            undo_link(state, dirs, facts, mounts, observer, request, &record, form)
        }
        PlacementForm::HeldRoot(form) => undo_held(
            state,
            dirs,
            facts,
            mounts,
            request,
            &record,
            form,
            effective_uid,
        ),
    };
    if result.as_ref().is_ok_and(|report| report.removed) {
        if let Some(intent) = pending {
            clear_adopt_intent(state, intent.kind)?;
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn undo_setting(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    request: &UndoRequest<'_>,
    record: &PlacementRecord,
    form: &SettingForm,
    effective_uid: u32,
) -> Result<UndoReport> {
    // Absence after adopt is divergence even when adopt created the file.
    // Require the recorded after-file before observing or capturing its marker.
    let (after_facts, after_bytes) =
        read_settings(dirs, &form.settings_path)?.ok_or_else(|| {
            diverged(
                &form.settings_path,
                &form.after.digest,
                "the file is absent",
            )
        })?;
    if snapshot_of(&after_facts, &after_bytes)? != form.after {
        return Err(diverged(
            &form.settings_path,
            &form.after.digest,
            "file identity or bytes changed since adopt",
        ));
    }
    let mut changes = Vec::new();
    let restored_bytes = match &form.before {
        Some(before) => {
            let restored = decode_hex(&before.bytes)?;
            if restored == after_bytes {
                changes.push(format!(
                    "{} already holds its pre-adopt bytes; undo writes nothing",
                    form.settings_path.display()
                ));
            } else {
                changes.push(format!(
                    "restore {} to the {} bytes it held before adopt",
                    form.settings_path.display(),
                    restored.len()
                ));
            }
            Some(restored)
        }
        None => {
            changes.push(format!(
                "remove {}, which adopt created",
                form.settings_path.display()
            ));
            None
        }
    };
    // Observe both effects without removing either. Capture and verify the
    // settings first and the marker second, before committing any mutation.
    let marker = form
        .held_root
        .as_ref()
        .map(|held| observe_adopted_marker(state, dirs, facts, mounts, record, held, effective_uid))
        .transpose()?;
    if let Some((_, marker_changes)) = &marker {
        changes.extend(marker_changes.clone());
    }
    if request.plan {
        return Ok(undo_report(request, record, changes));
    }
    let mut captured = undo_capture::Captures::new(state, dirs, record, effective_uid);
    let setting_index = captured.file(&form.settings_path, form.after.clone())?;
    let marker_index = match (&form.held_root, marker) {
        (Some(held), Some((snapshot, _))) => Some(captured.marker(held, snapshot)?),
        _ => None,
    };
    captured.capture_all()?;
    match (&form.before, restored_bytes) {
        (Some(before), Some(_)) if before.bytes == form.after.bytes => {
            captured.return_unchanged(setting_index)?;
        }
        (Some(before), Some(restored)) => {
            captured.restore_file(
                setting_index,
                &restored,
                if before.mode == 0 {
                    form.after.mode
                } else {
                    before.mode
                },
            )?;
            captured.remove(setting_index)?;
        }
        (None, _) => captured.remove(setting_index)?,
        (Some(_), None) => return Err(unknown("settings restore has no bytes")),
    }
    if let Some(index) = marker_index {
        captured.remove(index)?;
    }
    captured.finish()?;
    Ok(UndoReport {
        plan: false,
        placement_id: record.placement_id.clone(),
        kind: record.kind,
        changes,
        removed: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn undo_link(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &UndoRequest<'_>,
    record: &PlacementRecord,
    form: &LinkForm,
) -> Result<UndoReport> {
    let mut changes = Vec::new();
    // The link's text is the publication proof: it must still be the recorded
    // one, or the link is not the link this record published.
    match dirs.read_link(&form.link_path)? {
        Some(text) if text == form.target => {}
        Some(text) => {
            return Err(unknown(format!(
                "{} is a link to {}, not the recorded {}",
                form.link_path.display(),
                text.display(),
                form.target.display()
            ))
            .with_detail("path", form.link_path.display().to_string().into())
            .with_detail("recorded_target", form.target.display().to_string().into()))
        }
        None => {
            return Err(unknown(format!(
                "{} is absent, so the recorded link is not the one to remove",
                form.link_path.display()
            ))
            .with_detail("path", form.link_path.display().to_string().into()))
        }
    }
    // Custody is required before a link removal, exactly as it is before a swap
    // or a delete: the owned root must be the recorded one, and no same-uid
    // process may hold it open. `observe_adopted_marker` needs the same proofs for
    // a held root; this is the link form's own.
    let observed = observed_link_root(state, dirs, facts, mounts, observer, record, form)?;
    changes.push(format!("remove the link {}", form.link_path.display()));
    if form.restore == LinkRestore::EmptyDirectory {
        changes.push(format!(
            "restore the empty directory {}",
            form.link_path.display()
        ));
    }
    if request.plan {
        return Ok(undo_report(request, record, changes));
    }
    take_link_custody(state, dirs, facts, mounts, observer, &observed)?;
    let identity = form
        .published_identity
        .ok_or_else(|| unknown("published link identity is missing; undo refuses"))?;
    let restore_mode = if form.restore == LinkRestore::EmptyDirectory {
        Some(
            form.original_mode
                .ok_or_else(|| unknown("original directory mode is missing; undo refuses"))?,
        )
    } else {
        None
    };
    let mut captured = undo_capture::Captures::new(state, dirs, record, observer.effective_uid()?);
    let index = captured.link(&form.link_path, identity)?;
    captured.capture_all()?;
    if let Some(mode) = restore_mode {
        let (parent, name) = open_parent(dirs, &form.link_path)?;
        restore_empty_directory(dirs, &parent, &name, mode)?;
    }
    captured.remove(index)?;
    captured.finish()?;
    Ok(UndoReport {
        plan: false,
        placement_id: record.placement_id.clone(),
        kind: record.kind,
        changes,
        removed: true,
    })
}

/// Observe the owned root one `link` placement publishes.
///
/// The observation is the one custody needs: the root reopened from the volume's
/// current `caches/` folder, its device and inode, and its marker re-read as the
/// recorded bytes. A root that is absent, moved, re-marked or unreadable is
/// unknown state, never an absent-and-safe answer.
fn observed_link_root(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &PlacementRecord,
    form: &LinkForm,
) -> Result<custody::ObservedRoot> {
    let effective_uid = observer.effective_uid()?;
    let volume = area::registered_volume(state, &record.volume_uuid)?;
    let opened = area::open_folder(
        dirs,
        facts,
        mounts,
        observer,
        &volume,
        &record.root_dir,
        &record.folder,
    )
    .map_err(|error| absent_volume(error, &volume))?;
    let Some((folder_path, folder_handle)) = opened else {
        return Err(unknown(format!(
            "the volume's {} folder is absent, so the root {} cannot be proved",
            record.folder, form.root_id
        ))
        .with_detail("volume_uuid", record.volume_uuid.as_str().into()));
    };
    let root_path = folder_path.join(&form.root_id);
    if root_path != form.target {
        return Err(unknown(format!(
            "the recorded root resolves to {}, not the recorded link text {}",
            root_path.display(),
            form.target.display()
        ))
        .with_detail("recorded_target", form.target.display().to_string().into()));
    }
    let root = dirs.open_child_dir(&folder_handle, &form.root_id)?;
    let expectation = MarkerExpectation {
        volume_uuid: &record.volume_uuid,
        resolved_path: &root_path,
        effective_uid,
        now_ms: state::now_ms(),
    };
    match area::read_marker_with_digest(dirs, &root, &expectation) {
        (MarkerState::Owned(marker), digest) if marker.root_id == form.root_id => {
            if digest.as_deref() != Some(form.marker_digest.as_str()) {
                return Err(unknown(format!(
                    "the root at {} no longer carries the marker of {}",
                    root_path.display(),
                    form.root_id
                ))
                .with_detail("path", root_path.display().to_string().into()));
            }
        }
        (state_name, _) => {
            return Err(unknown(format!(
                "the root at {} is not the recorded one: {}",
                root_path.display(),
                match state_name {
                    MarkerState::Absent => "it carries no marker".to_owned(),
                    MarkerState::Owned(marker) => format!("it records root {}", marker.root_id),
                    MarkerState::Unknown { reason } => reason,
                }
            ))
            .with_detail("path", root_path.display().to_string().into()))
        }
    }
    Ok(custody::ObservedRoot {
        root_id: form.root_id.clone(),
        volume_uuid: record.volume_uuid.clone(),
        root_dir: record.root_dir.clone(),
        folder: record.folder.clone(),
        device: root.identity().device,
        inode: root.identity().inode,
        marker_digest: form.marker_digest.clone(),
        path: root_path,
    })
}

/// Decide custody for one observed root and require `Safe`.
fn take_link_custody(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    observed: &custody::ObservedRoot,
) -> Result<()> {
    let intents = state::read_intents(state, crate::contract::INTENT_ROWS)?;
    let pending = PendingIntent::pending(&intents.records);
    custody::take_custody(
        dirs,
        facts,
        mounts,
        observer,
        state,
        observed,
        &pending,
        CustodyBounds::default(),
    )
    .into_evidence()
    .map(|_| ())
}

/// Prove one in-place adopted directory is still the recorded one, then remove
/// exactly the marker Barge wrote.
///
/// Both proofs are required: the directory is the recorded inode on the recorded
/// device, and its marker bytes digest to the recorded digest. Anything else is
/// unknown state and nothing is removed.
#[allow(clippy::too_many_arguments)]
fn observe_adopted_marker(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    record: &PlacementRecord,
    held: &HeldRoot,
    effective_uid: u32,
) -> Result<(SettingsSnapshot, Vec<String>)> {
    // The marker lives on the volume, so the volume must be the recorded one
    // before the directory's identity is proved.
    let volume = area::registered_volume(state, &record.volume_uuid)?;
    area::VolumeRoot::open(dirs, facts, mounts, &volume)
        .map_err(|error| absent_volume(error, &volume))?;
    let Some(facts_at) = dirs.path_facts(&held.path)? else {
        return Err(unknown(format!(
            "{} is absent, so the adopted root cannot be proved",
            held.path.display()
        ))
        .with_detail("path", held.path.display().to_string().into()));
    };
    if facts_at.kind != EntryKind::Directory
        || facts_at.device.get() != held.device
        || facts_at.inode != held.inode
    {
        return Err(unknown(format!(
            "{} is no longer the directory that was adopted (device {}, inode {})",
            held.path.display(),
            held.device,
            held.inode
        ))
        .with_detail("path", held.path.display().to_string().into())
        .with_detail("recorded_inode", held.inode.into()));
    }
    let handle = dirs.open_path(&held.path)?;
    if (handle.identity().device.get(), handle.identity().inode) != (held.device, held.inode) {
        return Err(unknown("held root changed between observation and open"));
    }
    let expectation = MarkerExpectation {
        volume_uuid: &record.volume_uuid,
        resolved_path: &held.path,
        effective_uid,
        now_ms: state::now_ms(),
    };
    match area::read_marker_with_digest(dirs, &handle, &expectation) {
        (MarkerState::Owned(marker), digest)
            if marker.root_id == held.root_id
                && digest.as_deref() == Some(held.marker_digest.as_str()) => {}
        (state_name, _) => {
            return Err(unknown(format!(
                "{} no longer carries the marker adopt wrote: {}",
                held.path.display(),
                match state_name {
                    MarkerState::Absent => "it is absent".to_owned(),
                    MarkerState::Owned(marker) => format!("it records root {}", marker.root_id),
                    MarkerState::Unknown { reason } => reason,
                }
            ))
            .with_detail("path", held.path.display().to_string().into()))
        }
    }
    let path = held.path.join(area::ROOT_MARKER_FILE);
    let (marker_facts, bytes) = read_settings(dirs, &path)?
        .ok_or_else(|| unknown("adopted marker disappeared while observed"))?;
    if sha256_hex(&bytes) != held.marker_digest {
        return Err(unknown("adopted marker changed after observation"));
    }
    Ok((
        snapshot_of(&marker_facts, &bytes)?,
        vec![format!(
            "remove the marker {} wrote in {}; nothing else is touched",
            area::ROOT_MARKER_FILE,
            held.path.display()
        )],
    ))
}

#[allow(clippy::too_many_arguments)]
fn undo_held(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    request: &UndoRequest<'_>,
    record: &PlacementRecord,
    form: &HeldRootForm,
    effective_uid: u32,
) -> Result<UndoReport> {
    // A link that already published the directory is verified and left alone.
    if let Some(publication) = &form.publication {
        match dirs.read_link(&publication.link_path)? {
            Some(text) if text == publication.target => {}
            other => {
                return Err(unknown(format!(
                    "{} no longer links to {}; undo changes nothing",
                    publication.link_path.display(),
                    publication.target.display()
                ))
                .with_detail("path", publication.link_path.display().to_string().into())
                .with_detail(
                    "actual_target",
                    other.map(|text| text.display().to_string()).into(),
                ))
            }
        }
    }
    let (snapshot, changes) = observe_adopted_marker(
        state,
        dirs,
        facts,
        mounts,
        record,
        &form.held_root,
        effective_uid,
    )?;
    if request.plan {
        return Ok(undo_report(request, record, changes));
    }
    let mut captured = undo_capture::Captures::new(state, dirs, record, effective_uid);
    let index = captured.marker(&form.held_root, snapshot)?;
    captured.capture_all()?;
    captured.remove(index)?;
    captured.finish()?;
    Ok(UndoReport {
        plan: false,
        placement_id: record.placement_id.clone(),
        kind: record.kind,
        changes,
        removed: true,
    })
}

fn undo_report(
    request: &UndoRequest<'_>,
    record: &PlacementRecord,
    changes: Vec<String>,
) -> UndoReport {
    UndoReport {
        plan: request.plan,
        placement_id: record.placement_id.clone(),
        kind: record.kind,
        changes,
        removed: false,
    }
}

/// The refusal for a settings file that no longer holds the bytes adopt wrote.
fn diverged(path: &Path, expected: &str, actual: &str) -> Error {
    Error::new(
        ErrorKind::UnknownState,
        format!(
            "{} no longer holds the bytes adopt wrote; undo changes nothing",
            path.display()
        ),
    )
    .with_detail("path", path.display().to_string().into())
    .with_detail("expected_digest", expected.into())
    .with_detail("actual_digest", actual.into())
}

// ---- describe rows -------------------------------------------------------

/// Project every stored placement into `describe`'s row shape.
///
/// The link state is observed from the filesystem, never remembered: a setting
/// is `linked` when the file still states the recorded value, a link when its
/// text is the recorded one, and a held root when its recorded identity is still
/// the directory's. A volume that cannot be verified leaves the configured path
/// equal to the recorded one, so `describe` stays usable with the drive away.
pub fn placement_rows(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volumes: &[VolumeRecord],
) -> Result<crate::contract::RowSet<PlacementRow>> {
    let set = read_placements(state)?;
    let mut rows = Vec::new();
    for record in &set.records {
        let configured = configured_path(dirs, facts, mounts, observer, volumes, record);
        let link = link_state(dirs, record);
        rows.push(PlacementRow {
            kind: record.kind.placement_kind(),
            original_path: record.original_path.clone(),
            volume_uuid: record.volume_uuid.as_str().to_owned(),
            recorded_path: record.recorded_path.clone(),
            configured_path: configured,
            link,
            absent: RowAbsentPolicy::Fail,
        });
    }
    let total = set.total.max(rows.len());
    Ok(crate::contract::RowSet::bounded(
        rows,
        total,
        PLACEMENT_ROWS,
    ))
}

/// The path the current configuration would resolve for one record.
fn configured_path(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volumes: &[VolumeRecord],
    record: &PlacementRecord,
) -> PathBuf {
    if matches!(record.form, PlacementForm::Setting(_)) {
        return record.recorded_path.clone();
    }
    let Some(volume) = volumes
        .iter()
        .find(|volume| volume.volume_uuid == record.volume_uuid)
    else {
        return record.recorded_path.clone();
    };
    let Ok(folder) = area::resolve_feature(dirs, facts, mounts, observer, volume, Feature::Caches)
    else {
        return record.recorded_path.clone();
    };
    match record.recorded_path.file_name() {
        Some(name) => folder.path.join(name),
        None => record.recorded_path.clone(),
    }
}

/// Observed publication state for one record.
fn link_state(dirs: &dyn DirOps, record: &PlacementRecord) -> LinkState {
    match &record.form {
        PlacementForm::Setting(form) => {
            let Ok(found) = read_settings(dirs, &form.settings_path) else {
                return LinkState::Unknown;
            };
            let Some((facts, bytes)) = found else {
                return LinkState::Missing;
            };
            match setting_value(&form.key, &bytes, &facts) {
                Ok(Some(value)) if Path::new(&value) == form.data_path => LinkState::Linked,
                Ok(Some(_)) => LinkState::Unlinked,
                Ok(None) => LinkState::Unlinked,
                Err(_) => LinkState::Unknown,
            }
        }
        PlacementForm::Link(form) => match dirs.read_link(&form.link_path) {
            Ok(Some(text)) if text == form.target => LinkState::Linked,
            Ok(Some(_)) => LinkState::Unlinked,
            Ok(None) => LinkState::Missing,
            Err(_) => LinkState::Unknown,
        },
        PlacementForm::HeldRoot(form) => {
            if let Some(publication) = &form.publication {
                match dirs.read_link(&publication.link_path) {
                    Ok(Some(text)) if text == publication.target => {}
                    Ok(Some(_)) | Ok(None) => return LinkState::Unlinked,
                    Err(_) => return LinkState::Unknown,
                }
            }
            match dirs.path_facts(&form.path) {
                Ok(Some(facts))
                    if facts.kind == EntryKind::Directory
                        && facts.device.get() == form.held_root.device
                        && facts.inode == form.held_root.inode =>
                {
                    LinkState::Linked
                }
                Ok(Some(_)) => LinkState::Unlinked,
                Ok(None) => LinkState::Missing,
                Err(_) => LinkState::Unknown,
            }
        }
    }
}

/// The `cache list` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheListReport {
    /// Every recorded placement, bounded to the placement row cap.
    pub placements: crate::contract::RowSet<PlacementRow>,
}

/// List every recorded placement on this account.
///
/// It reads the registry and the placement records and inspects each
/// publication; it creates and changes nothing.
pub fn list(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
) -> Result<CacheListReport> {
    let registry = crate::registry::read(state)?;
    let rows = placement_rows(state, dirs, facts, mounts, observer, registry.volumes())?;
    Ok(CacheListReport { placements: rows })
}

// ---- hex -----------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(unknown(
            "a recorded byte string has an odd number of digits",
        ));
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let raw = text.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        let high = hex_digit(raw[index])?;
        let low = hex_digit(raw[index + 1])?;
        bytes.push(high * 16 + low);
        index += 2;
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(unknown(format!(
            "`{}` is not a hexadecimal digit",
            char::from(other)
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use super::{
        adopt, placement_rows, read_placements, undo, AdoptRequest, CacheKind, FormRequest,
        HeldRootForm, LinkForm, LinkRestore, PlacementForm, UndoRequest,
    };
    use crate::area;
    use crate::contract::ROOT_SCHEMA;
    use crate::describe::LinkState;
    use crate::error::ErrorKind;
    use crate::os::DirOps;
    use crate::state::StatePaths;
    use crate::testkit::{FakeAccount, TempDir, VolumeFixture};

    /// One fixture: a private account home and a registered fixture volume.
    struct Fixture {
        home: TempDir,
        volume: VolumeFixture,
        facts: std::rc::Rc<crate::testkit::FakeVolume>,
        state: StatePaths,
        account: FakeAccount,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let home = TempDir::new(label);
            let volume = VolumeFixture::new(label);
            volume.register();
            let account = FakeAccount {
                home: home.path().to_path_buf(),
            };
            let state = volume.state.clone();
            let facts = volume.facts_shared();
            Self {
                home,
                volume,
                facts,
                state,
                account,
            }
        }

        fn platform(&self) -> crate::os::MacOs {
            crate::os::platform()
        }

        fn request(&self, kind: CacheKind) -> AdoptRequest<'static> {
            // The fixture's volume UUID lives as long as the test.
            AdoptRequest {
                kind,
                volume_uuid: Box::leak(Box::new(self.volume.uuid.clone())),
                form: None,
                write_setting: false,
                adopt_existing_root: false,
                plan: false,
            }
        }

        fn adopt(&self, request: &AdoptRequest<'_>) -> crate::error::Result<super::AdoptReport> {
            adopt(
                &self.state,
                &self.account,
                &self.platform(),
                self.facts.as_ref(),
                &self.volume.mounts(),
                &self.volume.observer(),
                request,
            )
        }

        fn undo(&self, request: &UndoRequest<'_>) -> crate::error::Result<super::UndoReport> {
            self.undo_with(&self.volume.observer(), request)
        }

        fn undo_with(
            &self,
            observer: &dyn crate::os::ProcessObserver,
            request: &UndoRequest<'_>,
        ) -> crate::error::Result<super::UndoReport> {
            undo(
                &self.state,
                &self.platform(),
                self.facts.as_ref(),
                &self.volume.mounts(),
                observer,
                request,
            )
        }

        /// Make the placements directory refuse a write, so a record cannot be
        /// published after an effect has been applied.
        fn deny_record_writes(&self) {
            use std::os::unix::fs::PermissionsExt;
            let dir = self.state.placements_dir();
            std::fs::create_dir_all(&dir).expect("the placements directory");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
                .expect("the directory becomes read-only");
        }

        /// Restore write permission so the fixture can be removed.
        fn allow_record_writes(&self) {
            use std::os::unix::fs::PermissionsExt;
            let dir = self.state.placements_dir();
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.home.write(name, contents.as_bytes());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("the parent exists");
            }
            path
        }

        fn caches(&self) -> PathBuf {
            self.volume.root.join("caches")
        }
    }

    fn facts(path: &Path) -> crate::os::EntryFacts {
        crate::os::platform()
            .path_facts(path)
            .expect("the path can be inspected")
            .expect("the path exists")
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("the file is readable")
    }

    /// Every entry under one directory, recursively, as `(path, bytes)`.
    fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut rows = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let entries = std::fs::read_dir(&directory).expect("the directory lists");
            for entry in entries {
                let entry = entry.expect("the entry reads");
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path).expect("the entry's facts");
                if metadata.is_dir() {
                    rows.push((path.clone(), Vec::new()));
                    pending.push(path);
                } else if metadata.file_type().is_symlink() {
                    let text = std::fs::read_link(&path).expect("the link reads");
                    rows.push((path, text.into_os_string().into_encoded_bytes()));
                } else {
                    rows.push((path.clone(), read(&path)));
                }
            }
        }
        rows.sort();
        rows
    }

    #[test]
    fn a_setting_that_already_points_at_the_owned_root_changes_no_byte() {
        let fixture = Fixture::new("cache-setting-unchanged");
        let caches = fixture.caches();
        std::fs::create_dir_all(caches.join("npm")).expect("the cache directory exists");
        let contents = format!("# npm\ncache={}\nother=1\n", caches.join("npm").display());
        let settings = fixture.write(".npmrc", &contents);
        let before = read(&settings);

        let report = fixture
            .adopt(&fixture.request(CacheKind::NpmCache))
            .expect("an already-pointing setting is adopted");
        assert!(!report.plan);
        let record = report.placement.expect("a record is written");
        match &record.form {
            PlacementForm::Setting(form) => {
                assert!(!form.changed, "no byte was written");
                assert_eq!(form.data_path, caches.join("npm"));
                assert!(!form.created_directory);
            }
            other => panic!("expected the setting form, got {}", other.name()),
        }
        assert_eq!(read(&settings), before, "the settings file is untouched");
        assert!(
            !settings.with_file_name(".npmrc.barge-backup").exists(),
            "no backup is written when nothing changes"
        );

        let undo = fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::NpmCache),
                plan: false,
            })
            .expect("undo reverses the record");
        assert!(undo.removed);
        assert_eq!(
            read(&settings),
            before,
            "undo changes no byte of an unchanged setting"
        );
        assert!(read_placements(&fixture.state)
            .expect("the store reads")
            .records
            .is_empty());
    }

    #[test]
    fn a_setting_is_written_only_by_an_explicit_adopt_and_undo_restores_it() {
        let fixture = Fixture::new("cache-setting-write");
        let original = "cache=/Users/someone/.npm\nregistry=https://example.invalid\n";
        let settings = fixture.write(".npmrc", original);

        // Without the explicit flag, adopt names both values and changes nothing.
        let error = fixture
            .adopt(&fixture.request(CacheKind::NpmCache))
            .expect_err("a differing setting is refused without the explicit adopt");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert_eq!(error.detail()["current_value"], "/Users/someone/.npm");
        assert_eq!(read(&settings), original.as_bytes());
        assert!(
            fixture
                .adopt(&fixture.request(CacheKind::NpmCache))
                .is_err(),
            "still nothing was adopted"
        );

        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        let report = fixture
            .adopt(&request)
            .expect("the explicit adopt writes the setting");
        let record = report.placement.expect("a record is written");
        let before = match &record.form {
            PlacementForm::Setting(form) => {
                assert!(form.changed);
                assert_eq!(
                    form.before.as_ref().expect("before").bytes.len(),
                    original.len() * 2
                );
                form.before.clone().expect("before")
            }
            other => panic!("expected the setting form, got {}", other.name()),
        };
        let written = String::from_utf8(read(&settings)).expect("the settings are text");
        assert!(written.contains(&format!("cache={}", fixture.caches().join("npm").display())));
        assert!(written.contains("registry=https://example.invalid"));
        assert!(
            fixture.caches().join("npm").is_dir(),
            "the cache directory is created"
        );
        assert!(
            report.changes.iter().any(|change| change.contains("cache")),
            "the exact change is reported: {:?}",
            report.changes
        );

        // The record's after digest is the file as adopt left it.
        let after_bytes = read(&settings);
        match &record.form {
            PlacementForm::Setting(form) => {
                assert_eq!(
                    form.after.digest,
                    crate::digest::sha256_hex(&after_bytes),
                    "the record holds the bytes on disk"
                );
            }
            other => panic!("expected the setting form, got {}", other.name()),
        }

        fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect("undo restores the setting");
        assert_eq!(
            read(&settings),
            before_bytes(&before),
            "undo restores the bytes"
        );
    }

    fn before_bytes(snapshot: &super::SettingsSnapshot) -> Vec<u8> {
        super::decode_hex(&snapshot.bytes).expect("the recorded bytes decode")
    }

    #[test]
    fn undo_refuses_after_the_settings_file_diverged() {
        let fixture = Fixture::new("cache-setting-diverged");
        let settings = fixture.write(".npmrc", "cache=/old\n");
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        let record = fixture
            .adopt(&request)
            .expect("the setting is adopted")
            .placement
            .expect("a record is written");
        let after = read(&settings);

        std::fs::write(&settings, "cache=/someone/else\n").expect("the file diverges");
        let error = fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect_err("a diverged file refuses undo");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["path"], settings.display().to_string());
        assert_eq!(
            read(&settings),
            b"cache=/someone/else\n",
            "the diverged file is left exactly as it is"
        );
        assert_ne!(read(&settings), after);
        assert_eq!(
            read_placements(&fixture.state)
                .expect("the store reads")
                .records
                .len(),
            1,
            "the record stays so a human can decide"
        );
    }

    #[test]
    fn a_non_empty_real_directory_refuses_with_a_plan_and_changes_nothing() {
        let fixture = Fixture::new("cache-non-empty");
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(&original).expect("the directory exists");
        std::fs::write(original.join("payload"), b"cache data").expect("the payload");
        let before = tree(&original);

        let error = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect_err("a non-empty directory is never replaced by a link");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert_eq!(error.detail()["entries"], 1);
        assert_eq!(error.detail()["changed"], false);
        assert!(
            error.detail()["plan"].is_array(),
            "the refusal carries a plan: {:?}",
            error.detail()
        );
        assert_eq!(tree(&original), before, "nothing was renamed or deleted");
        assert!(!original.is_symlink());
        assert_eq!(
            read_placements(&fixture.state)
                .expect("the store reads")
                .records
                .len(),
            0,
            "nothing was recorded"
        );

        // A plan reports the same refusal and changes nothing either.
        let mut plan = fixture.request(CacheKind::CargoRegistry);
        plan.plan = true;
        assert!(fixture.adopt(&plan).is_err());
        assert_eq!(tree(&original), before);
    }

    #[test]
    fn a_matching_link_that_races_publication_is_refused_and_kept() {
        let fixture = Fixture::new("cache-link-publication-race");
        let public = fixture.home.path().join("public");
        std::fs::create_dir(&public).unwrap();
        let original = public.join("original");
        let target = public.join("payload");
        std::fs::write(&target, b"cache data").unwrap();
        let intent = super::AdoptIntent {
            schema: "barge.cache-adopt/1".to_owned(),
            id: "ab".repeat(16),
            kind: CacheKind::CargoRegistry,
            volume: fixture.volume.record.clone(),
            original: original.clone(),
            before: super::OriginalState::Absent,
            expected_setting: None,
            setting_after: None,
            target: Some(target.clone()),
            marker_before: None,
            marker_plan: None,
            held: None,
            candidate: None,
            link: Some(LinkForm {
                link_path: original.clone(),
                target: target.clone(),
                restore: LinkRestore::Absent,
                original_mode: None,
                published_identity: None,
                root_id: "cd".repeat(16),
                marker_digest: "ef".repeat(32),
            }),
        };
        super::begin_adopt_intent(&fixture.state, &intent).unwrap();
        let dirs = crate::testkit::Faults::new();
        *dirs.occupy_link_path.borrow_mut() = Some((original.clone(), target.clone()));
        let error = super::publish_link(
            &fixture.state,
            CacheKind::CargoRegistry,
            &dirs,
            &original,
            &target,
        )
        .expect_err(
            "an existing target is never a successful publication, even with matching text",
        );
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert_eq!(std::fs::read_link(&original).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"cache data");
        assert_eq!(
            std::fs::read_dir(&public).unwrap().count(),
            2,
            "only the racer link and payload remain"
        );
    }

    #[test]
    fn an_absent_path_gets_a_link_through_a_temporary_name() {
        let fixture = Fixture::new("cache-absent");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let original = fixture.home.path().join(".cargo/registry");
        assert!(!original.exists());

        let report = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect("an absent path is linked");
        let record = report.placement.expect("a record is written");
        let target = match &record.form {
            PlacementForm::Link(form) => {
                assert_eq!(form.restore, LinkRestore::Absent);
                form.target.clone()
            }
            other => panic!("expected the link form, got {}", other.name()),
        };
        assert!(target.starts_with(fixture.caches()));
        assert!(target.is_dir(), "the owned root exists");
        assert_eq!(
            fixture
                .platform()
                .read_link(&original)
                .expect("the link reads"),
            Some(target.clone())
        );
        let names: Vec<String> = std::fs::read_dir(fixture.home.path().join(".cargo"))
            .expect("the parent lists")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["registry".to_owned()],
            "no temporary name is left"
        );
        let marker = target.join(area::ROOT_MARKER_FILE);
        assert!(marker.is_file(), "the owned root carries its marker");
        assert!(String::from_utf8(read(&marker))
            .expect("the marker is text")
            .contains(ROOT_SCHEMA));

        fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect("undo removes the link");
        assert!(!original.exists(), "the path is absent again");
        assert!(
            target.is_dir(),
            "undo never deletes the cache's data or its root"
        );
    }

    #[test]
    fn an_empty_real_directory_becomes_a_link_and_undo_restores_it_empty() {
        let fixture = Fixture::new("cache-empty-dir");
        let original = fixture.home.path().join(".cargo/git");
        std::fs::create_dir_all(&original).expect("the empty directory exists");
        let record = fixture
            .adopt(&fixture.request(CacheKind::CargoGit))
            .expect("an empty directory is replaced by a link")
            .placement
            .expect("a record is written");
        match &record.form {
            PlacementForm::Link(form) => assert_eq!(form.restore, LinkRestore::EmptyDirectory),
            other => panic!("expected the link form, got {}", other.name()),
        }
        assert!(original.is_symlink());
        assert_eq!(facts(&original).kind, crate::os::EntryKind::Symlink);

        fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect("undo restores the empty directory");
        let restored = facts(&original);
        assert_eq!(restored.kind, crate::os::EntryKind::Directory);
        assert_eq!(
            std::fs::read_dir(&original).expect("it lists").count(),
            0,
            "the restored directory is empty"
        );
    }

    #[test]
    fn an_empty_non_empty_race_refuses_rather_than_removing_entries() {
        // A directory that fills after it was classified is refused by the
        // empty-directory proof, so its contents are never removed.
        let fixture = Fixture::new("cache-empty-race");
        let original = fixture.home.path().join(".cargo/git");
        std::fs::create_dir_all(&original).expect("the directory exists");
        let platform = fixture.platform();
        let facts = platform
            .path_facts(&original)
            .expect("the facts read")
            .expect("it exists");
        std::fs::write(original.join("appeared"), b"data").expect("an entry appears");
        let error = super::confirm_empty_directory(
            &platform,
            &fixture.volume.observer(),
            &original,
            &facts,
        )
        .expect_err("a directory that is no longer empty is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(original.join("appeared").is_file(), "the entry is kept");
    }

    #[test]
    fn a_symlinked_original_is_recorded_not_swapped() {
        let fixture = Fixture::new("cache-symlink");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).expect("the hand-made target exists");
        std::fs::write(target.join("payload"), b"registry data").expect("the payload");
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        std::os::unix::fs::symlink(&target, &original).expect("the hand-made link exists");
        let before_target = facts(&target);
        let before_tree = tree(&target);

        // Without the explicit in-place adopt, nothing is recorded and the plan
        // says what would be done.
        let error = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect_err("an unmarked target is not adopted silently");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(error.detail()["plan"].is_array());
        assert_eq!(
            fixture
                .platform()
                .read_link(&original)
                .expect("the link reads"),
            Some(target.clone())
        );

        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        let effect = fixture
            .adopt(&request)
            .expect("the existing link and its target are recorded in place")
            .placement
            .expect("a record is written");
        match &effect.form {
            PlacementForm::HeldRoot(form) => {
                assert_eq!(form.path, target);
                let publication = form.publication.as_ref().expect("the link is recorded");
                assert_eq!(publication.link_path, original);
                assert_eq!(publication.target, target);
            }
            other => panic!("expected the held-root form, got {}", other.name()),
        }
        assert_eq!(
            fixture
                .platform()
                .read_link(&original)
                .expect("the link reads"),
            Some(target.clone()),
            "the link was never swapped"
        );
        assert_eq!(
            facts(&target).inode,
            before_target.inode,
            "the adopted directory is the same directory"
        );
        let after_tree: Vec<_> = tree(&target)
            .into_iter()
            .filter(|(path, _)| {
                path.file_name().and_then(std::ffi::OsStr::to_str) != Some(area::ROOT_MARKER_FILE)
            })
            .collect();
        assert_eq!(after_tree, before_tree, "nothing in the data moved");
        assert!(target.join(area::ROOT_MARKER_FILE).is_file());

        fixture
            .undo(&UndoRequest {
                placement_id: Some(&effect.placement_id),
                kind: None,
                plan: false,
            })
            .expect("undo removes only the marker");
        assert!(
            !target.join(area::ROOT_MARKER_FILE).exists(),
            "the marker is gone"
        );
        assert_eq!(
            fixture
                .platform()
                .read_link(&original)
                .expect("the link reads"),
            Some(target.clone()),
            "the pre-existing link stays"
        );
        assert_eq!(
            tree(&target),
            before_tree,
            "the cache data is untouched by undo"
        );
    }

    #[test]
    fn a_hand_made_placement_layout_is_adopted_in_place_moving_nothing() {
        // Today's layout: `~/.cargo/{registry,git}` are links into
        // `/Volumes/Builds/caches/cargo/`, and the npm cache is named by a
        // setting. Adopting each moves nothing.
        let fixture = Fixture::new("cache-hand-made");
        let cargo = fixture.caches().join("cargo");
        std::fs::create_dir_all(cargo.join("registry")).expect("registry");
        std::fs::create_dir_all(cargo.join("git")).expect("git");
        std::fs::write(cargo.join("registry").join("index"), b"index").expect("index");
        let npm = fixture.caches().join("npm");
        std::fs::create_dir_all(&npm).expect("npm");
        std::fs::write(npm.join("_cacache"), b"content").expect("content");
        let settings = fixture.write(".npmrc", &format!("cache={}\n", npm.display()));
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        std::os::unix::fs::symlink(
            cargo.join("registry"),
            fixture.home.path().join(".cargo/registry"),
        )
        .expect("registry link");
        std::os::unix::fs::symlink(cargo.join("git"), fixture.home.path().join(".cargo/git"))
            .expect("git link");

        let before_volume = tree(&fixture.caches());
        let before_settings = read(&settings);

        let mut npm_request = fixture.request(CacheKind::NpmCache);
        npm_request.adopt_existing_root = true;
        fixture.adopt(&npm_request).expect("npm is adopted");
        let mut registry_request = fixture.request(CacheKind::CargoRegistry);
        registry_request.adopt_existing_root = true;
        fixture
            .adopt(&registry_request)
            .expect("registry is adopted");
        let mut git_request = fixture.request(CacheKind::CargoGit);
        git_request.adopt_existing_root = true;
        fixture.adopt(&git_request).expect("git is adopted");

        assert_eq!(read(&settings), before_settings, "the setting is untouched");
        let marker_files = before_volume.len();
        let after_volume = tree(&fixture.caches());
        // The only difference is one marker per adopted directory.
        let added: Vec<PathBuf> = after_volume
            .iter()
            .filter(|(path, _)| !before_volume.iter().any(|(before, _)| before == path))
            .map(|(path, _)| path.clone())
            .collect();
        assert_eq!(
            added.len(),
            3,
            "one marker per adopted directory: {added:?}"
        );
        for path in &added {
            assert_eq!(
                path.file_name().and_then(std::ffi::OsStr::to_str),
                Some(area::ROOT_MARKER_FILE)
            );
        }
        assert_eq!(after_volume.len(), marker_files + 3);
        for (path, bytes) in &before_volume {
            let found = after_volume
                .iter()
                .find(|(after, _)| after == path)
                .unwrap_or_else(|| panic!("{} disappeared", path.display()));
            assert_eq!(&found.1, bytes, "{} was rewritten", path.display());
        }
        for name in [".cargo/registry", ".cargo/git"] {
            let path = fixture.home.path().join(name);
            assert!(path.is_symlink(), "{name} is still a link");
        }
        let rows = placement_rows(
            &fixture.state,
            &fixture.platform(),
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            std::slice::from_ref(&fixture.volume.record),
        )
        .expect("the rows are built");
        assert_eq!(rows.count, 3);
        for row in rows.rows {
            assert_eq!(row.link, LinkState::Linked);
            assert_eq!(row.absent, crate::describe::AbsentPolicy::Fail);
            assert!(row.recorded_path.starts_with(fixture.caches()));
        }
    }

    #[test]
    fn a_setting_placement_adopts_its_directory_in_place_and_undo_removes_only_the_marker() {
        let fixture = Fixture::new("cache-setting-held");
        let npm = fixture.caches().join("npm");
        std::fs::create_dir_all(&npm).expect("the cache directory exists");
        std::fs::write(npm.join("_cacache"), b"content").expect("cache content");
        let contents = format!("cache={}\n", npm.display());
        let settings = fixture.write(".npmrc", &contents);
        let before_tree = tree(&npm);

        let mut request = fixture.request(CacheKind::NpmCache);
        request.adopt_existing_root = true;
        let record = fixture
            .adopt(&request)
            .expect("the setting and its directory are adopted")
            .placement
            .expect("a record is written");
        match &record.form {
            PlacementForm::Setting(form) => {
                let held = form.held_root.as_ref().expect("the directory is adopted");
                assert_eq!(held.path, npm);
            }
            other => panic!("expected the setting form, got {}", other.name()),
        }
        assert_eq!(read(&settings), contents.as_bytes(), "no byte was written");
        assert!(npm.join(area::ROOT_MARKER_FILE).is_file());

        fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect("undo reverses both parts");
        assert!(
            !npm.join(area::ROOT_MARKER_FILE).exists(),
            "the marker is gone"
        );
        assert_eq!(read(&settings), contents.as_bytes());
        let after_tree: Vec<_> = tree(&npm)
            .into_iter()
            .filter(|(path, _)| {
                path.file_name().and_then(std::ffi::OsStr::to_str) != Some(area::ROOT_MARKER_FILE)
            })
            .collect();
        assert_eq!(after_tree, before_tree, "the cache data is untouched");
    }

    #[test]
    fn a_setting_plan_reports_the_write_without_writing_it() {
        let fixture = Fixture::new("cache-setting-plan");
        let original = "cache=/old\n";
        let settings = fixture.write(".npmrc", original);
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        request.plan = true;
        let report = fixture.adopt(&request).expect("the plan is produced");
        assert!(report.plan);
        assert!(report.placement.is_none());
        assert!(
            report.changes.iter().any(|change| change.contains("cache")),
            "{:?}",
            report.changes
        );
        assert_eq!(read(&settings), original.as_bytes());
        assert!(!fixture.caches().exists());
        assert!(read_placements(&fixture.state)
            .expect("the store reads")
            .records
            .is_empty());
    }

    #[test]
    fn undo_link_requires_safe_custody_before_removing_the_link() {
        let fixture = Fixture::new("cache-link-custody");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let record = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect("the absent path is linked")
            .placement
            .expect("a record is written");
        let original = fixture.home.path().join(".cargo/registry");

        // An open descriptor under the owned root refuses the removal.
        let mut observer = fixture.volume.observer();
        observer.scan = crate::testkit::ScanMode::Open(vec![crate::os::OpenDescriptor {
            pid: std::process::id(),
            descriptor: 9,
            path: match &record.form {
                PlacementForm::Link(form) => form.target.clone(),
                other => panic!("expected the link form, got {}", other.name()),
            },
        }]);
        let error = fixture
            .undo_with(
                &observer,
                &UndoRequest {
                    placement_id: Some(&record.placement_id),
                    kind: None,
                    plan: false,
                },
            )
            .expect_err("an open descriptor refuses the link removal");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
        assert!(original.is_symlink(), "the link is still there");
        assert_eq!(
            read_placements(&fixture.state)
                .expect("the store reads")
                .records
                .len(),
            1,
            "the record stays"
        );

        // A bounded scan proves nothing either.
        let mut bounded = fixture.volume.observer();
        bounded.scan = crate::testkit::ScanMode::Bounded;
        let error = fixture
            .undo_with(
                &bounded,
                &UndoRequest {
                    placement_id: Some(&record.placement_id),
                    kind: None,
                    plan: false,
                },
            )
            .expect_err("a bounded scan refuses the link removal");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
        assert!(original.is_symlink());

        // With safe custody the same undo succeeds.
        fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect("safe custody authorizes the link removal");
        assert!(!original.exists());
    }

    #[test]
    fn a_live_hold_refuses_the_link_removal() {
        let fixture = Fixture::new("cache-link-hold");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let record = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect("the absent path is linked")
            .placement
            .expect("a record is written");
        let root_id = match &record.form {
            PlacementForm::Link(form) => form.root_id.clone(),
            other => panic!("expected the link form, got {}", other.name()),
        };
        let pid = std::process::id();
        let bytes = crate::testkit::VolumeFixture::hold_record(
            &root_id,
            "reviewer",
            pid,
            7_000_000,
            crate::testkit::FIXTURE_BOOT,
        );
        let holds = fixture.state.holds_dir().join(&root_id);
        std::fs::create_dir_all(&holds).expect("the hold directory");
        std::fs::write(holds.join("reviewer.json"), bytes).expect("the hold is written");

        let error = fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect_err("a live holder refuses the link removal");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
        assert!(
            fixture.home.path().join(".cargo/registry").is_symlink(),
            "the link is still there"
        );
    }

    #[test]
    fn a_link_adopt_whose_record_cannot_be_published_rolls_the_link_back() {
        let fixture = Fixture::new("cache-link-rollback");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let original = fixture.home.path().join(".cargo/registry");
        fixture.deny_record_writes();
        let error = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect_err("an unpublished record rolls the link back");
        fixture.allow_record_writes();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(
            error.detail()["rolled_back"].is_array(),
            "the rollback is reported: {:?}",
            error.detail()
        );
        assert!(!original.exists(), "the link was removed again");
    }

    #[test]
    fn an_empty_directory_link_whose_record_cannot_be_published_restores_the_directory() {
        let fixture = Fixture::new("cache-link-rollback-empty");
        let original = fixture.home.path().join(".cargo/git");
        std::fs::create_dir_all(&original).expect("the empty directory exists");
        fixture.deny_record_writes();
        let error = fixture
            .adopt(&fixture.request(CacheKind::CargoGit))
            .expect_err("an unpublished record restores the empty directory");
        fixture.allow_record_writes();
        assert_eq!(error.kind(), ErrorKind::Io);
        let restored = facts(&original);
        assert_eq!(restored.kind, crate::os::EntryKind::Directory);
        assert_eq!(std::fs::read_dir(&original).expect("it lists").count(), 0);
    }

    #[test]
    fn a_held_root_whose_record_cannot_be_published_removes_its_marker() {
        let fixture = Fixture::new("cache-held-rollback");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).expect("the target exists");
        std::fs::write(target.join("payload"), b"registry data").expect("the payload");
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        std::os::unix::fs::symlink(&target, &original).expect("the hand-made link");
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        fixture.deny_record_writes();
        let error = fixture
            .adopt(&request)
            .expect_err("an unpublished record removes the marker again");
        fixture.allow_record_writes();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(
            error.detail()["rolled_back"].is_array(),
            "{:?}",
            error.detail()
        );
        assert!(
            !target.join(area::ROOT_MARKER_FILE).exists(),
            "the marker was removed again"
        );
        assert_eq!(
            std::fs::read(target.join("payload")).expect("the payload"),
            b"registry data"
        );
        assert!(
            fixture
                .platform()
                .read_link(&original)
                .expect("the link reads")
                .is_some(),
            "the pre-existing link is untouched"
        );
    }

    #[test]
    fn a_written_setting_whose_record_cannot_be_published_is_restored() {
        let fixture = Fixture::new("cache-setting-rollback");
        let original = "cache=/old\nregistry=https://example.invalid\n";
        let settings = fixture.write(".npmrc", original);
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        request.adopt_existing_root = true;
        fixture.deny_record_writes();
        let error = fixture
            .adopt(&request)
            .expect_err("an unpublished record restores the settings file");
        fixture.allow_record_writes();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            read(&settings),
            original.as_bytes(),
            "the settings file holds its pre-adopt bytes again"
        );
        assert!(
            !fixture
                .caches()
                .join("npm")
                .join(area::ROOT_MARKER_FILE)
                .exists(),
            "the adopted marker was removed again"
        );
    }

    #[test]
    fn a_plan_changes_nothing() {
        let fixture = Fixture::new("cache-plan");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.plan = true;
        let report = fixture.adopt(&request).expect("the plan is produced");
        assert!(report.plan);
        assert!(report.placement.is_none());
        assert!(!report.changes.is_empty());
        assert!(!fixture.caches().exists(), "no folder was created");
        assert!(!fixture.home.path().join(".cargo/registry").exists());
        assert!(read_placements(&fixture.state)
            .expect("the store reads")
            .records
            .is_empty());
    }

    #[test]
    fn one_kind_is_adopted_once() {
        let fixture = Fixture::new("cache-once");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect("the first adopt succeeds");
        let error = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect_err("a second adopt of one kind refuses");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        // A plan refuses too: the placement already exists.
        let mut plan = fixture.request(CacheKind::CargoRegistry);
        plan.plan = true;
        assert!(fixture.adopt(&plan).is_err());
    }

    #[test]
    fn an_absent_volume_names_the_drive_and_changes_nothing() {
        let fixture = Fixture::new("cache-absent-volume");
        fixture.facts.present.set(false);
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let error = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect_err("an absent volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
        assert_eq!(
            error.detail()["attach"],
            fixture.volume.root.display().to_string()
        );
        assert!(
            error
                .message()
                .contains(&fixture.volume.root.display().to_string()),
            "the message names the drive: {}",
            error.message()
        );
        assert!(!fixture.home.path().join(".cargo/registry").exists());
        assert!(!fixture.caches().exists());
    }

    #[test]
    fn a_bun_section_is_written_and_undone_byte_for_byte() {
        let fixture = Fixture::new("cache-bun");
        let original =
            "logLevel = \"warn\"\n[install]\ncache = true\n\n[install.cache]\ndir = \"/old/bun\"\n";
        let settings = fixture.write(".bunfig.toml", original);
        let mut request = fixture.request(CacheKind::BunCache);
        request.write_setting = true;
        let record = fixture
            .adopt(&request)
            .expect("the bun setting is written")
            .placement
            .expect("a record is written");
        let written = String::from_utf8(read(&settings)).expect("the settings are text");
        assert!(written.contains(&format!(
            "dir = \"{}\"",
            fixture.caches().join("bun").display()
        )));
        assert!(!written.contains("/old/bun"));
        assert!(
            written.contains("cache = true"),
            "other tables are untouched"
        );
        fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect("undo restores the bun setting");
        assert_eq!(
            String::from_utf8(read(&settings)).expect("the settings are text"),
            original
        );
    }

    #[test]
    fn a_setting_with_more_than_one_candidate_key_is_not_edited() {
        let fixture = Fixture::new("cache-two-keys");
        let original = "cache=/one\ncache=/two\n";
        let settings = fixture.write(".npmrc", original);
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        let error = fixture
            .adopt(&request)
            .expect_err("a file with two candidate keys is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(read(&settings), original.as_bytes());
        assert_eq!(
            read_placements(&fixture.state)
                .expect("the store reads")
                .records
                .len(),
            0
        );
    }

    #[test]
    fn a_link_that_points_outside_the_volume_is_refused_with_both_values() {
        let fixture = Fixture::new("cache-foreign-link");
        let elsewhere = fixture.home.path().join("elsewhere-cache");
        std::fs::create_dir_all(&elsewhere).expect("the foreign directory exists");
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        std::os::unix::fs::symlink(&elsewhere, &original).expect("the foreign link");
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        let error = fixture
            .adopt(&request)
            .expect_err("a foreign link target is not adopted");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert_eq!(
            error.detail()["current_value"],
            elsewhere.display().to_string()
        );
        assert_eq!(
            fixture
                .platform()
                .read_link(&original)
                .expect("the link reads"),
            Some(elsewhere)
        );
        assert!(read_placements(&fixture.state)
            .expect("the store reads")
            .records
            .is_empty());
    }

    #[test]
    fn a_held_root_undo_refuses_when_the_directory_changed() {
        let fixture = Fixture::new("cache-held-changed");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).expect("the target exists");
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        std::os::unix::fs::symlink(&target, &original).expect("the hand-made link");
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        let record = fixture
            .adopt(&request)
            .expect("the target is adopted in place")
            .placement
            .expect("a record is written");

        // Replace the directory with another one at the same path.
        std::fs::remove_file(target.join(area::ROOT_MARKER_FILE)).expect("the marker is removed");
        std::fs::remove_dir(&target).expect("the empty directory is removed");
        std::fs::create_dir(&target).expect("a different directory appears");

        let error = fixture
            .undo(&UndoRequest {
                placement_id: Some(&record.placement_id),
                kind: None,
                plan: false,
            })
            .expect_err("a changed directory refuses undo");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            read_placements(&fixture.state)
                .expect("the store reads")
                .records
                .len(),
            1,
            "the record stays"
        );
        assert!(
            !target.join(area::ROOT_MARKER_FILE).exists(),
            "undo wrote nothing"
        );
    }

    #[test]
    fn the_record_shape_is_the_contract_it_names() {
        let fixture = Fixture::new("cache-record-shape");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let record = fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .expect("the adopt succeeds")
            .placement
            .expect("a record is written");
        let path = super::placement_path(&fixture.state, &record.placement_id).expect("a path");
        let document: serde_json::Value =
            serde_json::from_slice(&read(&path)).expect("the record is JSON");
        assert_eq!(document["schema"], "barge.placement/1");
        assert_eq!(document["kind"], "cargo_registry");
        assert_eq!(document["form"]["form"], "link");
        assert_eq!(document["generation"], 1);
        let facts = crate::os::platform()
            .path_facts(&path)
            .expect("the record's facts")
            .expect("it exists");
        assert!(facts.is_private(), "a record is private");
        assert_eq!(facts.kind, crate::os::EntryKind::File);
        // A record with an unknown field is refused, not defaulted.
        let mut value: serde_json::Value = document.clone();
        value["extra"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_vec(&value).expect("it encodes"))
            .expect("the record is rewritten");
        let error = read_placements(&fixture.state).expect_err("an unknown field is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn a_setting_form_requires_the_setting_a_kind_has() {
        let fixture = Fixture::new("cache-form-mismatch");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.form = Some(FormRequest::Setting);
        let error = fixture
            .adopt(&request)
            .expect_err("a Cargo cache is not published by a setting");
        assert_eq!(error.kind(), ErrorKind::Usage);
        // And a form the state cannot support is refused with a plan.
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.form = Some(FormRequest::HeldRoot);
        let error = fixture
            .adopt(&request)
            .expect_err("an absent path cannot be adopted in place");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(error.detail()["plan"].is_array());
    }

    #[test]
    fn the_held_root_form_records_the_kind_it_serves() {
        let fixture = Fixture::new("cache-held-form");
        let target = fixture.caches().join("cargo/git");
        std::fs::create_dir_all(&target).expect("the target exists");
        let original = fixture.home.path().join(".cargo/git");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).expect("the parent exists");
        std::os::unix::fs::symlink(&target, &original).expect("the hand-made link");
        let mut request = fixture.request(CacheKind::CargoGit);
        request.adopt_existing_root = true;
        let record = fixture
            .adopt(&request)
            .expect("the target is adopted")
            .placement
            .expect("a record is written");
        match &record.form {
            PlacementForm::HeldRoot(HeldRootForm { held_root, .. }) => {
                assert_eq!(held_root.path, target);
                assert_eq!(held_root.device, facts(&target).device.get());
                assert_eq!(held_root.inode, facts(&target).inode);
                assert_eq!(
                    held_root.observed.entries, 0,
                    "the observation is taken before the marker is written"
                );
            }
            PlacementForm::Link(LinkForm { .. }) => panic!("expected the held-root form"),
            PlacementForm::Setting(_) => panic!("expected the held-root form"),
        }
    }
    #[test]
    fn in_place_marker_publication_preserves_a_raced_entry_and_recovery_evidence() {
        let fixture = Fixture::new("cache-marker-exclusive-publication");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("payload"), b"cache data").unwrap();
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &original).unwrap();
        let marker = target.join(area::ROOT_MARKER_FILE);
        let raced = marker.clone();
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = saved.clone();
        let dirs = crate::testkit::Faults::new();
        *dirs.publication_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == area::ROOT_MARKER_FILE {
                std::fs::write(&raced, b"concurrent marker, keep exactly").unwrap();
                *observed.borrow_mut() = Some(facts(&raced));
            }
        }));
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        let error = adopt(
            &fixture.state,
            &fixture.account,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &request,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert_eq!(read(&marker), b"concurrent marker, keep exactly");
        assert_eq!(Some(facts(&marker)), *saved.borrow());
        assert_eq!(read(&target.join("payload")), b"cache data");
        assert_eq!(std::fs::read_link(&original).unwrap(), target);
        let pending = super::read_adopt_intent(&fixture.state, CacheKind::CargoRegistry)
            .unwrap()
            .unwrap();
        assert!(pending.held.is_none() && pending.candidate.is_none());
        let plan = pending.marker_plan.unwrap();
        let staged = target.join(format!(".barge-marker-{}", plan.root_id));
        let document: serde_json::Value = serde_json::from_slice(&read(&staged)).unwrap();
        assert_eq!(document["root_id"], plan.root_id);
        assert_eq!(
            error.detail()["staged_marker"],
            staged.display().to_string()
        );
        assert!(read_placements(&fixture.state).unwrap().records.is_empty());
        assert!(fixture.adopt(&request).is_err());
        assert!(fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoRegistry),
                plan: false
            })
            .is_err());
        assert_eq!(read(&marker), b"concurrent marker, keep exactly");
        assert_eq!(Some(facts(&marker)), *saved.borrow());
        assert!(staged.is_file(), "pending staging evidence is kept");
    }

    #[test]
    fn old_replacing_marker_publish_overwrites_the_same_boundary_race() {
        let fixture = Fixture::new("cache-marker-replacing-counterfactual");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).unwrap();
        let marker = target.join(area::ROOT_MARKER_FILE);
        let parent = fixture.platform().open_path(&target).unwrap();
        assert!(fixture
            .platform()
            .child_facts(&parent, area::ROOT_MARKER_FILE)
            .unwrap()
            .is_none());
        let raced = marker.clone();
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = saved.clone();
        let dirs = crate::testkit::Faults::new();
        *dirs.replacing_publication_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == area::ROOT_MARKER_FILE {
                std::fs::write(&raced, b"concurrent marker, keep exactly").unwrap();
                *observed.borrow_mut() = Some(facts(&raced));
            }
        }));
        // The old area::write_marker publication, after its absence observation.
        dirs.publish_child_file(
            &parent,
            area::ROOT_MARKER_FILE,
            b"old writer overwrote the marker",
            0o600,
        )
        .unwrap();
        assert_eq!(read(&marker), b"old writer overwrote the marker");
        assert_ne!(Some(facts(&marker)), *saved.borrow());
    }

    #[test]
    fn adoption_capture_keeps_a_concurrent_settings_edit_and_pending_evidence() {
        let fixture = Fixture::new("cache-adopt-setting-capture-edit");
        let settings = fixture.write(".npmrc", "cache=/old\n");
        let raced = settings.clone();
        let dirs = crate::testkit::Faults::new();
        *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == ".npmrc" {
                std::fs::write(&raced, b"cache=/concurrent\n").unwrap();
            }
        }));
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        assert!(adopt(
            &fixture.state,
            &fixture.account,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &request
        )
        .is_err());
        assert_eq!(read(&settings), b"cache=/concurrent\n");
        assert!(
            super::read_adopt_intent(&fixture.state, CacheKind::NpmCache)
                .unwrap()
                .is_some()
        );
        assert!(super::undo_capture::journal_path(&fixture.state, CacheKind::NpmCache).is_file());
    }

    #[test]
    fn adoption_exclusive_publication_never_overwrites_a_taken_settings_name() {
        for existing in [false, true] {
            let fixture = Fixture::new("cache-adopt-setting-publication-race");
            let settings = fixture.home.path().join(".npmrc");
            if existing {
                std::fs::write(&settings, b"cache=/old\n").unwrap();
            }
            let raced = settings.clone();
            let dirs = crate::testkit::Faults::new();
            *dirs.publication_probe.borrow_mut() = Some(Box::new(move |name| {
                if name == ".npmrc" {
                    std::fs::write(&raced, b"cache=/concurrent\n").unwrap();
                }
            }));
            let mut request = fixture.request(CacheKind::NpmCache);
            request.write_setting = true;
            assert!(adopt(
                &fixture.state,
                &fixture.account,
                &dirs,
                fixture.facts.as_ref(),
                &fixture.volume.mounts(),
                &fixture.volume.observer(),
                &request
            )
            .is_err());
            assert_eq!(read(&settings), b"cache=/concurrent\n");
            let journal = std::fs::read(super::undo_capture::journal_path(
                &fixture.state,
                CacheKind::NpmCache,
            ))
            .unwrap();
            let journal: serde_json::Value = serde_json::from_slice(&journal).unwrap();
            if existing {
                let private = journal["effects"][0]["private"].as_str().unwrap();
                assert_eq!(read(&fixture.home.path().join(private)), b"cache=/old\n");
            }
            assert!(
                super::read_adopt_intent(&fixture.state, CacheKind::NpmCache)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn failed_adoption_settings_rollback_keeps_replacement_and_marker() {
        let fixture = Fixture::new("cache-setting-rollback-replacement");
        let settings = fixture.write(".npmrc", "cache=/old\n");
        let raced = settings.clone();
        let calls = std::cell::Cell::new(0usize);
        let dirs = crate::testkit::Faults::new();
        *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == ".npmrc" {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    std::fs::write(&raced, b"cache=/concurrent\n").unwrap();
                }
            }
        }));
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        request.adopt_existing_root = true;
        fixture.deny_record_writes();
        assert!(adopt(
            &fixture.state,
            &fixture.account,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &request
        )
        .is_err());
        fixture.allow_record_writes();
        assert_eq!(read(&settings), b"cache=/concurrent\n");
        assert!(fixture
            .caches()
            .join("npm")
            .join(area::ROOT_MARKER_FILE)
            .is_file());
        assert!(
            super::read_adopt_intent(&fixture.state, CacheKind::NpmCache)
                .unwrap()
                .is_some()
        );
        assert!(super::undo_capture::journal_path(&fixture.state, CacheKind::NpmCache).is_file());
    }

    #[test]
    fn failed_adoption_link_rollback_keeps_a_replaced_public_link() {
        for empty in [false, true] {
            let fixture = Fixture::new("cache-link-rollback-replacement");
            let original = fixture.home.path().join(".cargo/registry");
            std::fs::create_dir_all(original.parent().unwrap()).unwrap();
            if empty {
                std::fs::create_dir(&original).unwrap();
            }
            let raced = original.clone();
            let held = fixture.home.path().join("moved-owned-link");
            let dirs = crate::testkit::Faults::new();
            *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
                if name == "registry" {
                    std::fs::rename(&raced, &held).unwrap();
                    std::fs::write(&raced, b"foreign file").unwrap();
                }
            }));
            fixture.deny_record_writes();
            assert!(adopt(
                &fixture.state,
                &fixture.account,
                &dirs,
                fixture.facts.as_ref(),
                &fixture.volume.mounts(),
                &fixture.volume.observer(),
                &fixture.request(CacheKind::CargoRegistry)
            )
            .is_err());
            fixture.allow_record_writes();
            assert_eq!(read(&original), b"foreign file");
            assert!(
                super::read_adopt_intent(&fixture.state, CacheKind::CargoRegistry)
                    .unwrap()
                    .is_some()
            );
            assert!(
                super::undo_capture::journal_path(&fixture.state, CacheKind::CargoRegistry)
                    .is_file()
            );
        }
    }

    #[test]
    fn failed_adoption_marker_rollback_preserves_a_concurrent_marker_replacement() {
        let fixture = Fixture::new("cache-marker-rollback-replacement");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("payload"), b"data").unwrap();
        let original = fixture.home.path().join(".cargo/registry");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &original).unwrap();
        let raced = target.join(area::ROOT_MARKER_FILE);
        let old = target.join("moved-owned-marker");
        let dirs = crate::testkit::Faults::new();
        *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == area::ROOT_MARKER_FILE {
                std::fs::rename(&raced, &old).unwrap();
                std::fs::write(&raced, b"foreign marker").unwrap();
            }
        }));
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        fixture.deny_record_writes();
        assert!(adopt(
            &fixture.state,
            &fixture.account,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &request
        )
        .is_err());
        fixture.allow_record_writes();
        assert_eq!(
            read(&target.join(area::ROOT_MARKER_FILE)),
            b"foreign marker"
        );
        assert_eq!(read(&target.join("payload")), b"data");
        assert!(original.is_symlink());
        assert!(
            super::read_adopt_intent(&fixture.state, CacheKind::CargoRegistry)
                .unwrap()
                .is_some()
        );
        assert!(
            super::undo_capture::journal_path(&fixture.state, CacheKind::CargoRegistry).is_file()
        );
    }

    #[test]
    fn setting_readback_failure_keeps_durable_recovery_before_bytes() {
        let fixture = Fixture::new("cache-adopt-intent-readback");
        let original = fixture.home.path().join(".npmrc");
        std::fs::write(&original, b"cache=/old/cache\nother=kept\n").unwrap();
        let dirs = crate::testkit::Faults::new();
        *dirs.fail_settings_readback.borrow_mut() = Some(".npmrc".to_owned());
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        let error = adopt(
            &fixture.state,
            &fixture.account,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &request,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        let pending = super::read_adopt_intent(&fixture.state, CacheKind::NpmCache)
            .unwrap()
            .unwrap();
        let super::OriginalState::File { snapshot } = &pending.before else {
            panic!("before bytes missing")
        };
        assert_eq!(
            super::decode_hex(&snapshot.bytes).unwrap(),
            b"cache=/old/cache\nother=kept\n"
        );
        assert!(
            fixture.adopt(&request).is_err(),
            "pending intent prevents another adoption"
        );
        fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::NpmCache),
                plan: false,
            })
            .unwrap();
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"cache=/old/cache\nother=kept\n"
        );
        assert!(
            super::read_adopt_intent(&fixture.state, CacheKind::NpmCache)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interrupted_empty_directory_removal_replays_its_exact_mode() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new("cache-intent-empty-replay");
        let original = fixture.home.path().join(".cargo/git");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o770)).unwrap();
        let before = super::original_state(&fixture.platform(), &original).unwrap();
        let pending = super::AdoptIntent {
            schema: "barge.cache-adopt/1".to_owned(),
            id: "ab".repeat(16),
            kind: CacheKind::CargoGit,
            volume: fixture.volume.record.clone(),
            original: original.clone(),
            before,
            expected_setting: None,
            setting_after: None,
            target: None,
            marker_before: None,
            marker_plan: None,
            link: None,
            held: None,
            candidate: None,
        };
        super::begin_adopt_intent(&fixture.state, &pending).unwrap();
        let mut competing = pending.clone();
        competing.id = "cd".repeat(16);
        assert!(super::begin_adopt_intent(&fixture.state, &competing).is_err());
        assert_eq!(
            super::read_adopt_intent(&fixture.state, CacheKind::CargoGit)
                .unwrap()
                .unwrap()
                .id,
            pending.id
        );
        std::fs::remove_dir(&original).unwrap();
        fixture.facts.present.set(false);
        assert!(fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoGit),
                plan: false
            })
            .is_err());
        assert!(
            !original.exists(),
            "replay must not restore on an unverified volume"
        );
        fixture.facts.present.set(true);
        fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoGit),
                plan: false,
            })
            .unwrap();
        assert_eq!(
            std::fs::metadata(&original).unwrap().permissions().mode() & 0o7777,
            0o770
        );
        assert!(
            super::read_adopt_intent(&fixture.state, CacheKind::CargoGit)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cache_targets_refuse_parent_components_and_symlinked_ancestors() {
        for escape in ["parent", "symlink"] {
            let fixture = Fixture::new("cache-containment-escape");
            let outside = fixture.home.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::create_dir_all(fixture.caches()).unwrap();
            let target = if escape == "parent" {
                fixture.caches().join("../outside/cache")
            } else {
                std::os::unix::fs::symlink(&outside, fixture.caches().join("alias")).unwrap();
                fixture.caches().join("alias/cache")
            };
            let original = fixture.home.path().join(".npmrc");
            let bytes = format!("cache={}\n", target.display());
            std::fs::write(&original, &bytes).unwrap();
            let mut request = fixture.request(CacheKind::NpmCache);
            request.adopt_existing_root = true;
            assert!(fixture.adopt(&request).is_err(), "{escape}");
            assert_eq!(std::fs::read_to_string(&original).unwrap(), bytes);
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        }
    }

    #[test]
    fn empty_directory_undo_restores_nondefault_permissions() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o700, 0o770] {
            let fixture = Fixture::new("cache-undo-mode");
            let original = fixture.home.path().join(".cargo/git");
            std::fs::create_dir_all(&original).unwrap();
            std::fs::set_permissions(&original, std::fs::Permissions::from_mode(mode)).unwrap();
            fixture
                .adopt(&fixture.request(CacheKind::CargoGit))
                .unwrap();
            fixture
                .undo(&UndoRequest {
                    placement_id: None,
                    kind: Some(CacheKind::CargoGit),
                    plan: false,
                })
                .unwrap();
            assert_eq!(
                std::fs::metadata(original).unwrap().permissions().mode() & 0o7777,
                mode
            );
        }
    }

    #[test]
    fn deleting_an_adopt_created_setting_refuses_undo_without_effects() {
        let fixture = Fixture::new("cache-setting-deleted-after-adopt");
        let settings = fixture.home.path().join(".npmrc");
        assert!(!settings.exists());
        let cache = fixture.caches().join("npm");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("payload"), b"kept cache data").unwrap();
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        request.adopt_existing_root = true;
        let record = fixture.adopt(&request).unwrap().placement.unwrap();
        let PlacementForm::Setting(form) = &record.form else {
            panic!("a setting placement")
        };
        assert!(form.before.is_none());
        assert!(form.held_root.is_some());
        assert!(settings.is_file());
        std::fs::remove_file(&settings).unwrap();
        let marker = cache.join(area::ROOT_MARKER_FILE);
        let marker_facts = facts(&marker);
        let before_home = tree(fixture.home.path());
        let before_state = tree(fixture.state.root());
        let before_volume = tree(&fixture.volume.root);
        for plan in [true, false] {
            let error = fixture
                .undo(&UndoRequest {
                    placement_id: None,
                    kind: Some(CacheKind::NpmCache),
                    plan,
                })
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UnknownState);
            assert!(!settings.exists());
            assert_eq!(tree(fixture.home.path()), before_home);
            assert_eq!(tree(fixture.state.root()), before_state);
            assert_eq!(tree(&fixture.volume.root), before_volume);
            assert_eq!(facts(&marker), marker_facts);
            assert_eq!(
                read_placements(&fixture.state).unwrap().records,
                vec![record.clone()]
            );
        }
    }

    #[test]
    fn settings_diverging_during_undo_are_not_overwritten() {
        let fixture = Fixture::new("cache-setting-undo-race");
        let original = fixture.home.path().join(".npmrc");
        std::fs::write(&original, b"cache=/old\n").unwrap();
        let mut request = fixture.request(CacheKind::NpmCache);
        request.write_setting = true;
        fixture.adopt(&request).unwrap();
        let dirs = crate::testkit::Faults::new();
        let name = original.clone();
        *dirs.capture_probe.borrow_mut() = Some(Box::new(move |entry| {
            if entry == ".npmrc" {
                std::fs::write(&name, b"cache=/concurrent-edit\n").unwrap();
            }
        }));
        assert!(undo(
            &fixture.state,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::NpmCache),
                plan: false
            }
        )
        .is_err());
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"cache=/concurrent-edit\n"
        );
        assert_eq!(read_placements(&fixture.state).unwrap().records.len(), 1);
    }

    #[test]
    fn same_text_link_replaced_during_custody_is_kept() {
        let fixture = Fixture::new("cache-link-undo-race");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).unwrap();
        fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .unwrap();
        let original = fixture.home.path().join(".cargo/registry");
        let target = std::fs::read_link(&original).unwrap();
        let dirs = crate::testkit::Faults::new();
        let name = original.clone();
        let old = fixture.home.path().join("old-link");
        let fired = std::cell::Cell::new(false);
        *dirs.observe_probe.borrow_mut() = Some(Box::new(move |entry| {
            if entry == area::ROOT_MARKER_FILE && !fired.replace(true) {
                std::fs::rename(&name, &old).unwrap();
                std::os::unix::fs::symlink(&target, &name).unwrap();
            }
        }));
        assert!(undo(
            &fixture.state,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoRegistry),
                plan: false
            }
        )
        .is_err());
        assert!(original.is_symlink());
        assert_eq!(read_placements(&fixture.state).unwrap().records.len(), 1);
    }
    #[test]
    fn replaced_link_at_the_capture_syscall_is_preserved_and_recorded() {
        let fixture = Fixture::new("cache-link-capture-race");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).unwrap();
        fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .unwrap();
        let path = fixture.home.path().join(".cargo/registry");
        let old = fixture.home.path().join("original-link");
        let moved = path.clone();
        let dirs = crate::testkit::Faults::new();
        *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == "registry" {
                std::fs::rename(&moved, &old).unwrap();
                std::fs::write(&moved, b"raced file must survive").unwrap();
            }
        }));
        let result = undo(
            &fixture.state,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoRegistry),
                plan: false,
            },
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"raced file must survive");
        let bytes = std::fs::read(super::undo_capture::journal_path(
            &fixture.state,
            CacheKind::CargoRegistry,
        ))
        .unwrap();
        let journal: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(journal["phase"].as_str().unwrap().starts_with("refused:"));
        assert_eq!(journal["effects"][0]["put_back"], "renamed");
        assert_eq!(read_placements(&fixture.state).unwrap().records.len(), 1);
        assert!(fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoRegistry),
                plan: false
            })
            .is_err());
    }

    #[test]
    fn settings_replaced_at_capture_are_neither_overwritten_nor_removed() {
        for existing in [false, true] {
            let fixture = Fixture::new("cache-setting-capture-race");
            let path = fixture.home.path().join(".npmrc");
            if existing {
                std::fs::write(&path, b"cache=/old\n").unwrap();
            }
            let mut request = fixture.request(CacheKind::NpmCache);
            request.write_setting = true;
            fixture.adopt(&request).unwrap();
            let old = fixture.home.path().join("adopted-setting");
            let moved = path.clone();
            let dirs = crate::testkit::Faults::new();
            *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
                if name == ".npmrc" {
                    std::fs::rename(&moved, &old).unwrap();
                    std::fs::write(&moved, b"concurrent setting edit\n").unwrap();
                }
            }));
            assert!(undo(
                &fixture.state,
                &dirs,
                fixture.facts.as_ref(),
                &fixture.volume.mounts(),
                &fixture.volume.observer(),
                &UndoRequest {
                    placement_id: None,
                    kind: Some(CacheKind::NpmCache),
                    plan: false
                }
            )
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"concurrent setting edit\n");
            assert_eq!(read_placements(&fixture.state).unwrap().records.len(), 1);
        }
    }

    #[test]
    fn marker_capture_race_puts_settings_and_foreign_marker_back_without_unlink() {
        let fixture = Fixture::new("cache-setting-marker-capture-race");
        let target = fixture.caches().join("npm");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("payload"), b"cache data").unwrap();
        let path = fixture.home.path().join(".npmrc");
        std::fs::write(&path, format!("cache={}\n", target.display())).unwrap();
        let mut request = fixture.request(CacheKind::NpmCache);
        request.adopt_existing_root = true;
        fixture.adopt(&request).unwrap();
        let setting = std::fs::read(&path).unwrap();
        let marker = target.join(area::ROOT_MARKER_FILE);
        let moved = marker.clone();
        let old = target.join("original-marker");
        let dirs = crate::testkit::Faults::new();
        *dirs.capture_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == area::ROOT_MARKER_FILE {
                std::fs::rename(&moved, &old).unwrap();
                std::fs::write(&moved, b"foreign marker").unwrap();
            }
        }));
        assert!(undo(
            &fixture.state,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::NpmCache),
                plan: false
            }
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), setting);
        assert_eq!(std::fs::read(marker).unwrap(), b"foreign marker");
        assert_eq!(
            std::fs::read(target.join("payload")).unwrap(),
            b"cache data"
        );
    }

    #[test]
    fn a_taken_original_name_keeps_both_the_foreign_entry_and_capture() {
        let fixture = Fixture::new("cache-undo-taken-name");
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).unwrap();
        fixture
            .adopt(&fixture.request(CacheKind::CargoRegistry))
            .unwrap();
        let dirs = crate::testkit::Faults::new();
        dirs.occupying_after_capture("registry");
        assert!(undo(
            &fixture.state,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoRegistry),
                plan: false
            }
        )
        .is_err());
        let parent = fixture.home.path().join(".cargo");
        assert_eq!(
            std::fs::read(parent.join("registry")).unwrap(),
            b"a racer took this name back"
        );
        let journal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(super::undo_capture::journal_path(
                &fixture.state,
                CacheKind::CargoRegistry,
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(journal["effects"][0]["put_back"], "taken");
        let private = journal["effects"][0]["private"].as_str().unwrap();
        assert!(parent.join(private).is_symlink());
    }

    #[test]
    fn late_settings_divergence_keeps_the_adopted_marker() {
        let fixture = Fixture::new("cache-setting-marker-late-edit");
        let target = fixture.caches().join("npm");
        std::fs::create_dir_all(&target).unwrap();
        let path = fixture.home.path().join(".npmrc");
        std::fs::write(&path, format!("cache={}\n", target.display())).unwrap();
        let mut request = fixture.request(CacheKind::NpmCache);
        request.adopt_existing_root = true;
        fixture.adopt(&request).unwrap();
        let marker = target.join(area::ROOT_MARKER_FILE);
        let original_marker = std::fs::read(&marker).unwrap();
        let edited = path.clone();
        let fired = std::cell::Cell::new(false);
        let dirs = crate::testkit::Faults::new();
        *dirs.observe_probe.borrow_mut() = Some(Box::new(move |name| {
            if name == area::ROOT_MARKER_FILE && !fired.replace(true) {
                std::fs::write(&edited, b"cache=/concurrent\n").unwrap();
            }
        }));
        assert!(undo(
            &fixture.state,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::NpmCache),
                plan: false
            }
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"cache=/concurrent\n");
        assert_eq!(std::fs::read(marker).unwrap(), original_marker);
    }

    #[test]
    fn interrupted_marker_readback_recovers_only_the_planned_marker() {
        let fixture = Fixture::new("cache-marker-intent-readback");
        let target = fixture.caches().join("cargo/registry");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("payload"), b"kept data").unwrap();
        std::fs::create_dir_all(fixture.home.path().join(".cargo")).unwrap();
        let original = fixture.home.path().join(".cargo/registry");
        std::os::unix::fs::symlink(&target, &original).unwrap();
        let dirs = crate::testkit::Faults::new();
        *dirs.fail_settings_readback.borrow_mut() = Some(area::ROOT_MARKER_FILE.to_owned());
        let mut request = fixture.request(CacheKind::CargoRegistry);
        request.adopt_existing_root = true;
        assert!(adopt(
            &fixture.state,
            &fixture.account,
            &dirs,
            fixture.facts.as_ref(),
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &request
        )
        .is_err());
        let intent = super::read_adopt_intent(&fixture.state, CacheKind::CargoRegistry)
            .unwrap()
            .unwrap();
        assert!(intent.marker_plan.is_some());
        assert!(target.join(area::ROOT_MARKER_FILE).exists());
        fixture
            .undo(&UndoRequest {
                placement_id: None,
                kind: Some(CacheKind::CargoRegistry),
                plan: false,
            })
            .unwrap();
        assert!(!target.join(area::ROOT_MARKER_FILE).exists());
        assert_eq!(std::fs::read(target.join("payload")).unwrap(), b"kept data");
        assert_eq!(std::fs::read_link(original).unwrap(), target);
    }
}
