//! The only authority Barge writes: the account volume registry
//! (`barge.registry/1`), its strict reader and validator, and a writer the
//! later slices use (stage 1 design, "Registry of volumes").
//!
//! A registry record says, per volume, what the kernel's identity is, how the
//! volume is expected to be available, what its data is worth, where Barge's
//! own folders live on it, who wrote the record, what scratch deletions are
//! authorized on it, and a generation that detects a stale reader.
//!
//! Two rules shape the types here. First, availability and durability are both
//! explicit and required: a record that omits either is invalid, and
//! `durability: undeclared` is a real third value that never means `expendable`.
//! Second, an invalid registry is never repaired and never rebuilt from markers:
//! [`read`] refuses it, and every mutating path starts there, so a corrupt
//! registry stops all authority instead of being replaced by a guess.
//!
//! Slice S2 lands the registry. Slice S3 adds area records and slice S6 the
//! placement records beside it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::contract::{DOCUMENT_SIZE_CAP_BYTES, REGISTRY_SCHEMA, VOLUME_ROWS};
use crate::error::{Error, ErrorKind, Result};
use crate::os::{ExpectedVolume, VolumeUuid};
use crate::state::{
    self, Bounds, Deadline, LockMode, ReadOutcome, StateLock, StatePaths, READ_DEADLINE,
};

/// The registry file's name inside the state directory.
pub const REGISTRY_FILE: &str = "registry.json";

/// The largest registry Barge reads or writes (design, "Bounded resources").
pub const REGISTRY_SIZE_CAP_BYTES: usize = DOCUMENT_SIZE_CAP_BYTES;

/// The most volume records a registry may hold.
pub const REGISTRY_VOLUME_ROWS: usize = VOLUME_ROWS;

/// The most scratch-deletion delegations one volume may record: one per marker
/// family the design names.
pub const REGISTRY_DELEGATION_ROWS: usize = 2;

/// The most legacy claims one volume may record, one per declared scope.
pub const REGISTRY_CLAIM_ROWS: usize = 128;

/// A volume's availability: when it is attached (design, "Volume modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Always attached; absence is abnormal.
    Permanent,
    /// Attached on and off; absence is normal.
    Ephemeral,
}

/// A volume's durability: what the data on it is worth (design, "Volume
/// modes").
///
/// The three values are independent of availability and never defaulted.
/// [`Durability::Undeclared`] is the honest state of a record that does not say,
/// and it never behaves like [`Durability::Expendable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Rebuildable data Barge may prune.
    Expendable,
    /// Data that lives only on that volume and is never pruned.
    Kept,
    /// An imported record that does not say; never treated as expendable.
    Undeclared,
}

impl Durability {
    /// Whether this durability permits pruning. Only `expendable` does, so an
    /// undeclared record cannot authorize a deletion by omission.
    pub const fn permits_pruning(self) -> bool {
        matches!(self, Self::Expendable)
    }

    /// Whether a human declared the durability. An undeclared record refuses
    /// every placement and area use until one is declared.
    pub const fn is_declared(self) -> bool {
        !matches!(self, Self::Undeclared)
    }
}

/// Where a volume record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOrigin {
    /// An operator declared it.
    Declared,
    /// Imported from Oxide's registration, read-only.
    ObservedOxide,
}

/// Barge's own per-volume folder overrides, relative to `root_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderOverrides {
    /// The per-volume `scratch/` override; `null` is `scratch/`.
    pub scratch: Option<String>,
    /// The per-volume `caches/` override; `null` is `caches/`.
    pub caches: Option<String>,
}

impl FolderOverrides {
    /// No overrides: Barge's own folders are `scratch/` and `caches/`.
    pub const fn none() -> Self {
        Self {
            scratch: None,
            caches: None,
        }
    }
}

/// A marker family whose scratch a delegation may cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerFamily {
    /// `.oxide-scratch.json`, concluded by Oxide's supervisor only.
    OxideScratch,
    /// `.effigy-scratch.json`, concluded by Effigy only.
    EffigyScratch,
}

/// One recorded delegation: the family's owner authorizing Barge to expire that
/// family's roots on one volume, under that family's own rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchDelegation {
    /// The marker family this delegation covers.
    pub family: MarkerFamily,
    /// Who declared it.
    pub by: String,
    /// When it was declared.
    pub declared_ms: u64,
}

/// What one legacy claim covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClaimScope {
    /// The children of `<scratch>/<name>/`.
    Project {
        /// The project folder's name.
        name: String,
    },
    /// Entries directly under `<scratch>/` that are files or whose name contains
    /// a dot, matching the host reaper's units.
    TopLevelAdhoc,
}

/// The unit shape a legacy claim deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimUnit {
    /// One child of the scoped project folder.
    Child,
    /// One top-level entry.
    Entry,
}

/// One human's claim over unmarked units in one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchClaim {
    /// The scope the claim covers; it never covers the scratch root as a whole.
    pub scope: ClaimScope,
    /// The unit shape the claim deletes.
    pub unit: ClaimUnit,
    /// How old a unit's newest entry must be before it is eligible.
    pub unmarked_idle_seconds: u64,
    /// Marker families whose units the claim never covers.
    pub families_excluded: Vec<MarkerFamily>,
    /// Who declared it.
    pub declared_by: String,
    /// When it was declared.
    pub declared_ms: u64,
}

/// The recorded deletion authority for these mechanisms.
///
/// Empty means "keep and report": no non-native unit is ever deleted without a
/// delegation or a claim that covers it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchGrants {
    /// One delegation per marker family, written by that family's owner.
    pub delegations: Vec<ScratchDelegation>,
    /// One claim per declared scope, written by a human.
    pub claims: Vec<ScratchClaim>,
}

impl ScratchGrants {
    /// No recorded authority.
    pub fn none() -> Self {
        Self::default()
    }
}

/// One registered volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRecord {
    /// The APFS volume UUID from the kernel: the identity that matters.
    pub volume_uuid: VolumeUuid,
    /// The last observed mount point; used to find the volume, never as
    /// identity.
    pub mount_hint: Option<PathBuf>,
    /// When the volume is attached.
    pub availability: Availability,
    /// What the data on it is; never defaulted.
    pub durability: Durability,
    /// The folder relative to the mount that holds everything Barge owns;
    /// `""` means the drive root.
    pub root_dir: String,
    /// Per-volume overrides of Barge's own folders.
    pub folders: FolderOverrides,
    /// Whether an operator declared the record or it was imported.
    pub origin: RecordOrigin,
    /// The recorded scratch-deletion authority for this volume.
    pub scratch_grants: ScratchGrants,
    /// Increases on every change, so a stale reader is detectable.
    pub generation: u64,
    /// When this record was written.
    pub registered_ms: u64,
}

impl VolumeRecord {
    /// The identity a use of this volume must verify against.
    pub fn expected_volume(&self) -> ExpectedVolume<'_> {
        ExpectedVolume {
            volume_uuid: &self.volume_uuid,
            mount_hint: self.mount_hint.as_deref(),
        }
    }

    /// The record's durability, or a refusal when it does not declare one.
    ///
    /// Every placement and area use asks this first: an imported record whose
    /// durability is `undeclared` refuses with `unknown_state` until a human
    /// declares it, because expending data is a permission that is never
    /// inferred.
    pub fn declared_durability(&self) -> Result<Durability> {
        if self.durability.is_declared() {
            return Ok(self.durability);
        }
        Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "volume {} does not declare its durability; a placement or area use refuses until a human declares kept or expendable",
                self.volume_uuid
            ),
        )
        .with_detail("volume_uuid", self.volume_uuid.as_str().into())
        .with_detail("durability", "undeclared".into()))
    }

    /// Whether this record and `other` describe the same volume with the same
    /// content, ignoring the generation that a write bumps.
    fn same_change(&self, other: &Self) -> bool {
        let mut left = self.clone();
        let mut right = other.clone();
        left.generation = 0;
        right.generation = 0;
        left == right
    }

    fn validate(&self) -> Result<()> {
        if self.generation == 0 {
            return Err(invalid(
                "a volume record's generation starts at 1",
                "volume_uuid",
                self.volume_uuid.as_str(),
            ));
        }
        if let Some(mount_hint) = &self.mount_hint {
            if !mount_hint.is_absolute() {
                return Err(invalid(
                    "a mount hint must be an absolute path",
                    "mount_hint",
                    &mount_hint.display().to_string(),
                ));
            }
        }
        validate_folder("root_dir", &self.root_dir, true)?;
        if let Some(scratch) = &self.folders.scratch {
            validate_folder("folders.scratch", scratch, false)?;
        }
        if let Some(caches) = &self.folders.caches {
            validate_folder("folders.caches", caches, false)?;
        }
        self.scratch_grants.validate()?;
        Ok(())
    }
}

impl ScratchGrants {
    fn validate(&self) -> Result<()> {
        if self.delegations.len() > REGISTRY_DELEGATION_ROWS {
            return Err(invalid(
                "at most one delegation per marker family is recorded",
                "delegations",
                &self.delegations.len().to_string(),
            ));
        }
        let mut families = BTreeSet::new();
        for delegation in &self.delegations {
            if delegation.by.trim().is_empty() {
                return Err(invalid(
                    "a delegation must name who declared it",
                    "family",
                    &format!("{:?}", delegation.family),
                ));
            }
            if !families.insert(delegation.family) {
                return Err(invalid(
                    "one marker family can be delegated only once per volume",
                    "family",
                    &format!("{:?}", delegation.family),
                ));
            }
        }
        if self.claims.len() > REGISTRY_CLAIM_ROWS {
            return Err(invalid(
                "too many legacy claims are recorded",
                "claims",
                &self.claims.len().to_string(),
            ));
        }
        let mut scopes = BTreeSet::new();
        for claim in &self.claims {
            if claim.declared_by.trim().is_empty() {
                return Err(invalid(
                    "a legacy claim must name who declared it",
                    "declared_by",
                    "",
                ));
            }
            let scope_key = match &claim.scope {
                ClaimScope::Project { name } => {
                    validate_component("scope.name", name)?;
                    format!("project:{name}")
                }
                ClaimScope::TopLevelAdhoc => "top_level_adhoc".to_owned(),
            };
            if !scopes.insert(scope_key.clone()) {
                return Err(invalid(
                    "one scope can be claimed only once per volume",
                    "scope",
                    &scope_key,
                ));
            }
            let mut excluded = BTreeSet::new();
            for family in &claim.families_excluded {
                if !excluded.insert(*family) {
                    return Err(invalid(
                        "a claim lists an excluded family only once",
                        "families_excluded",
                        &format!("{family:?}"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// One `barge.registry/1` document: the account's registered volumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryDocument {
    /// Always `barge.registry/1`; a different major is refused, not guessed at.
    pub schema: String,
    /// The registered volumes, in the order they are reported.
    pub volumes: Vec<VolumeRecord>,
}

impl RegistryDocument {
    /// A document holding `volumes`, under the one schema this build writes.
    pub fn new(volumes: Vec<VolumeRecord>) -> Self {
        Self {
            schema: REGISTRY_SCHEMA.to_owned(),
            volumes,
        }
    }

    /// Validate the schema, the row cap, every record and the uniqueness of
    /// every volume identity.
    pub fn validate(&self, max_records: usize) -> Result<()> {
        if self.schema != REGISTRY_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{REGISTRY_SCHEMA}` major this build reads",
                    self.schema
                ),
            )
            .with_detail("schema", self.schema.clone().into())
            .with_detail("supported", REGISTRY_SCHEMA.into()));
        }
        if self.volumes.len() > max_records {
            return Err(invalid(
                "the registry holds more volume records than the row cap allows",
                "volumes",
                &self.volumes.len().to_string(),
            )
            .with_detail("cap", max_records.into()));
        }
        let mut seen = BTreeSet::new();
        for volume in &self.volumes {
            volume.validate()?;
            if !seen.insert(volume.volume_uuid.clone()) {
                return Err(invalid(
                    "one volume is registered twice",
                    "volume_uuid",
                    volume.volume_uuid.as_str(),
                ));
            }
        }
        Ok(())
    }

    /// The record for one volume, if it is registered.
    pub fn volume(&self, volume_uuid: &VolumeUuid) -> Option<&VolumeRecord> {
        self.volumes
            .iter()
            .find(|volume| &volume.volume_uuid == volume_uuid)
    }

    /// The same records with their generations bumped: a new record starts at 1,
    /// a changed one increases by one, and an unchanged one keeps its generation
    /// so a reader can tell that nothing happened.
    fn with_bumped_generations(&self, current: &Registry) -> Self {
        let volumes = self
            .volumes
            .iter()
            .map(|record| {
                let mut next = record.clone();
                next.generation = match current.volume(&record.volume_uuid) {
                    Some(previous) if previous.same_change(record) => previous.generation,
                    Some(previous) => previous.generation.saturating_add(1),
                    None => 1,
                };
                next
            })
            .collect();
        Self::new(volumes)
    }
}

/// What the registry file currently says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registry {
    /// No registry file. That is "no registered volumes", never "rebuild one".
    Absent,
    /// A registry that validated against this build's schema.
    Present(RegistryDocument),
}

impl Registry {
    /// The registered volumes, in file order.
    pub fn volumes(&self) -> &[VolumeRecord] {
        match self {
            Self::Absent => &[],
            Self::Present(document) => &document.volumes,
        }
    }

    /// The document, when there is one.
    pub fn document(&self) -> Option<&RegistryDocument> {
        match self {
            Self::Absent => None,
            Self::Present(document) => Some(document),
        }
    }

    /// One volume's record, if it is registered.
    pub fn volume(&self, volume_uuid: &VolumeUuid) -> Option<&VolumeRecord> {
        self.document()?.volume(volume_uuid)
    }
}

/// Read the registry strictly.
///
/// An absent file is [`Registry::Absent`]; a file that is oversized, unreadable,
/// a link, an unknown major, missing or carrying an unknown field, a duplicate
/// volume or any other invalid content is refused with `unknown_state` naming
/// the file. Nothing here writes, repairs or reconstructs a registry, and no
/// marker or foreign registration is ever consulted to do so.
///
/// The caller takes the registry lock first — shared to observe, exclusive to
/// change — because this function reads bytes and nothing else.
pub fn read(state: &StatePaths) -> Result<Registry> {
    let path = state.registry_path();
    let bounds = Bounds::new(
        REGISTRY_SIZE_CAP_BYTES,
        REGISTRY_VOLUME_ROWS,
        Deadline::after(READ_DEADLINE),
    );
    let bytes = match state::read_bounded(&path, &bounds) {
        ReadOutcome::Absent => return Ok(Registry::Absent),
        ReadOutcome::Present(bytes) => bytes,
        ReadOutcome::Refused(error) => return Err(named(error, &path)),
    };
    let document: RegistryDocument = serde_json::from_slice(&bytes).map_err(|error| {
        named(
            Error::new(
                ErrorKind::UnknownState,
                format!("`{REGISTRY_SCHEMA}` refused the registry: {error}"),
            ),
            &path,
        )
    })?;
    document
        .validate(bounds.max_entries)
        .map_err(|error| named(error, &path))?;
    Ok(Registry::Present(document))
}

fn named(error: Error, path: &std::path::Path) -> Error {
    error.with_detail("path", path.display().to_string().into())
}

/// The registry writer, for the slices that change a volume record.
///
/// It takes no lock of its own: the caller holds the exclusive registry lock and
/// passes it in, so a write without the lock cannot happen by accident. It
/// creates the state directory (a mutating call is the only thing that may),
/// re-reads the current registry so an invalid one refuses instead of being
/// overwritten, bumps the generations, validates and publishes atomically.
///
/// Slice S2 has no CLI command that writes the registry; this exists for the
/// registration slice and is unit-tested on fixtures only.
#[derive(Debug)]
pub struct RegistryWriter<'a> {
    state: &'a StatePaths,
}

impl<'a> RegistryWriter<'a> {
    /// A writer over one state directory.
    pub fn new(state: &'a StatePaths) -> Self {
        Self { state }
    }

    /// Publish `document`, returning the document as written.
    pub fn publish(
        &self,
        lock: &StateLock,
        document: &RegistryDocument,
    ) -> Result<RegistryDocument> {
        if lock.rank() != state::LockRank::Registry || lock.mode() != LockMode::Exclusive {
            return Err(Error::new(
                ErrorKind::Usage,
                "a registry write needs the exclusive registry lock",
            )
            .with_detail("lock", lock.path().display().to_string().into())
            .with_detail("mode", "exclusive registry".into()));
        }
        let current = read(self.state)?;
        let next = document.with_bumped_generations(&current);
        next.validate(REGISTRY_VOLUME_ROWS)?;
        let mut bytes = serde_json::to_vec_pretty(&next).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("could not encode the registry: {error}"),
            )
        })?;
        bytes.push(b'\n');
        if bytes.len() > REGISTRY_SIZE_CAP_BYTES {
            return Err(Error::new(
                ErrorKind::ResourceBound,
                format!(
                    "the registry would be {} bytes, over the {REGISTRY_SIZE_CAP_BYTES}-byte cap",
                    bytes.len()
                ),
            )
            .with_detail("bytes", bytes.len().into())
            .with_detail("cap", REGISTRY_SIZE_CAP_BYTES.into()));
        }
        self.state.initialize()?;
        state::publish_atomic(&self.state.registry_path(), &bytes)?;
        Ok(next)
    }
}

/// Validate one folder value: a relative path inside the volume.
///
/// An empty value is the volume's drive root, and is allowed only where the
/// design says so (`root_dir`). A component that is empty, `.` or `..`, an
/// absolute prefix or a trailing separator is refused: the descriptor walk that
/// resolves the path is slice S3, and this check makes sure it is never handed a
/// path that could escape.
pub fn validate_folder(field: &str, value: &str, allow_empty: bool) -> Result<()> {
    if value.is_empty() {
        if allow_empty {
            return Ok(());
        }
        return Err(invalid(
            "a folder override is null when it is not set, not empty",
            field,
            value,
        ));
    }
    if value.contains('\0') {
        return Err(invalid("a folder contains a NUL byte", field, value));
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(invalid(
            "a folder is relative to the volume with no leading or trailing separator",
            field,
            value,
        ));
    }
    for component in value.split('/') {
        if component.is_empty() {
            return Err(invalid("a folder has an empty component", field, value));
        }
        if component == "." || component == ".." {
            return Err(invalid(
                "a folder has a `.` or `..` component",
                field,
                value,
            ));
        }
    }
    Ok(())
}

/// Validate one name that must be a single path component.
pub fn validate_component(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(invalid("a name is empty", field, value));
    }
    if value.contains('/') || value.contains('\0') {
        return Err(invalid(
            "a name is one path component, not a path",
            field,
            value,
        ));
    }
    if value == "." || value == ".." {
        return Err(invalid("a name is not `.` or `..`", field, value));
    }
    Ok(())
}

fn invalid(message: &str, field: &str, value: &str) -> Error {
    Error::new(ErrorKind::UnknownState, message.to_owned())
        .with_detail("field", field.into())
        .with_detail("value", value.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        read, validate_component, validate_folder, Availability, ClaimScope, ClaimUnit, Durability,
        FolderOverrides, MarkerFamily, RecordOrigin, Registry, RegistryDocument, RegistryWriter,
        ScratchClaim, ScratchDelegation, ScratchGrants, VolumeRecord, REGISTRY_SIZE_CAP_BYTES,
    };
    use crate::error::{ErrorKind, Result};
    use crate::os::{AccountFacts, VolumeUuid};
    use crate::state::{Deadline, LockSet, StatePaths};
    use crate::testkit::TempDir;

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";
    const OTHER_UUID: &str = "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D";

    struct FakeAccount {
        home: PathBuf,
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

    fn state(fixture: &TempDir) -> StatePaths {
        StatePaths::resolve(
            &FakeAccount {
                home: fixture.path().to_path_buf(),
            },
            Some(&fixture.path().join("state")),
        )
        .expect("the fixture state directory resolves")
    }

    fn record(uuid: &str) -> VolumeRecord {
        VolumeRecord {
            volume_uuid: VolumeUuid::parse(uuid).expect("a test UUID"),
            mount_hint: Some(PathBuf::from("/Volumes/Builds")),
            availability: Availability::Permanent,
            durability: Durability::Expendable,
            root_dir: String::new(),
            folders: FolderOverrides::none(),
            origin: RecordOrigin::Declared,
            scratch_grants: ScratchGrants::none(),
            generation: 1,
            registered_ms: 1_792_525_019_847,
        }
    }

    fn document(records: Vec<VolumeRecord>) -> RegistryDocument {
        RegistryDocument::new(records)
    }

    /// One mutating path: take the registry lock, then publish.
    fn mutate(state: &StatePaths, next: &RegistryDocument) -> Result<RegistryDocument> {
        // A mutating path creates the layout, then takes the registry lock.
        state.initialize()?;
        let mut locks =
            LockSet::with_deadline(state.clone(), Deadline::after(Duration::from_secs(5)));
        let lock = locks.lock_registry()?;
        RegistryWriter::new(state).publish(&lock, next)
    }

    #[test]
    fn an_absent_registry_is_no_volumes_and_is_never_created_by_reading() {
        let fixture = TempDir::new("registry-absent");
        let state = state(&fixture);
        assert_eq!(
            read(&state).expect("reading an absent registry"),
            Registry::Absent
        );
        assert!(
            !state.registry_path().exists(),
            "reading must not create a registry"
        );
        assert!(
            !state.root().exists(),
            "reading must not create the state directory"
        );
    }

    #[test]
    fn a_registry_round_trips_through_its_writer_and_reader() {
        let fixture = TempDir::new("registry-round-trip");
        let state = state(&fixture);
        let written =
            mutate(&state, &document(vec![record(UUID)])).expect("the registry is written");
        assert_eq!(written.volumes[0].generation, 1, "a new record starts at 1");

        let read_back = read(&state).expect("the registry reads");
        assert_eq!(read_back, Registry::Present(written));
        let bytes = std::fs::read(state.registry_path()).expect("the file exists");
        assert!(bytes.len() < REGISTRY_SIZE_CAP_BYTES);
        let text = String::from_utf8(bytes).expect("the registry is text");
        assert!(text.contains("\"schema\": \"barge.registry/1\""), "{text}");
    }

    #[test]
    fn a_changed_record_increments_its_generation_and_an_unchanged_one_keeps_it() {
        let fixture = TempDir::new("registry-generations");
        let state = state(&fixture);
        mutate(&state, &document(vec![record(UUID), record(OTHER_UUID)])).expect("first write");

        let mut changed = record(UUID);
        changed.root_dir = "barge".to_owned();
        let written =
            mutate(&state, &document(vec![changed, record(OTHER_UUID)])).expect("the second write");
        let first = written
            .volume(&VolumeUuid::parse(UUID).expect("a UUID"))
            .expect("the first volume");
        let second = written
            .volume(&VolumeUuid::parse(OTHER_UUID).expect("a UUID"))
            .expect("the second volume");
        assert_eq!(first.generation, 2, "a changed record is bumped");
        assert_eq!(
            second.generation, 1,
            "an unchanged record keeps its generation"
        );
    }

    #[test]
    fn a_truncated_registry_refuses_every_mutation() {
        let fixture = TempDir::new("registry-truncated");
        let state = state(&fixture);
        state.initialize().expect("the layout exists");
        let truncated = br#"{"schema":"barge.registry/1","volumes":[{"volume_uuid":"#;
        std::fs::write(state.registry_path(), truncated).expect("the fixture registry is written");

        let error = read(&state).expect_err("a truncated registry is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            error.detail()["path"],
            state.registry_path().display().to_string()
        );

        let error = mutate(&state, &document(vec![record(UUID)]))
            .expect_err("every mutation refuses an invalid registry");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            std::fs::read(state.registry_path()).expect("the file still reads"),
            truncated,
            "the refused mutation changed nothing"
        );
    }

    #[test]
    fn an_oversized_registry_refuses_every_mutation() {
        let fixture = TempDir::new("registry-oversized");
        let state = state(&fixture);
        state.initialize().expect("the layout exists");
        let oversized = vec![b' '; REGISTRY_SIZE_CAP_BYTES + 1];
        std::fs::write(state.registry_path(), &oversized).expect("the fixture registry is written");
        let error = mutate(&state, &document(vec![record(UUID)]))
            .expect_err("an oversized registry refuses every mutation");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
        assert_eq!(
            std::fs::read(state.registry_path())
                .expect("the file still reads")
                .len(),
            oversized.len(),
            "the refused mutation changed nothing"
        );
    }

    #[test]
    fn an_unknown_version_an_unknown_field_and_a_missing_field_all_refuse() {
        let fixture = TempDir::new("registry-strictness");
        let state = state(&fixture);
        state.initialize().expect("the layout exists");
        let valid =
            serde_json::to_value(document(vec![record(UUID)])).expect("the document encodes");

        let cases = [
            ("unknown version", {
                let mut value = valid.clone();
                value["schema"] = serde_json::json!("barge.registry/2");
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("unknown field", {
                let mut value = valid.clone();
                value["volumes"][0]["mode"] = serde_json::json!("permanent");
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("missing durability", {
                let mut value = valid.clone();
                value["volumes"][0]
                    .as_object_mut()
                    .expect("a record")
                    .remove("durability");
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("missing availability", {
                let mut value = valid.clone();
                value["volumes"][0]
                    .as_object_mut()
                    .expect("a record")
                    .remove("availability");
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("missing scratch grants", {
                let mut value = valid.clone();
                value["volumes"][0]
                    .as_object_mut()
                    .expect("a record")
                    .remove("scratch_grants");
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("generation zero", {
                let mut value = valid.clone();
                value["volumes"][0]["generation"] = serde_json::json!(0);
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("relative mount hint", {
                let mut value = valid.clone();
                value["volumes"][0]["mount_hint"] = serde_json::json!("relative/mount");
                serde_json::to_vec(&value).expect("encodes")
            }),
            ("duplicate volume", {
                let mut value = valid.clone();
                let first = value["volumes"][0].clone();
                value["volumes"] = serde_json::json!([first.clone(), first]);
                serde_json::to_vec(&value).expect("encodes")
            }),
        ];

        for (label, bytes) in cases {
            std::fs::write(state.registry_path(), &bytes).expect("the fixture registry is written");
            let error = read(&state).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
            let error = mutate(&state, &document(vec![record(UUID)])).expect_err(label);
            assert_eq!(error.kind(), ErrorKind::UnknownState, "{label}");
        }
    }

    #[test]
    fn the_registry_is_never_rebuilt_from_a_marker_or_a_foreign_registration() {
        let fixture = TempDir::new("registry-not-rebuilt");
        let state = state(&fixture);
        // A foreign registration and its pool sit beside Barge's state, and
        // nothing about them may become a registry record by reading it.
        fixture.write(
            "oxide/oxide-managed/removable.json",
            br#"{"version":2,"mount_point":"/Volumes/Builds","volume_uuid":"A6AC3ED7-33EA-46B4-8082-7254CF291ED2","container_uuid":"AD1C4598-BCE0-4B23-9AAE-01349C7FA596","reserve_bytes":1,"cap_bytes":2,"quota_bytes":null,"capacity_bytes":3,"registered_ms":4,"mode":"permanent"}"#,
        );
        let registry = read(&state).expect("the absent registry reads");
        assert_eq!(registry, Registry::Absent);
        assert!(registry.volumes().is_empty());
        assert!(
            !state.registry_path().exists(),
            "an observation never becomes authority"
        );
    }

    #[test]
    fn a_write_without_the_registry_lock_is_refused() {
        let fixture = TempDir::new("registry-needs-lock");
        let state = state(&fixture);
        state.initialize().expect("the layout exists");
        let volume = VolumeUuid::parse(UUID).expect("a UUID");
        let mut locks =
            LockSet::with_deadline(state.clone(), Deadline::after(Duration::from_secs(5)));
        let volume_lock = locks.lock_volume(&volume).expect("a volume lock");
        let error = RegistryWriter::new(&state)
            .publish(&volume_lock, &document(vec![record(UUID)]))
            .expect_err("a volume lock does not authorize a registry write");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(!state.registry_path().exists());
    }

    #[test]
    fn a_document_that_fails_validation_is_not_published() {
        let fixture = TempDir::new("registry-invalid-write");
        let state = state(&fixture);
        let mut bad = record(UUID);
        bad.root_dir = "../escape".to_owned();
        let error = mutate(&state, &document(vec![bad])).expect_err("an escaping root_dir refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(!state.registry_path().exists());

        let mut bad = record(UUID);
        bad.folders.scratch = Some(String::new());
        let error = mutate(&state, &document(vec![bad])).expect_err("an empty override refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        let mut duplicate = record(UUID);
        duplicate.mount_hint = Some(PathBuf::from("relative/mount"));
        let error =
            mutate(&state, &document(vec![duplicate])).expect_err("a relative mount hint refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
    }

    #[test]
    fn durability_is_explicit_and_undeclared_never_permits_pruning() {
        assert!(Durability::Expendable.permits_pruning());
        assert!(!Durability::Kept.permits_pruning());
        assert!(!Durability::Undeclared.permits_pruning());
        assert!(!Durability::Undeclared.is_declared());

        let mut imported = record(UUID);
        imported.durability = Durability::Undeclared;
        imported.origin = RecordOrigin::ObservedOxide;
        let error = imported
            .declared_durability()
            .expect_err("an undeclared durability refuses a placement or area use");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            imported.declared_durability().unwrap_err().detail()["durability"],
            "undeclared"
        );
        assert_eq!(
            record(UUID)
                .declared_durability()
                .expect("a declared record"),
            Durability::Expendable
        );
    }

    #[test]
    fn a_record_names_the_identity_a_use_must_verify() {
        let record = record(UUID);
        let expected = record.expected_volume();
        assert_eq!(expected.volume_uuid.as_str(), UUID);
        assert_eq!(expected.mount_hint, Some(Path::new("/Volumes/Builds")));
    }

    #[test]
    fn a_folder_is_relative_and_a_component_is_one_name() {
        validate_folder("root_dir", "", true).expect("an empty root_dir is the drive root");
        validate_folder("root_dir", "barge", true).expect("one component");
        validate_folder("folders.scratch", "nested/scratch", false).expect("nested is fine");
        for bad in ["/absolute", "trailing/", "a//b", "./a", "a/../b", ".."] {
            assert!(validate_folder("root_dir", bad, true).is_err(), "{bad}");
        }
        assert!(
            validate_folder("folders.caches", "", false).is_err(),
            "empty override"
        );

        validate_component("scope.name", "example").expect("a name");
        for bad in ["", ".", "..", "a/b"] {
            assert!(validate_component("scope.name", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scratch_grants_carry_the_delegations_and_claims_that_authorize_a_delete() {
        let mut record = record(UUID);
        record.scratch_grants = ScratchGrants {
            delegations: vec![ScratchDelegation {
                family: MarkerFamily::EffigyScratch,
                by: "operator".to_owned(),
                declared_ms: 0,
            }],
            claims: vec![ScratchClaim {
                scope: ClaimScope::Project {
                    name: "example".to_owned(),
                },
                unit: ClaimUnit::Child,
                unmarked_idle_seconds: 1_209_600,
                families_excluded: vec![MarkerFamily::OxideScratch, MarkerFamily::EffigyScratch],
                declared_by: "operator".to_owned(),
                declared_ms: 0,
            }],
        };
        let fixture = TempDir::new("registry-grants");
        let state = state(&fixture);
        let written = mutate(&state, &document(vec![record.clone()])).expect("the record writes");
        assert_eq!(written.volumes[0].scratch_grants, record.scratch_grants);

        let mut twice = record.clone();
        twice.scratch_grants.delegations.push(ScratchDelegation {
            family: MarkerFamily::EffigyScratch,
            by: "operator".to_owned(),
            declared_ms: 1,
        });
        assert_eq!(
            mutate(&state, &document(vec![twice]))
                .expect_err("one delegation per family")
                .kind(),
            ErrorKind::UnknownState
        );

        let mut twice = record.clone();
        let claim = twice.scratch_grants.claims[0].clone();
        twice.scratch_grants.claims.push(claim);
        assert_eq!(
            mutate(&state, &document(vec![twice]))
                .expect_err("one claim per scope")
                .kind(),
            ErrorKind::UnknownState
        );

        let mut unknown_field = serde_json::to_value(&record).expect("the record encodes");
        unknown_field["scratch_grants"]["delegations"][0]["scope"] = serde_json::json!("x");
        std::fs::write(
            state.registry_path(),
            serde_json::to_vec(&serde_json::json!({
                "schema": "barge.registry/1",
                "volumes": [unknown_field],
            }))
            .expect("encodes"),
        )
        .expect("the fixture registry is written");
        assert_eq!(
            read(&state)
                .expect_err("an unknown grant field is refused")
                .kind(),
            ErrorKind::UnknownState
        );
    }

    #[test]
    fn a_registry_row_cap_is_the_volume_cap() {
        let records: Vec<VolumeRecord> = (0..super::REGISTRY_VOLUME_ROWS)
            .map(|index| {
                let mut record = record(UUID);
                record.volume_uuid = VolumeUuid::from_bytes([
                    u8::try_from(index).expect("a small index"),
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    1,
                ]);
                record
            })
            .collect();
        let document = document(records);
        document
            .validate(super::REGISTRY_VOLUME_ROWS)
            .expect("the row cap itself is allowed");
        let mut over = document.clone();
        over.volumes.push(record(OTHER_UUID));
        let error = over
            .validate(super::REGISTRY_VOLUME_ROWS)
            .expect_err("one row over the cap is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["cap"], super::REGISTRY_VOLUME_ROWS);
    }
}
