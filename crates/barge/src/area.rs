//! Consumer areas: path containment, declaration, root creation, holds and root
//! listing (stage 1 design, "Area declaration API", "Area declaration record on
//! disk", "Owned roots, markers and holds" and "Path containment and
//! resolution").
//!
//! Slice S3 lands the generic mechanics S4 (scratch), S5 (custody and deletion)
//! and S6 (caches) build on. Nothing here deletes, prunes or moves data: it
//! creates an owned root and its marker inside a declared area, records holds,
//! and reports what it found.
//!
//! Three rules shape the module.
//!
//! - **The descriptor walk is the authority.** A folder is resolved one
//!   component at a time from the verified volume root. Every component is
//!   checked with `lstat`, opened with no-follow, and its opened identity is
//!   compared with the checked one, so a symlink, a foreign-owned directory, a
//!   component on another device or an entry that changed between the two steps
//!   is refused before anything is created. String arithmetic never decides
//!   containment.
//! - **Every record is strict and bounded.** A declaration document, a root
//!   marker and a hold record are read under a size cap, parsed with no unknown
//!   fields, validated field by field, and never repaired or defaulted. An
//!   absent, oversized, symlinked, foreign-owned, future-dated or
//!   unknown-version record is unknown, and unknown is never owned.
//! - **The caller's declaration wins (O-003).** A stored declaration is the
//!   CLI's form of the same declaration, never a wider policy than the one a
//!   caller passed: the effective declaration is recorded exactly, and the
//!   caller's `absent`, `prune` and `reserve` values are never widened by what
//!   the state directory happens to hold.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::contract::{
    RowSet, AREA_DECLARATION_CAP_BYTES, AREA_ROWS, HOLD_CAP_BYTES, HOLD_ROWS, LISTING_NAME_ROWS,
    MARKER_CAP_BYTES, ROOT_ROWS,
};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    self, DeviceId, DirHandle, DirId, DirOps, EntryKind, MountRoot, ProcessObserver, VolumeFacts,
    VolumeIdentity, VolumeUuid,
};
use crate::registry::{self, VolumeRecord};
use crate::state::{self, Bounds, Deadline, ReadOutcome, StatePaths, READ_DEADLINE};

/// The owned-root marker's file name inside every root Barge creates.
pub const ROOT_MARKER_FILE: &str = ".barge-root.json";

/// The mode a root directory is created with.
pub const ROOT_DIRECTORY_MODE: u32 = 0o700;

/// The mode a folder Barge creates on the way to an area is created with: a
/// plain directory at the umask, as the design describes for scratch projects.
pub const AREA_DIRECTORY_MODE: u32 = 0o755;

/// The mode every authoritative record file is written with.
pub const RECORD_MODE: u32 = 0o600;

/// How many fresh ids one root creation tries before refusing.
pub const ROOT_ID_ATTEMPTS: usize = 8;

/// The longest consumer-chosen name: `[a-z0-9][a-z0-9._-]{0,63}`.
const MAX_NAME_BYTES: usize = 64;

/// The longest folder Barge records: far below any platform limit and well
/// inside one bounded declaration document.
const MAX_FOLDER_BYTES: usize = 1024;

/// The longest purpose a hold may record.
const MAX_PURPOSE_BYTES: usize = 256;

/// How old a declaration's prune age may be: a century, so a value the platform
/// cannot represent is refused rather than silently truncated.
const MAX_PRUNE_AGE_SECONDS: u64 = 100 * 365 * 24 * 60 * 60;

/// What a caller sees when the volume holding an area is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AbsentPolicy {
    /// A consumer-owned explicit internal path.
    FallbackLocal {
        /// The consumer's own path, absolute and explicit.
        path: PathBuf,
    },
    /// The caller decides; Barge reports the area unavailable.
    Wait,
    /// Fail, naming the drive to attach.
    Fail {
        /// The drive to attach, as the message should name it.
        attach: String,
    },
}

/// How an area's roots may be pruned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrunePolicy {
    /// Never pruned.
    None,
    /// A root whose idleness is older than the declared age may be pruned.
    OldestIdleAfter {
        /// The declared age, in seconds.
        age_seconds: u64,
    },
}

impl PrunePolicy {
    /// The declared age, when the policy has one.
    pub const fn age(&self) -> Option<Duration> {
        match self {
            Self::None => None,
            Self::OldestIdleAfter { age_seconds } => Some(Duration::from_secs(*age_seconds)),
        }
    }
}

/// Whether an area's bytes count toward the volume's reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReserveParticipation {
    /// The area's bytes count.
    Counts,
    /// The area's bytes are excluded.
    Excluded,
}

/// The declaration a consumer gives Barge, and the shape it is stored in.
///
/// Authority is the same whether the declaration is passed in memory or read
/// back from the state directory; the stored document adds only its schema,
/// timestamps and generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaDeclaration {
    /// The consumer-chosen namespace, opaque to Barge and validated for
    /// containment only.
    pub namespace: String,
    /// The consumer-chosen area name.
    pub area: String,
    /// The default folder under the volume's `root_dir`.
    pub folder: String,
    /// Per-volume folder overrides, keyed by volume UUID.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
    /// What a caller sees when the volume is absent.
    pub absent: AbsentPolicy,
    /// How this area's roots may be pruned.
    pub prune: PrunePolicy,
    /// Whether this area's bytes count toward the volume's reserve.
    pub reserve: ReserveParticipation,
}

impl AreaDeclaration {
    /// Validate the declaration's names, folders, overrides and policies.
    ///
    /// Nothing is opened: this is the check that makes sure the descriptor walk
    /// is never handed a name or folder that could escape, and that a policy
    /// Barge cannot represent is refused rather than approximated.
    pub fn validate(&self) -> Result<()> {
        validate_name("namespace", &self.namespace)?;
        validate_name("area", &self.area)?;
        folder_components("folder", &self.folder)?;
        if self.overrides.len() > crate::contract::VOLUME_ROWS {
            return Err(invalid(
                "an area has at most one override per registered volume",
                "overrides",
                &self.overrides.len().to_string(),
            ));
        }
        let mut seen = BTreeSet::new();
        for (volume, folder) in &self.overrides {
            let volume_uuid = VolumeUuid::parse(volume)?;
            if !seen.insert(volume_uuid.clone()) {
                return Err(invalid(
                    "one volume is overridden twice, under two spellings",
                    "overrides",
                    volume_uuid.as_str(),
                ));
            }
            folder_components("overrides", folder)?;
        }
        match &self.absent {
            AbsentPolicy::FallbackLocal { path } => {
                if !path.is_absolute() {
                    return Err(invalid(
                        "a fallback-local path is the consumer's explicit absolute path",
                        "absent.path",
                        &path.display().to_string(),
                    ));
                }
                if path.as_os_str().as_encoded_bytes().contains(&0) {
                    return Err(invalid(
                        "a fallback-local path contains a NUL byte",
                        "absent.path",
                        &path.display().to_string(),
                    ));
                }
            }
            AbsentPolicy::Wait => {}
            AbsentPolicy::Fail { attach } => {
                if attach.trim().is_empty() {
                    return Err(invalid(
                        "an absent-volume failure names the drive to attach",
                        "absent.attach",
                        attach,
                    ));
                }
            }
        }
        if let PrunePolicy::OldestIdleAfter { age_seconds } = &self.prune {
            if *age_seconds == 0 || *age_seconds > MAX_PRUNE_AGE_SECONDS {
                return Err(invalid(
                    "a prune age is a positive number of seconds within a century",
                    "prune.age_seconds",
                    &age_seconds.to_string(),
                ));
            }
        }
        Ok(())
    }

    /// The folder this declaration resolves to on one volume: its override when
    /// one is recorded for that volume, else its default folder.
    pub fn folder_for(&self, volume_uuid: &VolumeUuid) -> Option<&str> {
        self.overrides
            .iter()
            .find(|(key, _)| VolumeUuid::parse(key).is_ok_and(|parsed| &parsed == volume_uuid))
            .map_or(Some(self.folder.as_str()), |(_, folder)| {
                Some(folder.as_str())
            })
    }

    /// The declaration as it is stored: schema, timestamp and generation added,
    /// override keys written in their canonical UUID spelling.
    fn into_document(self, declared_ms: u64, generation: u64) -> Result<AreaDocument> {
        let mut overrides = BTreeMap::new();
        for (volume, folder) in self.overrides {
            overrides.insert(VolumeUuid::parse(&volume)?.as_str().to_owned(), folder);
        }
        Ok(AreaDocument {
            schema: crate::contract::AREA_SCHEMA.to_owned(),
            namespace: self.namespace,
            area: self.area,
            folder: self.folder,
            overrides,
            absent: self.absent,
            prune: self.prune,
            reserve: self.reserve,
            declared_ms,
            generation,
        })
    }
}

/// One `barge.area/1` document: a declaration as the state directory holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AreaDocument {
    /// Always `barge.area/1`; another major is refused, never guessed at.
    pub schema: String,
    /// The consumer-chosen namespace.
    pub namespace: String,
    /// The consumer-chosen area name.
    pub area: String,
    /// The default folder under `root_dir`.
    pub folder: String,
    /// Per-volume overrides, keyed by volume UUID.
    pub overrides: BTreeMap<String, String>,
    /// What a caller sees when the volume is absent.
    pub absent: AbsentPolicy,
    /// How the area's roots may be pruned.
    pub prune: PrunePolicy,
    /// Whether the area's bytes count toward the reserve.
    pub reserve: ReserveParticipation,
    /// When the declaration was written.
    pub declared_ms: u64,
    /// Increases on every change to the same declaration.
    pub generation: u64,
}

impl AreaDocument {
    /// The declaration this document holds.
    pub fn declaration(&self) -> AreaDeclaration {
        AreaDeclaration {
            namespace: self.namespace.clone(),
            area: self.area.clone(),
            folder: self.folder.clone(),
            overrides: self.overrides.clone(),
            absent: self.absent.clone(),
            prune: self.prune.clone(),
            reserve: self.reserve,
        }
    }

    /// Validate the schema, the generation and every field of the declaration.
    pub fn validate(&self) -> Result<()> {
        if self.schema != crate::contract::AREA_SCHEMA {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "`{}` is not the `{}` major this build reads",
                    self.schema,
                    crate::contract::AREA_SCHEMA
                ),
            )
            .with_detail("schema", self.schema.clone().into())
            .with_detail("supported", crate::contract::AREA_SCHEMA.into()));
        }
        if self.generation == 0 {
            return Err(invalid(
                "a declaration's generation starts at 1",
                "generation",
                "0",
            ));
        }
        self.declaration().validate()
    }
}

/// The effective policy a root was created under, recorded exactly.
///
/// It is the caller's declaration with the folder resolved for the volume. A
/// stored declaration never widens it (O-003): when the caller's policies differ
/// from a stored document's, `stored_differs` says so and the caller's values
/// stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveDeclaration {
    /// The consumer-chosen namespace.
    pub namespace: String,
    /// The consumer-chosen area name.
    pub area: String,
    /// The folder this volume resolved to.
    pub folder: String,
    /// What a caller sees when the volume is absent.
    pub absent: AbsentPolicy,
    /// How this area's roots may be pruned.
    pub prune: PrunePolicy,
    /// Whether this area's bytes count toward the reserve.
    pub reserve: ReserveParticipation,
    /// Whether a stored declaration for the same area carries different
    /// policies. The caller's values are still the ones applied.
    pub stored_differs: bool,
}

impl EffectiveDeclaration {
    /// The caller's declaration as the effective policy, with `folder` already
    /// resolved for one volume.
    pub fn new(caller: &AreaDeclaration, stored: Option<&AreaDocument>, folder: String) -> Self {
        let stored_differs = stored.is_some_and(|document| {
            let declaration = document.declaration();
            declaration.absent != caller.absent
                || declaration.prune != caller.prune
                || declaration.reserve != caller.reserve
                || declaration.folder != caller.folder
                || declaration.overrides != caller.overrides
        });
        Self {
            namespace: caller.namespace.clone(),
            area: caller.area.clone(),
            folder,
            absent: caller.absent.clone(),
            prune: caller.prune.clone(),
            reserve: caller.reserve,
            stored_differs,
        }
    }
}

/// One root's kind, as its marker records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// A project run's scratch root.
    ScratchRun,
    /// A cache root.
    CacheRoot,
    /// A plain area root.
    AreaRoot,
}

/// How an owned root came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootOrigin {
    /// Barge created the directory.
    Created,
    /// Barge adopted an existing directory in place (a later slice).
    AdoptedExisting,
}

/// How a concluded run closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConclusionOutcome {
    /// The run closed.
    Closed,
    /// The run left work open.
    Unclosed,
    /// The run failed.
    Failed,
    /// The run was retained on purpose.
    Retained,
}

/// Whether a run's closure is positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConclusionClosure {
    /// The owner claims a positive closure.
    Positive,
    /// An honest non-conclusion.
    None,
}

/// Why a run's root was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptReason {
    /// Nothing is kept.
    None,
    /// Kept as evidence.
    Evidence,
    /// Kept because the run was unclosed.
    Unclosed,
    /// Kept as an exception.
    Exception,
    /// Kept because a nested run was unclosed.
    #[serde(rename = "nested-unclosed")]
    NestedUnclosed,
}

/// The scope of a run's closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NestedScope {
    /// The run owned everything inside it.
    Exclusive,
    /// The run hosted resources another authority created.
    Hosting,
}

/// One `scratch_run` root's conclusion, recorded once by its owner (S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConclusion {
    /// When the conclusion was written.
    pub concluded_ms: u64,
    /// How the run closed.
    pub outcome: ConclusionOutcome,
    /// Whether closure is positive.
    pub closure: ConclusionClosure,
    /// Why the root was kept.
    pub kept_reason: KeptReason,
    /// What the conclusion covers.
    pub nested_scope: NestedScope,
}

/// The area a root belongs to, with the effective declaration it was created
/// under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkerArea {
    /// The consumer-chosen namespace.
    pub namespace: String,
    /// The consumer-chosen area name.
    pub area: String,
    /// The effective declaration applied at creation, so a later reader can see
    /// which policies produced the root. The two-field form is valid and means
    /// "identity only"; a missing value never widens anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective: Option<EffectiveDeclaration>,
}

impl MarkerArea {
    /// The identity-only form, as the design pins it.
    pub fn identity(namespace: &str, area: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            area: area.to_owned(),
            effective: None,
        }
    }
}

/// One `barge.root/1` marker: the identity and provenance of an owned root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootMarker {
    /// Always `barge.root/1`.
    pub schema: String,
    /// The root's fresh 128-bit id, as 32 hexadecimal digits.
    pub root_id: String,
    /// The root's fresh 128-bit nonce, as 32 hexadecimal digits.
    pub nonce: String,
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// Always `barge`: a marker is only ever Barge's own.
    pub owner: String,
    /// What the root is for.
    pub kind: RootKind,
    /// The area the root lives in.
    pub area: MarkerArea,
    /// The absolute path Barge resolved when it created the root. It is an
    /// observation for humans; the descriptor walk remains the authority.
    pub resolved_path: PathBuf,
    /// When the marker was written.
    pub created_ms: u64,
    /// Increases on every change; a root starts at 1.
    pub generation: u64,
    /// Whether Barge created the directory or adopted it in place.
    pub origin: RootOrigin,
    /// The run's conclusion, `null` while it is running.
    #[serde(default)]
    pub conclusion: Option<RootConclusion>,
}

/// What reading one root's marker found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker at all: the directory is not owned and never adopted.
    Absent,
    /// A marker that validated and matches the volume and path it was read at.
    Owned(Box<RootMarker>),
    /// A marker that is there but is not owned: absent, foreign, symlinked,
    /// oversized, future-dated, unknown-version or schema-divergent. It is never
    /// reused, never deleted and always reported.
    Unknown {
        /// Why the marker is not owned, for a human.
        reason: String,
    },
}

impl MarkerState {
    /// The trait-object-free name a document reports.
    pub const fn name(&self) -> MarkerStateName {
        match self {
            Self::Absent => MarkerStateName::Absent,
            Self::Owned(_) => MarkerStateName::Owned,
            Self::Unknown { .. } => MarkerStateName::Unknown,
        }
    }
}

/// The marker states a document reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerStateName {
    /// An owned root.
    Owned,
    /// No marker.
    Absent,
    /// A marker that is not owned.
    Unknown,
}

/// What a marker must say to be this volume's, at this path, for this user.
#[derive(Debug, Clone, Copy)]
pub struct MarkerExpectation<'a> {
    /// The volume UUID the verified volume reported.
    pub volume_uuid: &'a VolumeUuid,
    /// The absolute path the descriptor walk actually reached.
    pub resolved_path: &'a Path,
    /// The account's effective uid.
    pub effective_uid: u32,
    /// The current time, so a future-dated marker is refused.
    pub now_ms: u64,
}

/// One owned root, as creation returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedRoot {
    /// The fresh 128-bit id, as 32 hexadecimal digits.
    pub root_id: String,
    /// The marker's fresh nonce.
    pub nonce: String,
    /// What the root is for.
    pub kind: RootKind,
    /// The area identity the root belongs to.
    pub area: MarkerArea,
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The absolute path the descriptor walk reached.
    pub path: PathBuf,
    /// The root directory's own device and inode.
    pub identity: DirId,
    /// The marker's SHA-256, so a later reader can re-verify the exact bytes.
    pub marker_digest: String,
    /// When the root was created.
    pub created_ms: u64,
}

/// What a caller asks [`create_root`] to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSpec {
    /// What the root is for.
    pub kind: RootKind,
}

/// A verified volume root, opened as a descriptor.
///
/// Holding one proves the volume's facts matched the record when it was opened.
/// It proves nothing about the next moment, so every resolution re-reads the
/// facts before it writes.
#[derive(Debug)]
pub struct VolumeRoot {
    handle: DirHandle,
    identity: VolumeIdentity,
    mount_point: PathBuf,
}

impl VolumeRoot {
    /// Verify the record's volume and open its mount root as a descriptor.
    ///
    /// This is the first step of containment: the identity is read through an
    /// opened descriptor, the kernel's mount root is proved, and the opened
    /// directory's own device must match the volume's, so no walk starts on a
    /// path that is not the volume it claims.
    pub fn open(
        dirs: &dyn DirOps,
        facts: &dyn VolumeFacts,
        mounts: &dyn MountRoot,
        record: &VolumeRecord,
    ) -> Result<Self> {
        let mount_hint = record.mount_hint.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "volume {} has no recorded mount point, so Barge cannot find it",
                    record.volume_uuid
                ),
            )
            .with_detail("volume_uuid", record.volume_uuid.as_str().into())
        })?;
        let verified = os::verify_volume(facts, mounts, mount_hint, &record.expected_volume())?;
        let handle = dirs.open_path(verified.path())?;
        if handle.identity().device != verified.identity().device {
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "the mount root {} opened on device {}, but the volume reported {}",
                    verified.path().display(),
                    handle.identity().device,
                    verified.identity().device
                ),
            )
            .with_detail("path", verified.path().display().to_string().into())
            .with_detail("opened_device", handle.identity().device.to_string().into())
            .with_detail(
                "volume_device",
                verified.identity().device.to_string().into(),
            ));
        }
        Ok(Self {
            handle,
            identity: verified.identity().clone(),
            mount_point: verified.path().to_path_buf(),
        })
    }

    /// The verified facts.
    pub fn identity(&self) -> &VolumeIdentity {
        &self.identity
    }

    /// The proved mount root.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// The opened mount root, taken out so a walk can start from it.
    pub fn into_handle(self) -> DirHandle {
        self.handle
    }

    /// Re-read the volume's facts and refuse when anything differs, so a caller
    /// that is about to write proves the volume is still the recorded one.
    pub fn reverify(&self, facts: &dyn VolumeFacts) -> Result<()> {
        let again = facts.identify(&self.mount_point)?;
        if again == self.identity {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::VolumeMismatch,
            format!(
                "the volume at {} changed since it was verified",
                self.mount_point.display()
            ),
        )
        .with_detail("path", self.mount_point.display().to_string().into())
        .with_detail("verified_device", self.identity.device.to_string().into())
        .with_detail("actual_device", again.device.to_string().into()))
    }
}

/// A resolved folder: the deepest directory that exists, the components still
/// to create, and the absolute path the configuration resolves to.
#[derive(Debug)]
struct Walk {
    handle: DirHandle,
    remaining: Vec<String>,
    path: PathBuf,
}

impl Walk {
    fn exists(&self) -> bool {
        self.remaining.is_empty()
    }
}

/// Validate one consumer-chosen name: `[a-z0-9][a-z0-9._-]{0,63}`, one path
/// component.
pub fn validate_name(field: &str, value: &str) -> Result<()> {
    let first_ok = value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let rest_ok = value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
    });
    if last_ok(value) && first_ok && rest_ok && value.len() <= MAX_NAME_BYTES {
        return Ok(());
    }
    Err(containment(
        field,
        value,
        "a name is one component matching `[a-z0-9][a-z0-9._-]{0,63}`",
    ))
}

fn last_ok(value: &str) -> bool {
    !value.is_empty() && !value.contains('/') && !value.contains('\0')
}

/// Validate a folder and split it into its components.
///
/// A folder is a relative path inside the volume: one or more components, none
/// empty, none `.` or `..`, no absolute prefix, no leading or trailing
/// separator, no NUL and no component over the platform's own name limit. The
/// descriptor walk is still the authority; this makes sure it is never handed a
/// string that could escape.
pub fn folder_components(field: &str, folder: &str) -> Result<Vec<String>> {
    if folder.is_empty() {
        return Err(containment(
            field,
            folder,
            "a folder has at least one component",
        ));
    }
    if folder.len() > MAX_FOLDER_BYTES {
        return Err(containment(
            field,
            folder,
            "a folder is longer than Barge records",
        ));
    }
    if folder.contains('\0') {
        return Err(containment(field, folder, "a folder contains a NUL byte"));
    }
    if folder.starts_with('/') {
        return Err(containment(
            field,
            folder,
            "a folder is relative to the volume, not absolute",
        ));
    }
    if folder.ends_with('/') {
        return Err(containment(
            field,
            folder,
            "a folder has no trailing separator",
        ));
    }
    let mut components = Vec::new();
    for component in folder.split('/') {
        if component.is_empty() {
            return Err(containment(
                field,
                folder,
                "a folder has an empty component",
            ));
        }
        if component == "." || component == ".." {
            return Err(containment(
                field,
                folder,
                "a folder has a `.` or `..` component",
            ));
        }
        if component.len() > MAX_NAME_BYTES * 4 {
            return Err(containment(
                field,
                folder,
                "a folder component is longer than the platform allows",
            ));
        }
        components.push(component.to_owned());
    }
    Ok(components)
}

fn containment(field: &str, value: &str, reason: &str) -> Error {
    Error::new(
        ErrorKind::RefusedContainment,
        format!("`{value}` is not a contained folder: {reason}"),
    )
    .with_detail("field", field.into())
    .with_detail("value", value.into())
    .with_detail("reason", reason.into())
}

fn invalid(message: &str, field: &str, value: &str) -> Error {
    Error::new(ErrorKind::UnknownState, message.to_owned())
        .with_detail("field", field.into())
        .with_detail("value", value.into())
}

/// Resolve a container of folder components under `start`, one descriptor step
/// at a time.
///
/// Each component is checked with `lstat`, refused when it is not a real
/// directory of this user on this device, opened with no-follow, and its opened
/// identity compared with the one that was checked: an entry that changed
/// between the two steps is a containment refusal. The first absent component
/// ends the walk, and the rest are reported as still to create.
fn walk_folder(
    dirs: &dyn DirOps,
    start: DirHandle,
    components: &[String],
    device: DeviceId,
    prefix: &Path,
    effective_uid: u32,
) -> Result<Walk> {
    let mut handle = start;
    let mut path = prefix.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let checked = dirs.child_facts(&handle, component)?;
        let Some(checked) = checked else {
            let remaining = components[index..].to_vec();
            for rest in &remaining {
                path.push(rest);
            }
            return Ok(Walk {
                handle,
                remaining,
                path,
            });
        };
        if checked.kind != EntryKind::Directory {
            return Err(containment(
                "folder",
                component,
                &format!(
                    "the component is a {}, and only a real directory is walked",
                    checked.kind.as_str()
                ),
            ));
        }
        if checked.device != device {
            return Err(containment(
                "folder",
                component,
                &format!(
                    "the component is on device {}, not the volume's {}",
                    checked.device, device
                ),
            ));
        }
        if checked.uid != effective_uid {
            return Err(containment(
                "folder",
                component,
                &format!(
                    "the component is owned by uid {}, not this account's {effective_uid}",
                    checked.uid
                ),
            ));
        }
        let opened = dirs.open_child_dir(&handle, component)?;
        let checked_identity = DirId {
            device: checked.device,
            inode: checked.inode,
        };
        if opened.identity() != checked_identity {
            return Err(containment(
                "folder",
                component,
                "the component changed between the check and the open",
            ));
        }
        if opened.identity().device != device {
            return Err(containment(
                "folder",
                component,
                "the opened component is on another device",
            ));
        }
        handle = opened;
        path.push(component);
    }
    Ok(Walk {
        handle,
        remaining: Vec::new(),
        path,
    })
}

/// A declared area resolved for one volume: the deepest directory that exists,
/// the components still to create, and the effective declaration.
#[derive(Debug)]
pub struct ResolvedArea {
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The proved mount root.
    pub mount_point: PathBuf,
    /// The volume's recorded `root_dir`; `""` is the mount root.
    pub root_dir: String,
    /// The effective folder for this volume.
    pub folder: String,
    /// The absolute path the configuration resolves to. It is an observation
    /// for humans; the descriptor walk is the authority.
    pub path: PathBuf,
    /// The effective declaration, recorded exactly.
    pub declaration: EffectiveDeclaration,
    dirs: Direction,
}

/// Where the directory walk stands, hidden so a caller cannot forge one.
#[derive(Debug)]
enum Direction {
    /// The area folder exists; the handle is it.
    Existing { handle: DirHandle },
    /// The area folder does not exist; these components remain to create.
    Missing {
        handle: DirHandle,
        remaining: Vec<String>,
        device: DeviceId,
    },
}

impl ResolvedArea {
    /// Whether the area folder exists. Resolution creates nothing.
    pub fn exists(&self) -> bool {
        matches!(self.dirs, Direction::Existing { .. })
    }
}

/// Resolve one declaration for one volume, creating nothing.
///
/// The volume is verified, `root_dir` and then the effective folder are walked
/// from the verified mount root, and the effective declaration is built from the
/// caller's declaration: a stored document is reported as differing but never
/// widens a policy (O-003).
pub fn resolve(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    caller: &AreaDeclaration,
    stored: Option<&AreaDocument>,
) -> Result<ResolvedArea> {
    caller.validate()?;
    if let Some(document) = stored {
        document.validate()?;
        if document.namespace != caller.namespace || document.area != caller.area {
            return Err(Error::new(
                ErrorKind::Usage,
                "the stored declaration is for a different area than the caller's",
            )
            .with_detail(
                "stored",
                format!("{}/{}", document.namespace, document.area).into(),
            )
            .with_detail(
                "caller",
                format!("{}/{}", caller.namespace, caller.area).into(),
            ));
        }
    }
    // An undeclared durability refuses every area use, before a byte is read or
    // written: expending data is never inferred.
    record.declared_durability()?;
    let root = VolumeRoot::open(dirs, facts, mounts, record)?;
    let effective_uid = observer.effective_uid()?;
    let device = root.identity().device;
    let base = root.mount_point().to_path_buf();
    let root_handle = root.into_handle();

    let root_dir_components = if record.root_dir.is_empty() {
        Vec::new()
    } else {
        folder_components("root_dir", &record.root_dir)?
    };
    let walked_root = walk_folder(
        dirs,
        root_handle,
        &root_dir_components,
        device,
        &base,
        effective_uid,
    )?;

    let folder = effective_folder(record, caller)?;
    let folder_components = folder_components("folder", &folder)?;
    let walked = if walked_root.exists() {
        walk_folder(
            dirs,
            walked_root.handle,
            &folder_components,
            device,
            &walked_root.path,
            effective_uid,
        )?
    } else {
        // The root_dir itself is missing, so everything under it is still to
        // create; the path is the composed observation either way.
        let mut path = walked_root.path.clone();
        for component in &folder_components {
            path.push(component);
        }
        Walk {
            handle: walked_root.handle,
            remaining: walked_root
                .remaining
                .iter()
                .chain(folder_components.iter())
                .cloned()
                .collect(),
            path,
        }
    };

    let declaration = EffectiveDeclaration::new(caller, stored, folder);
    let dirs_state = if walked.exists() {
        Direction::Existing {
            handle: walked.handle,
        }
    } else {
        Direction::Missing {
            handle: walked.handle,
            remaining: walked.remaining,
            device,
        }
    };
    Ok(ResolvedArea {
        volume_uuid: record.volume_uuid.clone(),
        mount_point: base,
        root_dir: record.root_dir.clone(),
        folder: declaration.folder.clone(),
        path: walked.path,
        declaration,
        dirs: dirs_state,
    })
}

/// The folder a declaration resolves to on one volume: an override recorded for
/// that volume, else the default folder.
pub fn effective_folder(record: &VolumeRecord, declaration: &AreaDeclaration) -> Result<String> {
    for (volume, folder) in &declaration.overrides {
        if VolumeUuid::parse(volume).is_ok_and(|parsed| parsed == record.volume_uuid) {
            folder_components("overrides", folder)?;
            return Ok(folder.clone());
        }
    }
    folder_components("folder", &declaration.folder)?;
    Ok(declaration.folder.clone())
}

/// Open one area folder descriptor-relative from the verified mount root,
/// creating nothing.
///
/// This is the walk a replay needs: the intent records `root_dir` and the folder
/// it acted in and nothing more, so the folder is resolved again from the
/// record's verified volume. `Ok(None)` means the folder is positively not
/// there, which is a different fact from an unreadable one.
pub fn open_folder(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    root_dir: &str,
    folder: &str,
) -> Result<Option<(PathBuf, DirHandle)>> {
    record.declared_durability()?;
    let volume = VolumeRoot::open(dirs, facts, mounts, record)?;
    let effective_uid = observer.effective_uid()?;
    let device = volume.identity().device;
    let base = volume.mount_point().to_path_buf();
    let mut components = if root_dir.is_empty() {
        Vec::new()
    } else {
        folder_components("root_dir", root_dir)?
    };
    components.extend(folder_components("folder", folder)?);
    let walked = walk_folder(
        dirs,
        volume.into_handle(),
        &components,
        device,
        &base,
        effective_uid,
    )?;
    if !walked.exists() {
        return Ok(None);
    }
    Ok(Some((walked.path, walked.handle)))
}

/// Create one owned root inside the resolved area.
///
/// The area folder (and any missing component of it) is created as a plain
/// directory of this user, then one root is created exclusively with a fresh
/// 128-bit id, `0700`, and its marker written atomically before any consumer
/// payload. A name that is already taken is retried with a new id, so two
/// creators can never receive the same root.
pub fn create_root(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    resolved: &ResolvedArea,
    spec: RootSpec,
) -> Result<OwnedRoot> {
    let effective_uid = observer.effective_uid()?;
    let area_handle = ensure_area_folder(dirs, resolved, effective_uid)?;
    let mut last_error: Option<Error> = None;
    for _ in 0..ROOT_ID_ATTEMPTS {
        let root_id = hex(16, dirs.random_bytes(16)?);
        let nonce = hex(16, dirs.random_bytes(16)?);
        match dirs.create_child_dir(&area_handle, &root_id, ROOT_DIRECTORY_MODE) {
            Ok(identity) => {
                let root = dirs.open_child_dir(&area_handle, &root_id)?;
                if root.identity() != identity {
                    return Err(Error::new(
                        ErrorKind::RefusedContainment,
                        format!("the root `{root_id}` changed while it was created"),
                    )
                    .with_detail("root_id", root_id.into()));
                }
                return write_root_marker(
                    dirs,
                    resolved,
                    &root,
                    &root_id,
                    &nonce,
                    spec,
                    effective_uid,
                );
            }
            Err(error) => {
                // A taken name is the race the design expects: retry with a
                // fresh id. Anything else is the platform's own failure.
                if dirs.child_facts(&area_handle, &root_id)?.is_some() {
                    last_error = Some(error);
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err(Error::new(
        ErrorKind::ResourceBound,
        format!("could not find a fresh root id after {ROOT_ID_ATTEMPTS} attempts"),
    )
    .with_detail("attempts", ROOT_ID_ATTEMPTS.into())
    .with_detail(
        "last_error",
        last_error
            .map_or_else(String::new, |error| error.to_string())
            .into(),
    ))
}

/// One volume's record and its resolved area, as a command prepares them.
#[derive(Debug)]
pub struct VolumeArea {
    /// The registered volume record the use was authorized by.
    pub record: VolumeRecord,
    /// The resolved area.
    pub resolved: ResolvedArea,
}

/// Look one registered volume up and refuse when it is not registered.
///
/// A volume that is not in the registry is unknown state naming the UUID: Barge
/// never treats an unregistered volume as usable, and it never registers one
/// here.
pub fn registered_volume(state: &StatePaths, volume_uuid: &VolumeUuid) -> Result<VolumeRecord> {
    let registry = registry::read(state)?;
    registry.volume(volume_uuid).cloned().ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            format!("volume {volume_uuid} is not registered"),
        )
        .with_detail("volume_uuid", volume_uuid.as_str().into())
    })
}

/// Prepare one declared area on one registered volume, mutating nothing.
///
/// This is the CLI's route: the registry is read (an invalid registry refuses),
/// the volume must be registered and its durability declared, and the area is
/// resolved through the descriptor walk. No folder is created here.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
) -> Result<VolumeArea> {
    let record = registered_volume(state, volume_uuid)?;
    let resolved = resolve(dirs, facts, mounts, observer, &record, declaration, stored)?;
    Ok(VolumeArea { record, resolved })
}

/// Create one owned root inside a declared area on a registered volume.
///
/// The volume lock is held across the resolution and the creation, so two Barge
/// commands cannot create folder structure on one volume at the same time. The
/// registry lock is not held across the walk. Nothing is created when the
/// volume is absent or mismatched, the durability is undeclared, the registry is
/// invalid or the declaration does not contain its folder.
#[allow(clippy::too_many_arguments)]
pub fn create_root_on_volume(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
    spec: RootSpec,
) -> Result<(VolumeArea, OwnedRoot)> {
    // Every refusal the volume itself can cause happens before any effect: an
    // unregistered UUID, an undeclared durability, an absent volume and a
    // different volume at the recorded mount are all proved here, so a refused
    // creation leaves no state layout, no lock file and no folder behind.
    let record = registered_volume(state, volume_uuid)?;
    record.declared_durability()?;
    VolumeRoot::open(dirs, facts, mounts, &record)?;
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let _volume_lock = locks.lock_volume(volume_uuid)?;
    let volume = prepare(
        state,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        declaration,
        stored,
    )?;
    let root = create_root(dirs, observer, &volume.resolved, spec)?;
    Ok((volume, root))
}

/// List one declared area's roots on a registered volume, mutating nothing.
#[allow(clippy::too_many_arguments)]
pub fn list_roots_on_volume(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
    max_roots: usize,
) -> Result<(VolumeArea, RootListing)> {
    let volume = prepare(
        state,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        declaration,
        stored,
    )?;
    let listing = list_roots(dirs, observer, state, &volume.resolved, max_roots)?;
    Ok((volume, listing))
}

/// Adopt one component that is already there, only when the opened directory is
/// the one that was checked.
///
/// Every adoption path in the folder creation goes through this: the entry must
/// be a real directory of this account on this device, it is opened without
/// following a link, and the opened device and inode must equal the checked
/// ones. An entry that changed between `lstat` and `open` — including one a
/// competing creator swapped in behind the race — is refused rather than
/// adopted, so no path creates a root inside a directory the walk did not prove.
fn adopt_existing_component(
    dirs: &dyn DirOps,
    parent: &DirHandle,
    component: &str,
    facts: &crate::os::EntryFacts,
    device: DeviceId,
    effective_uid: u32,
) -> Result<DirHandle> {
    if facts.kind != EntryKind::Directory || facts.device != device || facts.uid != effective_uid {
        return Err(containment(
            "folder",
            component,
            "an existing component is not this account's directory on this volume",
        ));
    }
    let opened = dirs.open_child_dir(parent, component)?;
    let checked_identity = DirId {
        device: facts.device,
        inode: facts.inode,
    };
    if opened.identity() != checked_identity {
        return Err(containment(
            "folder",
            component,
            "the component changed between the check and the open",
        ));
    }
    if opened.identity().device != device {
        return Err(containment(
            "folder",
            component,
            "the opened component is on another device",
        ));
    }
    Ok(opened)
}

/// Create any missing folder component on the way to the area, refusing to
/// adopt anything that is already there but is not this account's directory.
fn ensure_area_folder(
    dirs: &dyn DirOps,
    resolved: &ResolvedArea,
    effective_uid: u32,
) -> Result<DirHandle> {
    match &resolved.dirs {
        Direction::Existing { handle } => dirs.duplicate_dir(handle),
        Direction::Missing {
            handle,
            remaining,
            device,
        } => {
            let mut current = dirs.duplicate_dir(handle)?;
            let mut position = 0;
            while position < remaining.len() {
                let component = remaining[position].clone();
                let existing = dirs.child_facts(&current, &component)?;
                let handle = match existing {
                    Some(facts) => adopt_existing_component(
                        dirs,
                        &current,
                        &component,
                        &facts,
                        *device,
                        effective_uid,
                    )?,
                    None => {
                        match dirs.create_child_dir(&current, &component, AREA_DIRECTORY_MODE) {
                            Ok(identity) => {
                                let opened = dirs.open_child_dir(&current, &component)?;
                                if opened.identity() != identity {
                                    return Err(containment(
                                        "folder",
                                        &component,
                                        "the component changed while it was created",
                                    ));
                                }
                                opened
                            }
                            Err(error) => {
                                // A concurrent creator may have won: adopt the
                                // component only through the same lstat-then-open
                                // identity check the named path uses, so a swap
                                // behind the race is refused, not adopted. With
                                // nothing there, the platform's failure stands.
                                let Some(facts) = dirs.child_facts(&current, &component)? else {
                                    return Err(error);
                                };
                                adopt_existing_component(
                                    dirs,
                                    &current,
                                    &component,
                                    &facts,
                                    *device,
                                    effective_uid,
                                )?
                            }
                        }
                    }
                };
                current = handle;
                position += 1;
            }
            Ok(current)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_root_marker(
    dirs: &dyn DirOps,
    resolved: &ResolvedArea,
    root: &DirHandle,
    root_id: &str,
    nonce: &str,
    spec: RootSpec,
    effective_uid: u32,
) -> Result<OwnedRoot> {
    let created_ms = state::now_ms();
    let path = resolved.path.join(root_id);
    let marker = RootMarker {
        schema: crate::contract::ROOT_SCHEMA.to_owned(),
        root_id: root_id.to_owned(),
        nonce: nonce.to_owned(),
        volume_uuid: resolved.volume_uuid.clone(),
        owner: "barge".to_owned(),
        kind: spec.kind,
        area: MarkerArea {
            namespace: resolved.declaration.namespace.clone(),
            area: resolved.declaration.area.clone(),
            effective: Some(resolved.declaration.clone()),
        },
        resolved_path: path.clone(),
        created_ms,
        generation: 1,
        origin: RootOrigin::Created,
        conclusion: None,
    };
    marker.validate(&MarkerExpectation {
        volume_uuid: &resolved.volume_uuid,
        resolved_path: &path,
        effective_uid,
        now_ms: created_ms,
    })?;
    let mut bytes = serde_json::to_vec_pretty(&marker).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the root marker: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > MARKER_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the root marker would be {} bytes, over the {MARKER_CAP_BYTES}-byte cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", MARKER_CAP_BYTES.into()));
    }
    let digest = sha256_hex(&bytes);
    dirs.publish_child_file(root, ROOT_MARKER_FILE, &bytes, RECORD_MODE)?;
    // Read the marker back so creation proves what is on disk, not what was
    // meant to be written.
    let expectation = MarkerExpectation {
        volume_uuid: &resolved.volume_uuid,
        resolved_path: &path,
        effective_uid,
        now_ms: created_ms,
    };
    match read_marker(dirs, root, &expectation) {
        MarkerState::Owned(back) if back.root_id == root_id => {}
        other => {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the marker just written for root `{root_id}` did not validate on re-read: {}",
                    describe_marker(&other)
                ),
            )
            .with_detail("root_id", root_id.into()));
        }
    }
    Ok(OwnedRoot {
        root_id: root_id.to_owned(),
        nonce: nonce.to_owned(),
        kind: spec.kind,
        area: marker.area,
        volume_uuid: resolved.volume_uuid.clone(),
        path,
        identity: root.identity(),
        marker_digest: digest,
        created_ms,
    })
}

fn describe_marker(state: &MarkerState) -> String {
    match state {
        MarkerState::Absent => "it is absent".to_owned(),
        MarkerState::Owned(_) => "it is owned".to_owned(),
        MarkerState::Unknown { reason } => reason.clone(),
    }
}

/// Read and validate one root's marker strictly.
///
/// The marker must be a private regular file of this user, at most the marker
/// cap, and must parse with exactly the fields `barge.root/1` defines. It is
/// owned only when its volume UUID, resolved path, id and nonce shapes and
/// timestamps all agree with what was actually walked. Everything else is
/// unknown, never reused and never deletable.
pub fn read_marker(
    dirs: &dyn DirOps,
    root: &DirHandle,
    expectation: &MarkerExpectation<'_>,
) -> MarkerState {
    read_marker_with_digest(dirs, root, expectation).0
}

/// Read one root's marker and the SHA-256 of the exact bytes read.
///
/// The reader compares no digest itself: the expected digest belongs to the
/// caller that recorded the root. Custody and replay are that caller, and this
/// is how they prove the marker is the one they were told about rather than a
/// different file of the same shape.
pub fn read_marker_with_digest(
    dirs: &dyn DirOps,
    root: &DirHandle,
    expectation: &MarkerExpectation<'_>,
) -> (MarkerState, Option<String>) {
    let facts = match dirs.child_facts(root, ROOT_MARKER_FILE) {
        Ok(Some(facts)) => facts,
        Ok(None) => return (MarkerState::Absent, None),
        Err(error) => {
            return (
                MarkerState::Unknown {
                    reason: error.to_string(),
                },
                None,
            )
        }
    };
    let unknown = |reason: String| (MarkerState::Unknown { reason }, None);
    if facts.kind != EntryKind::File {
        return unknown(format!(
            "the marker is a {}, and Barge only owns a regular file",
            facts.kind.as_str()
        ));
    }
    if facts.uid != expectation.effective_uid {
        return unknown(format!(
            "the marker is owned by uid {}, not this account's {}",
            facts.uid, expectation.effective_uid
        ));
    }
    if !facts.is_private() {
        return unknown(format!("the marker's mode {:o} is not private", facts.mode));
    }
    if facts.size > MARKER_CAP_BYTES as u64 {
        return unknown(format!(
            "the marker is {} bytes, over the {MARKER_CAP_BYTES}-byte cap",
            facts.size
        ));
    }
    let bytes = match dirs.read_child_file(root, ROOT_MARKER_FILE, MARKER_CAP_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return unknown("the marker disappeared while it was read".to_owned()),
        Err(error) => return unknown(error.to_string()),
    };
    let digest = sha256_hex(&bytes);
    let marker: RootMarker = match serde_json::from_slice(&bytes) {
        Ok(marker) => marker,
        Err(error) => {
            return unknown(format!(
                "the marker is not a `{}` document: {error}",
                crate::contract::ROOT_SCHEMA
            ))
        }
    };
    // The bytes were read, so the digest is a fact even when the marker does not
    // validate where it was found: custody and replay carry the expected digest
    // themselves, and a quarantined leaf's marker necessarily fails the path
    // check after the rename.
    match marker.validate(expectation) {
        Ok(()) => (MarkerState::Owned(Box::new(marker)), Some(digest)),
        Err(error) => (
            MarkerState::Unknown {
                reason: error.to_string(),
            },
            Some(digest),
        ),
    }
}

impl RootMarker {
    /// Validate every field of a marker against what was actually walked.
    fn validate(&self, expectation: &MarkerExpectation<'_>) -> Result<()> {
        if self.schema != crate::contract::ROOT_SCHEMA {
            return Err(unknown_marker(format!(
                "`{}` is not the `{}` major this build reads",
                self.schema,
                crate::contract::ROOT_SCHEMA
            )));
        }
        state::validate_identifier("root_id", &self.root_id)?;
        state::validate_identifier("nonce", &self.nonce)?;
        if &self.volume_uuid != expectation.volume_uuid {
            return Err(unknown_marker(format!(
                "the marker records volume {}, not the verified {}",
                self.volume_uuid, expectation.volume_uuid
            )));
        }
        if self.owner != "barge" {
            return Err(unknown_marker(format!(
                "the marker's owner is `{}`, not Barge",
                self.owner
            )));
        }
        validate_name("area.namespace", &self.area.namespace)?;
        validate_name("area.area", &self.area.area)?;
        if self.resolved_path != expectation.resolved_path {
            return Err(unknown_marker(format!(
                "the marker records {}, but the walk reached {}",
                self.resolved_path.display(),
                expectation.resolved_path.display()
            )));
        }
        if self.created_ms > expectation.now_ms {
            return Err(unknown_marker(format!(
                "the marker is dated {} ms in the future",
                self.created_ms - expectation.now_ms
            )));
        }
        if self.generation == 0 {
            return Err(unknown_marker(
                "the marker's generation starts at 1".to_owned(),
            ));
        }
        match (&self.conclusion, self.kind) {
            (Some(conclusion), RootKind::ScratchRun) => {
                if conclusion.concluded_ms > expectation.now_ms {
                    return Err(unknown_marker(
                        "the conclusion is dated in the future".to_owned(),
                    ));
                }
            }
            (Some(_), _) => {
                return Err(unknown_marker(
                    "only a scratch run carries a conclusion".to_owned(),
                ))
            }
            (None, _) => {}
        }
        if let Some(effective) = &self.area.effective {
            if effective.namespace != self.area.namespace || effective.area != self.area.area {
                return Err(unknown_marker(
                    "the recorded effective declaration is for another area".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

fn unknown_marker(reason: String) -> Error {
    Error::new(ErrorKind::UnknownState, reason)
}

/// The bounded set of stored declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationSet {
    /// The declarations kept, sorted by namespace and area.
    pub documents: Vec<AreaDocument>,
    /// How many the state directory held, at least `documents.len()`.
    pub total: usize,
    /// Whether rows were left out.
    pub truncated: bool,
}

impl DeclarationSet {
    /// No declarations.
    pub fn empty() -> Self {
        Self {
            documents: Vec::new(),
            total: 0,
            truncated: false,
        }
    }

    /// The document for one area, if it is declared.
    pub fn find(&self, namespace: &str, area: &str) -> Option<&AreaDocument> {
        self.documents
            .iter()
            .find(|document| document.namespace == namespace && document.area == area)
    }

    /// The rows a document reports.
    pub fn rows(&self) -> RowSet<AreaDocument> {
        RowSet::bounded(self.documents.clone(), self.total, AREA_ROWS)
    }
}

/// Where one declaration lives in the state directory.
pub fn declaration_path(state: &StatePaths, namespace: &str, area: &str) -> Result<PathBuf> {
    validate_name("namespace", namespace)?;
    validate_name("area", area)?;
    Ok(state
        .areas_dir()
        .join(namespace)
        .join(format!("{area}.json")))
}

/// Read every stored declaration strictly, within the area row cap and the
/// declaration size cap.
///
/// A namespace directory, a declaration file or a document that does not
/// validate is unknown state naming the path: an unreadable declaration is never
/// read as absent, because a silently short list would read as "no areas".
pub fn read_declarations(state: &StatePaths) -> Result<DeclarationSet> {
    let areas_dir = state.areas_dir();
    let namespaces = match read_directory(&areas_dir)? {
        None => return Ok(DeclarationSet::empty()),
        Some(namespaces) => namespaces,
    };
    let mut documents = Vec::new();
    let mut total = 0usize;
    for namespace_entry in namespaces {
        let namespace = namespace_entry;
        validate_name("namespace", &namespace)?;
        let namespace_dir = areas_dir.join(&namespace);
        let Some(entries) = read_directory(&namespace_dir)? else {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "the namespace directory {} disappeared while it was read",
                    namespace_dir.display()
                ),
            )
            .with_detail("path", namespace_dir.display().to_string().into()));
        };
        for file in entries {
            let Some(area) = file.strip_suffix(".json") else {
                continue;
            };
            validate_name("area", area)?;
            let path = namespace_dir.join(&file);
            let bytes = match state::read_bounded(
                &path,
                &Bounds::new(
                    AREA_DECLARATION_CAP_BYTES,
                    AREA_ROWS,
                    Deadline::after(READ_DEADLINE),
                ),
            ) {
                ReadOutcome::Present(bytes) => bytes,
                ReadOutcome::Absent => continue,
                ReadOutcome::Refused(error) => {
                    return Err(error.with_detail("path", path.display().to_string().into()))
                }
            };
            let document: AreaDocument = serde_json::from_slice(&bytes).map_err(|error| {
                Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "`{}` refused the declaration: {error}",
                        crate::contract::AREA_SCHEMA
                    ),
                )
                .with_detail("path", path.display().to_string().into())
            })?;
            document.validate().map_err(|error| {
                Error::new(
                    ErrorKind::UnknownState,
                    format!("{} is not a valid declaration: {error}", path.display()),
                )
                .with_detail("path", path.display().to_string().into())
            })?;
            if document.namespace != namespace || document.area != area {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "{} declares {}/{} but lives at {namespace}/{area}",
                        path.display(),
                        document.namespace,
                        document.area
                    ),
                )
                .with_detail("path", path.display().to_string().into()));
            }
            total = total.saturating_add(1);
            if documents.len() < AREA_ROWS {
                documents.push(document);
            }
        }
    }
    documents
        .sort_by(|left, right| (&left.namespace, &left.area).cmp(&(&right.namespace, &right.area)));
    Ok(DeclarationSet {
        documents,
        total,
        truncated: total > AREA_ROWS,
    })
}

/// One directory's entry names, sorted, or `None` when it does not exist.
///
/// A symlink or a non-directory at the path is unknown state: Barge reads its
/// own state directory and never through a link.
fn read_directory(path: &Path) -> Result<Option<Vec<String>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("{} is not a real directory", path.display()),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    let mut names = Vec::new();
    let entries = std::fs::read_dir(path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot list {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot list {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(Some(names))
}

/// Declare one area, refusing a collision before anything is written.
///
/// The declaration is validated, the registry and every stored declaration are
/// read under the exclusive registry lock, the effective folders of every
/// declaration are compared per registered volume against each other and
/// against Barge's own `scratch/` and `caches/` folders, and only then is the
/// document published atomically. Two owners of one folder is not a state Barge
/// writes.
pub fn declare(state: &StatePaths, caller: &AreaDeclaration, now_ms: u64) -> Result<AreaDocument> {
    caller.validate()?;
    // Refuse an invalid registry before any effect, including creating the
    // state layout. It is then re-read under the lock, so the collision check
    // sees the registry the write is authorized by.
    registry::read(state)?;
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let lock = locks.lock_registry()?;
    let registry = registry::read(state)?;
    let stored = read_declarations(state)?;
    ensure_no_collisions(registry.volumes(), &stored.documents, caller)?;
    let generation = stored
        .find(&caller.namespace, &caller.area)
        .map_or(1, |previous| previous.generation.saturating_add(1));
    let document = caller.clone().into_document(now_ms, generation)?;
    document.validate()?;
    let path = declaration_path(state, &document.namespace, &document.area)?;
    let namespace_dir = path.parent().ok_or_else(|| {
        Error::new(
            ErrorKind::Usage,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    ensure_private_directory(namespace_dir)?;
    let mut bytes = serde_json::to_vec_pretty(&document).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the declaration: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > AREA_DECLARATION_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the declaration would be {} bytes, over the {AREA_DECLARATION_CAP_BYTES}-byte cap",
                bytes.len()
            ),
        )
        .with_detail("bytes", bytes.len().into())
        .with_detail("cap", AREA_DECLARATION_CAP_BYTES.into()));
    }
    state::publish_atomic(&path, &bytes)?;
    drop(lock);
    Ok(document)
}

/// The declaration for one area, if it is declared.
pub fn show(state: &StatePaths, namespace: &str, area: &str) -> Result<Option<AreaDocument>> {
    declaration_path(state, namespace, area)?;
    Ok(read_declarations(state)?.find(namespace, area).cloned())
}

/// Refuse a declaration that overlaps another declaration or one of Barge's own
/// folders on any registered volume.
fn ensure_no_collisions(
    volumes: &[VolumeRecord],
    stored: &[AreaDocument],
    candidate: &AreaDeclaration,
) -> Result<()> {
    if volumes.is_empty() {
        // No volume is registered, so the folder strings themselves are the only
        // thing two declarations could share.
        let candidate_components = folder_components("folder", &candidate.folder)?;
        let candidate_label = format!("{}/{}", candidate.namespace, candidate.area);
        for document in stored {
            if document.namespace == candidate.namespace && document.area == candidate.area {
                continue;
            }
            let existing = folder_components("folder", &document.folder)?;
            refuse_overlap(
                &format!("{}/{}", document.namespace, document.area),
                &existing,
                &candidate_label,
                &candidate_components,
            )?;
        }
        return Ok(());
    }
    for record in volumes {
        // A declaration over an imported record with undeclared durability is
        // an area use, and every area use refuses until a human declares it.
        record.declared_durability()?;
        let candidate_folder = effective_folder(record, candidate)?;
        let candidate_components = volume_components(record, &candidate_folder)?;
        for document in stored {
            if document.namespace == candidate.namespace && document.area == candidate.area {
                continue;
            }
            let folder = effective_folder(record, &document.declaration())?;
            let existing = volume_components(record, &folder)?;
            refuse_overlap(
                &format!("{}/{}", document.namespace, document.area),
                &existing,
                &format!("{}/{}", candidate.namespace, candidate.area),
                &candidate_components,
            )?;
        }
        for (name, folder) in [
            (
                "scratch",
                record
                    .folders
                    .scratch
                    .clone()
                    .unwrap_or_else(|| "scratch".to_owned()),
            ),
            (
                "caches",
                record
                    .folders
                    .caches
                    .clone()
                    .unwrap_or_else(|| "caches".to_owned()),
            ),
        ] {
            let managed = volume_components(record, &folder)?;
            refuse_overlap(
                name,
                &managed,
                &format!("{}/{}", candidate.namespace, candidate.area),
                &candidate_components,
            )?;
        }
    }
    Ok(())
}

fn volume_components(record: &VolumeRecord, folder: &str) -> Result<Vec<String>> {
    let mut components = if record.root_dir.is_empty() {
        Vec::new()
    } else {
        folder_components("root_dir", &record.root_dir)?
    };
    components.extend(folder_components("folder", folder)?);
    Ok(components)
}

fn refuse_overlap(first: &str, left: &[String], second: &str, right: &[String]) -> Result<()> {
    let first_inside = left.starts_with(right);
    let second_inside = right.starts_with(left);
    if !first_inside && !second_inside {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::RefusedContainment,
        if left == right {
            format!("`{second}` would own the same folder as `{first}`")
        } else {
            format!("`{second}` would nest with `{first}`; two owners of one folder is refused")
        },
    )
    .with_detail("area", second.into())
    .with_detail("conflicts_with", first.into())
    .with_detail("folder", left.join("/").into()))
}

/// Create one private directory (mode `0700`), or accept an existing real one.
fn ensure_private_directory(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => set_mode(path, 0o700),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path).map_err(|error| {
                Error::new(
                    ErrorKind::Io,
                    format!("cannot read {}: {error}", path.display()),
                )
                .with_detail("path", path.display().to_string().into())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!("{} is not a real directory", path.display()),
                )
                .with_detail("path", path.display().to_string().into()));
            }
            Ok(())
        }
        Err(error) => Err(Error::new(
            ErrorKind::Io,
            format!("cannot create {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot set the mode of {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })
}

/// A scope a hold declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldScope {
    /// The holder reads the root; another reader may join it.
    Read,
    /// The holder writes the root; a later slice's custody evidence records it.
    Write,
}

impl HoldScope {
    /// The name a document reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// One `barge.hold/1` record: a cooperative holder's claim on one root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldRecord {
    /// Always `barge.hold/1`.
    pub schema: String,
    /// The root the hold covers.
    pub root_id: String,
    /// The holder's own id, which names the file.
    pub holder: String,
    /// The holder process's pid.
    pub pid: u32,
    /// The holder process's start identity, so a reused pid is not the holder.
    pub process_start: u64,
    /// The boot identity the start identity belongs to.
    pub boot_identity: String,
    /// What the holder does with the root.
    pub scope: HoldScope,
    /// Why the holder holds it.
    pub purpose: String,
    /// When the hold was written.
    pub held_ms: u64,
}

/// How one recorded hold is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldState {
    /// The recorded process is positively the holder.
    Live,
    /// The recorded process is positively gone, or another boot wrote the hold,
    /// so the hold is stale.
    Stale,
    /// The hold cannot be decided: unreadable, foreign-owned, unknown version, a
    /// pid whose start identity is unreadable, or a live pid whose start identity
    /// disagrees with the record. Unknown is never stale.
    Unknown,
}

/// The hold states a document reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldStateName {
    /// Live.
    Live,
    /// Stale.
    Stale,
    /// Unknown.
    Unknown,
}

/// One hold as a document reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldRow {
    /// The holder's id.
    pub holder: String,
    /// How the hold was decided.
    pub state: HoldStateName,
    /// What the holder does.
    pub scope: HoldScope,
    /// Why the holder holds it.
    pub purpose: String,
    /// The recorded pid.
    pub pid: u32,
    /// When the hold was written.
    pub held_ms: u64,
    /// The recorded process start identity.
    pub process_start: u64,
    /// The recorded boot identity.
    pub boot_identity: String,
    /// Why an unknown hold is unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What reading one root's holds found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldReading {
    /// The holds, sorted by holder.
    pub holds: Vec<(HoldRecord, HoldState, Option<String>)>,
    /// How many files the directory held, at least `holds.len()`.
    pub total: usize,
    /// Whether more holds exist than the per-root cap.
    pub truncated: bool,
}

impl HoldReading {
    /// No holds.
    pub fn empty() -> Self {
        Self {
            holds: Vec::new(),
            total: 0,
            truncated: false,
        }
    }

    /// Whether no hold blocks idleness: a live hold means a holder is using the
    /// root, and an unknown hold cannot be decided, so neither is idle. Idleness
    /// comes only from Barge's own holds.
    pub fn is_idle(&self) -> bool {
        !self
            .holds
            .iter()
            .any(|(_, state, _)| matches!(state, HoldState::Live | HoldState::Unknown))
    }

    /// The rows a document reports.
    pub fn rows(&self) -> Vec<HoldRow> {
        self.holds
            .iter()
            .map(|(hold, state, reason)| HoldRow {
                holder: hold.holder.clone(),
                state: match state {
                    HoldState::Live => HoldStateName::Live,
                    HoldState::Stale => HoldStateName::Stale,
                    HoldState::Unknown => HoldStateName::Unknown,
                },
                scope: hold.scope,
                purpose: hold.purpose.clone(),
                pid: hold.pid,
                held_ms: hold.held_ms,
                process_start: hold.process_start,
                boot_identity: hold.boot_identity.clone(),
                reason: reason.clone(),
            })
            .collect()
    }
}

/// What a caller asks [`hold`] to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldRequest {
    /// The root to hold.
    pub root_id: String,
    /// The holder's own id.
    pub holder: String,
    /// The holder process's pid.
    pub pid: u32,
    /// What the holder does with the root.
    pub scope: HoldScope,
    /// Why the holder holds it.
    pub purpose: String,
}

/// One recorded hold and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    /// The record as written.
    pub record: HoldRecord,
    /// The file it was published at.
    pub path: PathBuf,
}

/// Record a hold before a consumer touches a root.
///
/// The record names the holder's pid, process start identity, boot identity,
/// scope, purpose and time, so a later reader can decide whether the holder is
/// still there. The root lock is held while the file is published, which is the
/// lock order the design fixes, and the file is published atomically and
/// privately.
pub fn hold(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    request: &HoldRequest,
) -> Result<Hold> {
    state::validate_identifier("root_id", &request.root_id)?;
    validate_name("holder", &request.holder)?;
    if request.purpose.trim().is_empty() {
        return Err(invalid("a hold states its purpose", "purpose", ""));
    }
    if request.purpose.len() > MAX_PURPOSE_BYTES {
        return Err(invalid(
            "a hold's purpose is bounded",
            "purpose",
            &request.purpose.len().to_string(),
        ));
    }
    // Every mutating path starts at the registry: an invalid registry refuses
    // before any effect, and it is never rebuilt from markers.
    registry::read(state)?;
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let _lock = locks.lock_root(&request.root_id)?;
    let reading = read_holds(state, observer, &request.root_id)?;
    if reading.total >= HOLD_ROWS {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "root {} already carries {HOLD_ROWS} holds, the per-root cap",
                request.root_id
            ),
        )
        .with_detail("root_id", request.root_id.clone().into())
        .with_detail("cap", HOLD_ROWS.into()));
    }
    let process_start = observer.process_start(request.pid)?.ok_or_else(|| {
        Error::new(
            ErrorKind::CustodyNotProven,
            format!(
                "pid {} is positively gone, so it cannot record a hold",
                request.pid
            ),
        )
        .with_detail("pid", u64::from(request.pid).into())
    })?;
    let boot_identity = observer.boot_identity()?;
    let record = HoldRecord {
        schema: crate::contract::HOLD_SCHEMA.to_owned(),
        root_id: request.root_id.clone(),
        holder: request.holder.clone(),
        pid: request.pid,
        process_start,
        boot_identity,
        scope: request.scope,
        purpose: request.purpose.clone(),
        held_ms: state::now_ms(),
    };
    let directory = state.holds_dir().join(&request.root_id);
    ensure_private_directory(&directory)?;
    let path = directory.join(format!("{}.json", request.holder));
    let mut bytes = serde_json::to_vec_pretty(&record).map_err(|error| {
        Error::new(ErrorKind::Io, format!("could not encode the hold: {error}"))
    })?;
    bytes.push(b'\n');
    if bytes.len() > HOLD_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!("the hold record is over the {HOLD_CAP_BYTES}-byte cap"),
        )
        .with_detail("cap", HOLD_CAP_BYTES.into()));
    }
    state::publish_atomic(&path, &bytes)?;
    Ok(Hold { record, path })
}

/// Release one holder's hold. A hold that is already absent is already released.
pub fn release(state: &StatePaths, root_id: &str, holder: &str) -> Result<PathBuf> {
    state::validate_identifier("root_id", root_id)?;
    validate_name("holder", holder)?;
    registry::read(state)?;
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let _lock = locks.lock_root(root_id)?;
    let path = state
        .holds_dir()
        .join(root_id)
        .join(format!("{holder}.json"));
    let bytes = match state::read_bounded(
        &path,
        &Bounds::new(HOLD_CAP_BYTES, HOLD_ROWS, Deadline::after(READ_DEADLINE)),
    ) {
        ReadOutcome::Absent => return Ok(path),
        ReadOutcome::Present(bytes) => bytes,
        ReadOutcome::Refused(error) => {
            return Err(error.with_detail("path", path.display().to_string().into()))
        }
    };
    let record: HoldRecord = serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "`{}` refused the hold: {error}; it was not removed",
                crate::contract::HOLD_SCHEMA
            ),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    if record.schema != crate::contract::HOLD_SCHEMA
        || record.root_id != root_id
        || record.holder != holder
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!(
                "{} is not the hold `{holder}` on root `{root_id}`; it was not removed",
                path.display()
            ),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    std::fs::remove_file(&path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot remove {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    if let Some(directory) = path.parent() {
        // Best effort: the removal is what matters, and a directory sync the
        // platform refuses must not turn a completed release into a failure.
        let _ = std::fs::File::open(directory).and_then(|handle| handle.sync_all());
    }
    Ok(path)
}

/// Read and decide one root's holds, bounded by the per-root cap.
///
/// A hold file that is absent is nothing; unreadable, oversized, symlinked,
/// foreign-owned, unknown-version or schema-divergent content is `Unknown`; a
/// record whose boot changed or whose process is positively gone is `Stale`; a
/// record whose process start identity matches is `Live`.
pub fn read_holds(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    root_id: &str,
) -> Result<HoldReading> {
    state::validate_identifier("root_id", root_id)?;
    let directory = state.holds_dir().join(root_id);
    let Some(names) = read_directory(&directory)? else {
        return Ok(HoldReading::empty());
    };
    let effective_uid = observer.effective_uid()?;
    let mut holds = Vec::new();
    let total = names.len();
    for name in names.into_iter().take(HOLD_ROWS) {
        let Some(holder) = name.strip_suffix(".json") else {
            continue;
        };
        if validate_name("holder", holder).is_err() {
            holds.push((
                HoldRecord {
                    schema: crate::contract::HOLD_SCHEMA.to_owned(),
                    root_id: root_id.to_owned(),
                    holder: holder.to_owned(),
                    pid: 0,
                    process_start: 0,
                    boot_identity: String::new(),
                    scope: HoldScope::Read,
                    purpose: String::new(),
                    held_ms: 0,
                },
                HoldState::Unknown,
                Some(format!("`{holder}` is not a hold file name Barge writes")),
            ));
            continue;
        }
        let path = directory.join(&name);
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            holds.push(unknown_hold(
                holder,
                root_id,
                format!("{} is not a regular file", path.display()),
            ));
            continue;
        }
        if metadata.uid() != effective_uid {
            holds.push(unknown_hold(
                holder,
                root_id,
                format!(
                    "{} is owned by uid {}, not this account's {effective_uid}",
                    path.display(),
                    metadata.uid()
                ),
            ));
            continue;
        }
        if metadata.mode() & 0o077 != 0 {
            holds.push(unknown_hold(
                holder,
                root_id,
                format!(
                    "{} has mode {:o}, which is not private",
                    path.display(),
                    metadata.mode() & 0o7777
                ),
            ));
            continue;
        }
        let bytes = match state::read_bounded(
            &path,
            &Bounds::new(HOLD_CAP_BYTES, HOLD_ROWS, Deadline::after(READ_DEADLINE)),
        ) {
            ReadOutcome::Present(bytes) => bytes,
            ReadOutcome::Absent => continue,
            ReadOutcome::Refused(error) => {
                holds.push(unknown_hold(holder, root_id, error.to_string()));
                continue;
            }
        };
        let record: HoldRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => {
                holds.push(unknown_hold(
                    holder,
                    root_id,
                    format!("the hold record is unreadable: {error}"),
                ));
                continue;
            }
        };
        if record.schema != crate::contract::HOLD_SCHEMA
            || record.root_id != root_id
            || record.holder != holder
        {
            holds.push(unknown_hold(
                holder,
                root_id,
                "the hold record does not name this root and holder".to_owned(),
            ));
            continue;
        }
        let (state_name, reason) = decide_hold(observer, &record, holder);
        holds.push((record, state_name, reason));
    }
    holds.sort_by(|left, right| left.0.holder.cmp(&right.0.holder));
    Ok(HoldReading {
        holds,
        total,
        truncated: total > HOLD_ROWS,
    })
}

fn unknown_hold(
    holder: &str,
    root_id: &str,
    reason: String,
) -> (HoldRecord, HoldState, Option<String>) {
    (
        HoldRecord {
            schema: crate::contract::HOLD_SCHEMA.to_owned(),
            root_id: root_id.to_owned(),
            holder: holder.to_owned(),
            pid: 0,
            process_start: 0,
            boot_identity: String::new(),
            scope: HoldScope::Read,
            purpose: String::new(),
            held_ms: 0,
        },
        HoldState::Unknown,
        Some(reason),
    )
}

/// Decide one hold.
///
/// It is stale only when the recorded process is positively gone or another boot
/// wrote it. A live pid whose start identity differs is *not* proved stale: the
/// design treats the reused-pid case as unknown unless every recorded identity
/// positively disagrees, and an unknown hold blocks deletion (stage 1 design,
/// failure matrix). A fact that cannot be read is unknown, never stale.
fn decide_hold(
    observer: &dyn ProcessObserver,
    record: &HoldRecord,
    holder: &str,
) -> (HoldState, Option<String>) {
    let boot = match observer.boot_identity() {
        Ok(boot) => boot,
        Err(error) => {
            return (
                HoldState::Unknown,
                Some(format!("the boot identity is unreadable: {error}")),
            )
        }
    };
    if boot != record.boot_identity {
        return (
            HoldState::Stale,
            Some(format!(
                "the hold was written under boot {}, and this is boot {boot}",
                record.boot_identity
            )),
        );
    }
    match observer.process_start(record.pid) {
        Ok(None) => (
            HoldState::Stale,
            Some(format!("pid {} is positively gone", record.pid)),
        ),
        Ok(Some(start)) if start == record.process_start => (HoldState::Live, None),
        Ok(Some(start)) => (
            HoldState::Unknown,
            Some(format!(
                "pid {} is now start {start}, not the recorded {}; a live pid whose start identity disagrees is not proved stale",
                record.pid, record.process_start
            )),
        ),
        Err(error) => (
            HoldState::Unknown,
            Some(format!(
                "the start identity of pid {} for `{holder}` is unreadable: {error}",
                record.pid
            )),
        ),
    }
}

/// One root as a listing observes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootObservation {
    /// The directory's name inside the area.
    pub name: String,
    /// Whether the root is owned, unmarked or unknown.
    pub state: MarkerStateName,
    /// Why an unknown marker is unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The marker's root id, when the marker was owned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// What the root is for, when the marker was owned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RootKind>,
    /// The area namespace, when the marker was owned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The area name, when the marker was owned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// The path the marker recorded at creation, when it was owned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_path: Option<PathBuf>,
    /// The path the current configuration resolves. Recorded beats configured,
    /// so the two can differ without anything having moved.
    pub configured_path: PathBuf,
    /// The root directory's device, when it was opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<u64>,
    /// The root directory's inode, when it was opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode: Option<u64>,
    /// The SHA-256 of the marker's exact bytes, when the marker is owned. A
    /// caller that acts on the root compares this with what it re-reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_digest: Option<String>,
    /// When the marker says the root was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<u64>,
    /// The newest activity Barge observed: the root directory's own
    /// modification time or the marker's creation time, whichever is newer. No
    /// consumer content is interpreted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    /// Whether no live hold covers the root. Idleness comes only from Barge's
    /// own holds.
    pub idle: bool,
    /// The root's holds, bounded to the per-root cap.
    pub holds: Vec<HoldRow>,
    /// Whether more holds exist than the per-root cap.
    pub holds_truncated: bool,
}

/// One bounded root listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootListing {
    /// The roots kept, in the platform's own listing order.
    pub roots: Vec<RootObservation>,
    /// How many entries the area folder held.
    pub examined: usize,
    /// Whether the listing was cut by the row or name bound.
    pub truncated: bool,
    /// Entries that are neither roots nor symlinks: files and other entries
    /// inside an area are reported by count and never interpreted.
    pub skipped_non_directories: usize,
}

impl RootListing {
    /// No roots.
    pub fn empty() -> Self {
        Self {
            roots: Vec::new(),
            examined: 0,
            truncated: false,
            skipped_non_directories: 0,
        }
    }

    /// The rows a document reports.
    pub fn rows(&self) -> RowSet<RootObservation> {
        RowSet::bounded(self.roots.clone(), self.roots.len(), ROOT_ROWS)
    }
}

/// List the roots of one resolved area, bounded.
///
/// Only directories and symlinks are observed; every directory is a row whose
/// marker state says whether it is owned, unmarked or unknown. A marker is never
/// adopted, a symlink is never followed and no consumer content is read. Holds
/// come from the state directory and are the only basis for idleness.
pub fn list_roots(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    resolved: &ResolvedArea,
    max_roots: usize,
) -> Result<RootListing> {
    let handle = match &resolved.dirs {
        Direction::Existing { handle } => dirs.duplicate_dir(handle)?,
        Direction::Missing { .. } => return Ok(RootListing::empty()),
    };
    let effective_uid = observer.effective_uid()?;
    let now_ms = state::now_ms();
    let name_cap = max_roots.clamp(1, LISTING_NAME_ROWS);
    let listing = dirs.list_children(&handle, name_cap)?;
    let mut roots = Vec::new();
    let mut skipped_non_directories = 0usize;
    for entry in &listing.entries {
        if roots.len() >= max_roots {
            break;
        }
        if entry.facts.kind == EntryKind::Symlink {
            roots.push(unreadable_root(
                &entry.name,
                &resolved.path.join(&entry.name),
                "the entry is a symbolic link, which is never followed or owned".to_owned(),
            ));
            continue;
        }
        if entry.facts.kind != EntryKind::Directory {
            skipped_non_directories = skipped_non_directories.saturating_add(1);
            continue;
        }
        let configured_path = resolved.path.join(&entry.name);
        let root = match dirs.open_child_dir(&handle, &entry.name) {
            Ok(root) => root,
            Err(error) => {
                roots.push(unreadable_root(
                    &entry.name,
                    &configured_path,
                    error.to_string(),
                ));
                continue;
            }
        };
        let expectation = MarkerExpectation {
            volume_uuid: &resolved.volume_uuid,
            resolved_path: &configured_path,
            effective_uid,
            now_ms,
        };
        let (marker, marker_digest) = read_marker_with_digest(dirs, &root, &expectation);
        let holds = if state::validate_identifier("root_id", &entry.name).is_err() {
            HoldReading::empty()
        } else {
            match read_holds(state, observer, &entry.name) {
                Ok(holds) => holds,
                Err(error) => {
                    roots.push(unreadable_root(
                        &entry.name,
                        &configured_path,
                        format!("the root's holds are unreadable: {error}"),
                    ));
                    continue;
                }
            }
        };
        roots.push(observe_root(
            &entry.name,
            &root,
            &configured_path,
            entry.facts.modified_ms,
            marker,
            marker_digest,
            &holds,
        ));
    }
    Ok(RootListing {
        roots,
        examined: listing.total,
        truncated: listing.truncated,
        skipped_non_directories,
    })
}

fn observe_root(
    name: &str,
    root: &DirHandle,
    configured_path: &Path,
    directory_modified_ms: Option<u64>,
    marker: MarkerState,
    marker_digest: Option<String>,
    holds: &HoldReading,
) -> RootObservation {
    let mut row = RootObservation {
        name: name.to_owned(),
        state: marker.name(),
        reason: None,
        root_id: None,
        kind: None,
        namespace: None,
        area: None,
        recorded_path: None,
        configured_path: configured_path.to_path_buf(),
        device: Some(root.identity().device.get()),
        inode: Some(root.identity().inode),
        marker_digest,
        created_ms: None,
        last_activity_ms: None,
        idle: holds.is_idle(),
        holds: holds.rows(),
        holds_truncated: holds.truncated,
    };
    match marker {
        MarkerState::Owned(marker) => {
            row.root_id = Some(marker.root_id.clone());
            row.kind = Some(marker.kind);
            row.namespace = Some(marker.area.namespace.clone());
            row.area = Some(marker.area.area.clone());
            row.recorded_path = Some(marker.resolved_path.clone());
            row.created_ms = Some(marker.created_ms);
            row.last_activity_ms = Some(marker.created_ms.max(directory_modified_ms.unwrap_or(0)));
        }
        MarkerState::Absent => {
            row.reason = Some("the directory carries no marker".to_owned());
            row.last_activity_ms = directory_modified_ms;
        }
        MarkerState::Unknown { reason } => {
            row.reason = Some(reason);
            row.last_activity_ms = directory_modified_ms;
        }
    }
    row
}

fn unreadable_root(name: &str, configured_path: &Path, reason: String) -> RootObservation {
    RootObservation {
        name: name.to_owned(),
        state: MarkerStateName::Unknown,
        reason: Some(reason),
        root_id: None,
        kind: None,
        namespace: None,
        area: None,
        recorded_path: None,
        configured_path: configured_path.to_path_buf(),
        device: None,
        inode: None,
        marker_digest: None,
        created_ms: None,
        last_activity_ms: None,
        // An unreadable root's holds are unknown, so it is never reported idle.
        idle: false,
        holds: Vec::new(),
        holds_truncated: false,
    }
}

/// The `barge.area.declare/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclareReport {
    /// Where the declaration was published.
    pub path: PathBuf,
    /// The declaration as written, with its generation.
    pub declaration: AreaDocument,
}

/// The `barge.area.show/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowReport {
    /// Where the declaration lives.
    pub path: PathBuf,
    /// The declaration as stored.
    pub declaration: AreaDocument,
}

/// The `barge.area.list/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListReport {
    /// The declarations, bounded to the area row cap.
    pub areas: RowSet<AreaDocument>,
}

/// The `barge.area.root.create/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootCreateReport {
    /// The registered volume the root was created on.
    pub volume_uuid: String,
    /// The effective folder for that volume.
    pub folder: String,
    /// The absolute area path the configuration resolved.
    pub area_path: PathBuf,
    /// The effective declaration recorded in the root's marker.
    pub effective: EffectiveDeclaration,
    /// The root that was created.
    pub root: OwnedRoot,
}

/// The `barge.area.root.list/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootListReport {
    /// The registered volume that was listed.
    pub volume_uuid: String,
    /// The effective folder for that volume.
    pub folder: String,
    /// The absolute area path the configuration resolved.
    pub area_path: PathBuf,
    /// The effective declaration the listing used.
    pub effective: EffectiveDeclaration,
    /// The roots, bounded to the root row cap.
    pub roots: RowSet<RootObservation>,
    /// How many entries the area folder held.
    pub examined: usize,
    /// Whether the listing was cut by a bound.
    pub truncated: bool,
    /// Entries that are neither roots nor symlinks.
    pub skipped_non_directories: usize,
}

/// The `barge.area.hold/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldReport {
    /// Where the hold record was published.
    pub path: PathBuf,
    /// The hold as written and decided.
    pub hold: HoldRow,
}

/// The `barge.area.release/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReport {
    /// The hold file that was removed, or would have been.
    pub path: PathBuf,
    /// Whether a hold was actually removed.
    pub released: bool,
    /// The root the hold covered.
    pub root_id: String,
    /// The holder that released.
    pub holder: String,
}

/// Bytes as lowercase hexadecimal.
fn hex(length: usize, bytes: Vec<u8>) -> String {
    let mut text = String::with_capacity(length * 2);
    for byte in bytes.iter().take(length) {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        declare, folder_components, hold, list_roots, read_declarations, read_holds, read_marker,
        release, resolve, show, validate_name, AreaDeclaration, HoldRequest, HoldScope, HoldState,
        MarkerExpectation, MarkerState, MarkerStateName, RootKind, RootSpec, AREA_DIRECTORY_MODE,
        ROOT_DIRECTORY_MODE, ROOT_MARKER_FILE,
    };
    use crate::digest::sha256_hex;
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::{
        AccountFacts, ChildListing, DescriptorScan, DescriptorScanRequest, DeviceId, DirHandle,
        DirId, DirOps, EntryFacts, FileSystem, MacOs, MountPoint, MountRoot, ProcessObserver,
        VolumeFacts, VolumeIdentity, VolumeUuid,
    };
    use crate::registry::{
        Availability, Durability, FolderOverrides, RecordOrigin, RegistryDocument, RegistryWriter,
        ScratchGrants, VolumeRecord,
    };
    use crate::state::{Deadline, LockSet, StatePaths};
    use crate::testkit::TempDir;

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";
    const OTHER_UUID: &str = "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D";
    const BOOT: &str = "1700000000.0";

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

    /// Injected volume facts: a fixture directory stands in for a mounted
    /// volume, and a test decides whether it is present and which UUID it has.
    struct FakeVolume {
        root: PathBuf,
        uuid: VolumeUuid,
        device: DeviceId,
        present: bool,
    }

    impl VolumeFacts for FakeVolume {
        fn identify(&self, _path: &Path) -> Result<VolumeIdentity> {
            if !self.present {
                return Err(Error::new(
                    ErrorKind::VolumeAbsent,
                    "the fixture volume is not attached",
                ));
            }
            Ok(VolumeIdentity {
                mount_point: self.root.clone(),
                filesystem: FileSystem::Apfs,
                device: self.device,
                volume_uuid: Some(self.uuid.clone()),
                boot_volume: false,
            })
        }
    }

    struct FakeMounts {
        root: PathBuf,
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

    #[derive(Default)]
    struct FakeObserver {
        uid: u32,
        boot: String,
        processes: BTreeMap<u32, Option<u64>>,
        unreadable: BTreeSet<u32>,
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

        fn verify_platform_binary(
            &self,
            _classified: crate::os::ProcessIdentity,
        ) -> Result<crate::os::PlatformStatus> {
            Ok(crate::os::PlatformStatus::NotVerified {
                reason: "this observer reports no platform binary".to_owned(),
            })
        }

        fn scan_open_descriptors(
            &self,
            _request: &DescriptorScanRequest<'_>,
        ) -> Result<DescriptorScan> {
            // S3's tests never ask for custody; S5's own tests inject the scan.
            Ok(DescriptorScan::Complete {
                processes: 0,
                descriptors: 0,
                foreign: 0,
                retry: crate::os::RetryEvidence::default(),
                os_blind_spot: crate::os::BlindSpotEvidence::default(),
                open: Vec::new(),
            })
        }
    }

    /// The platform's directory operations with the injected facts a platform
    /// cannot be asked to produce: a foreign owner, a component on another
    /// device, and a component swapped between the check and the open.
    #[derive(Default)]
    struct Adjust {
        inner: MacOs,
        foreign_uid_name: Option<String>,
        shifted_device_name: Option<String>,
        shifted_device: Option<DeviceId>,
        swap_name: Option<String>,
        swap_target: Option<PathBuf>,
        swap_moved: Option<PathBuf>,
        swapped: RefCell<bool>,
        competing_create: Option<String>,
    }

    impl Adjust {
        fn new() -> Self {
            Self {
                inner: crate::os::platform(),
                ..Self::default()
            }
        }

        fn swapping(name: &str, target: &Path, moved: &Path) -> Self {
            Self {
                inner: crate::os::platform(),
                swap_name: Some(name.to_owned()),
                swap_target: Some(target.to_path_buf()),
                swap_moved: Some(moved.to_path_buf()),
                ..Self::default()
            }
        }

        fn with_foreign_owner(mut self, name: &str) -> Self {
            self.foreign_uid_name = Some(name.to_owned());
            self
        }

        fn with_device_shift(mut self, name: &str, device: DeviceId) -> Self {
            self.shifted_device_name = Some(name.to_owned());
            self.shifted_device = Some(device);
            self
        }

        /// A competing creator wins the `mkdir` race for `name` and the path
        /// fails with the platform's own error, exactly as a concurrent
        /// creator's `EEXIST` looks to the loser.
        fn with_competing_create(mut self, name: &str) -> Self {
            self.competing_create = Some(name.to_owned());
            self
        }

        fn maybe_swap(&self, name: &str) {
            if self.swap_name.as_deref() != Some(name) || *self.swapped.borrow() {
                return;
            }
            let (Some(target), Some(moved)) = (self.swap_target.as_ref(), self.swap_moved.as_ref())
            else {
                return;
            };
            if std::fs::rename(target, moved).is_ok() && std::fs::create_dir(target).is_ok() {
                *self.swapped.borrow_mut() = true;
            }
        }
    }

    impl DirOps for Adjust {
        fn open_path(&self, path: &Path) -> Result<DirHandle> {
            self.inner.open_path(path)
        }

        fn path_facts(&self, path: &Path) -> Result<Option<EntryFacts>> {
            self.inner.path_facts(path)
        }

        fn open_child_dir(&self, parent: &DirHandle, name: &str) -> Result<DirHandle> {
            self.maybe_swap(name);
            let opened = self.inner.open_child_dir(parent, name)?;
            if self.shifted_device_name.as_deref() == Some(name) {
                let device = self
                    .shifted_device
                    .ok_or_else(|| Error::new(ErrorKind::Usage, "no injected device"))?;
                return Ok(DirHandle::injected(DirId {
                    device,
                    inode: opened.identity().inode,
                }));
            }
            Ok(opened)
        }

        fn duplicate_dir(&self, handle: &DirHandle) -> Result<DirHandle> {
            self.inner.duplicate_dir(handle)
        }

        fn child_facts(&self, parent: &DirHandle, name: &str) -> Result<Option<EntryFacts>> {
            let facts = self.inner.child_facts(parent, name)?;
            let Some(mut facts) = facts else {
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
            if self.competing_create.as_deref() == Some(name) {
                // The competing creator's directory exists; this caller lost the
                // race and sees the platform's refusal.
                self.inner.create_child_dir(parent, name, mode)?;
                return Err(Error::new(
                    ErrorKind::Io,
                    "a competing creator already made this directory",
                ));
            }
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
            kind: crate::os::RemoveKind,
        ) -> Result<crate::os::ChildRemoval> {
            self.inner.remove_child(parent, name, kind)
        }

        fn rename_child(
            &self,
            parent: &DirHandle,
            from: &str,
            to: &str,
        ) -> Result<crate::os::RenameOutcome> {
            self.inner.rename_child(parent, from, to)
        }

        fn dir_facts(&self, handle: &DirHandle) -> Result<EntryFacts> {
            self.inner.dir_facts(handle)
        }

        fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> Result<()> {
            self.inner.set_dir_mode(handle, mode)
        }

        fn recheck_dir(&self, handle: &DirHandle) -> Result<DirId> {
            self.inner.recheck_dir(handle)
        }

        fn random_bytes(&self, length: usize) -> Result<Vec<u8>> {
            self.inner.random_bytes(length)
        }
    }

    struct Fixture {
        dir: TempDir,
        root: PathBuf,
        uuid: VolumeUuid,
        device: DeviceId,
        record: VolumeRecord,
        state: StatePaths,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = TempDir::new(label);
            let root = dir.join("volume");
            std::fs::create_dir(&root).expect("the fixture volume root is created");
            let platform = crate::os::platform();
            let device = platform
                .open_path(&root)
                .expect("the fixture root opens")
                .identity()
                .device;
            let uuid = VolumeUuid::parse(UUID).expect("a test UUID");
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

        fn platform(&self) -> MacOs {
            crate::os::platform()
        }

        fn facts(&self) -> FakeVolume {
            FakeVolume {
                root: self.root.clone(),
                uuid: self.uuid.clone(),
                device: self.device,
                present: true,
            }
        }

        fn absent_facts(&self) -> FakeVolume {
            FakeVolume {
                root: self.root.clone(),
                uuid: self.uuid.clone(),
                device: self.device,
                present: false,
            }
        }

        fn mismatched_facts(&self) -> FakeVolume {
            FakeVolume {
                root: self.root.clone(),
                uuid: VolumeUuid::parse(OTHER_UUID).expect("a test UUID"),
                device: self.device,
                present: true,
            }
        }

        fn mounts(&self) -> FakeMounts {
            FakeMounts {
                root: self.root.clone(),
            }
        }

        fn observer(&self) -> FakeObserver {
            FakeObserver {
                uid: self.platform().effective_uid().expect("the effective uid"),
                boot: BOOT.to_owned(),
                processes: BTreeMap::from([(std::process::id(), Some(7_000_000))]),
                unreadable: BTreeSet::new(),
            }
        }

        fn register(&self, record: &VolumeRecord) {
            self.state.initialize().expect("the layout exists");
            let mut locks =
                LockSet::with_deadline(self.state.clone(), Deadline::after(Duration::from_secs(5)));
            let lock = locks.lock_registry().expect("the registry lock");
            RegistryWriter::new(&self.state)
                .publish(&lock, &RegistryDocument::new(vec![record.clone()]))
                .expect("the registry is written");
        }

        fn record_with_root_dir(&self, root_dir: &str) -> VolumeRecord {
            let mut record = self.record.clone();
            record.root_dir = root_dir.to_owned();
            record
        }

        fn declaration(&self, folder: &str) -> AreaDeclaration {
            AreaDeclaration {
                namespace: "example".to_owned(),
                area: "pool".to_owned(),
                folder: folder.to_owned(),
                overrides: BTreeMap::new(),
                absent: super::AbsentPolicy::Fail {
                    attach: "the build volume".to_owned(),
                },
                prune: super::PrunePolicy::OldestIdleAfter {
                    age_seconds: 86_400,
                },
                reserve: super::ReserveParticipation::Counts,
            }
        }

        fn resolved(
            &self,
            record: &VolumeRecord,
            declaration: &AreaDeclaration,
        ) -> super::ResolvedArea {
            let platform = self.platform();
            let facts = self.facts();
            let mounts = self.mounts();
            let observer = self.observer();
            resolve(
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

        fn resolve_error(
            &self,
            record: &VolumeRecord,
            declaration: &AreaDeclaration,
            dirs: &dyn DirOps,
        ) -> Error {
            let facts = self.facts();
            let mounts = self.mounts();
            let observer = self.observer();
            resolve(dirs, &facts, &mounts, &observer, record, declaration, None)
                .expect_err("the resolution refuses")
        }

        fn create_root(
            &self,
            record: &VolumeRecord,
            declaration: &AreaDeclaration,
        ) -> (super::OwnedRoot, super::ResolvedArea) {
            let platform = self.platform();
            let observer = self.observer();
            let resolved = self.resolved(record, declaration);
            let root = super::create_root(
                &platform,
                &observer,
                &resolved,
                RootSpec {
                    kind: RootKind::AreaRoot,
                },
            )
            .expect("the root is created");
            let resolved = self.resolved(record, declaration);
            (root, resolved)
        }

        fn list(&self, resolved: &super::ResolvedArea) -> super::RootListing {
            let platform = self.platform();
            let observer = self.observer();
            list_roots(&platform, &observer, &self.state, resolved, 4096).expect("the area lists")
        }
    }

    fn marker_path(root: &super::OwnedRoot) -> PathBuf {
        root.path.join(ROOT_MARKER_FILE)
    }

    fn read_owned(fixture: &Fixture, resolved: &super::ResolvedArea, name: &str) -> MarkerState {
        let platform = fixture.platform();
        let observer = fixture.observer();
        let root = platform
            .open_path(&resolved.path.join(name))
            .expect("the root opens");
        let path = resolved.path.join(name);
        read_marker(
            &platform,
            &root,
            &MarkerExpectation {
                volume_uuid: &fixture.uuid,
                resolved_path: &path,
                effective_uid: observer.effective_uid().expect("the uid"),
                now_ms: crate::state::now_ms(),
            },
        )
    }

    fn rewrite_marker(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let bytes = std::fs::read(path).expect("the marker is read");
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the marker parses");
        edit(&mut value);
        let mut out = serde_json::to_vec_pretty(&value).expect("the marker encodes");
        out.push(b'\n');
        std::fs::write(path, out).expect("the marker is written");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("the marker is private");
    }

    fn assert_nothing_created(path: &Path) {
        assert!(
            !path.exists(),
            "{} must not exist after a refusal",
            path.display()
        );
    }

    // ---- containment and path resolution --------------------------------

    #[test]
    fn a_real_directory_resolves_from_the_verified_mount_root() {
        let fixture = Fixture::new("area-resolve");
        let declaration = fixture.declaration("example/pool");
        std::fs::create_dir_all(fixture.root.join("example/pool")).expect("the folder exists");
        let resolved = fixture.resolved(&fixture.record, &declaration);
        assert!(resolved.exists());
        assert_eq!(resolved.path, fixture.root.join("example/pool"));
        assert_eq!(resolved.folder, "example/pool");
        assert_eq!(resolved.declaration.folder, "example/pool");
    }

    #[test]
    fn root_dir_empty_and_set_resolve_differently() {
        let fixture = Fixture::new("area-root-dir");
        let declaration = fixture.declaration("example/pool");
        std::fs::create_dir_all(fixture.root.join("example/pool")).expect("the empty-root folder");
        std::fs::create_dir_all(fixture.root.join("managed/example/pool"))
            .expect("the managed folder");

        let empty = fixture.resolved(&fixture.record, &declaration);
        assert_eq!(empty.path, fixture.root.join("example/pool"));

        let managed = fixture.resolved(&fixture.record_with_root_dir("managed"), &declaration);
        assert_eq!(managed.path, fixture.root.join("managed/example/pool"));
        assert_eq!(managed.root_dir, "managed");
    }

    #[test]
    fn a_folder_override_resolves_for_its_own_volume_only() {
        let fixture = Fixture::new("area-override");
        let mut declaration = fixture.declaration("example/pool");
        declaration.overrides.insert(
            fixture.uuid.as_str().to_owned(),
            "elsewhere/pool".to_owned(),
        );
        std::fs::create_dir_all(fixture.root.join("elsewhere/pool")).expect("the override folder");
        let resolved = fixture.resolved(&fixture.record, &declaration);
        assert_eq!(resolved.folder, "elsewhere/pool");
        assert_eq!(resolved.path, fixture.root.join("elsewhere/pool"));
    }

    #[test]
    fn an_absolute_folder_is_refused_with_nothing_created() {
        let fixture = Fixture::new("containment-absolute");
        let declaration = fixture.declaration("/etc");
        let error = fixture.resolve_error(&fixture.record, &declaration, &fixture.platform());
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        assert_nothing_created(&fixture.root.join("etc"));
        assert!(declaration.validate().is_err());
    }

    #[test]
    fn a_parent_component_is_refused_with_nothing_created() {
        let fixture = Fixture::new("containment-parent");
        for folder in ["..", "example/../pool", "example/.."] {
            let declaration = fixture.declaration(folder);
            let error = fixture.resolve_error(&fixture.record, &declaration, &fixture.platform());
            assert_eq!(error.kind(), ErrorKind::RefusedContainment, "{folder}");
            assert!(!fixture.root.join("pool").exists());
        }
        let mut record = fixture.record.clone();
        record.root_dir = "..".to_owned();
        let error = fixture.resolve_error(
            &record,
            &fixture.declaration("example"),
            &fixture.platform(),
        );
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
    }

    #[test]
    fn an_empty_or_edge_component_is_refused() {
        let fixture = Fixture::new("containment-empty");
        for folder in [
            "",
            "example//pool",
            "/example",
            "example/",
            ".",
            "example/./pool",
        ] {
            assert!(
                folder_components("folder", folder).is_err(),
                "`{folder}` is refused"
            );
            let error = fixture.resolve_error(
                &fixture.record,
                &fixture.declaration(folder),
                &fixture.platform(),
            );
            assert_eq!(error.kind(), ErrorKind::RefusedContainment, "{folder}");
        }
        assert_nothing_created(&fixture.root.join("example"));
    }

    #[test]
    fn a_symlinked_component_is_refused_and_nothing_is_created() {
        let fixture = Fixture::new("containment-symlink");
        let outside = fixture.dir.join("outside");
        std::fs::create_dir_all(&outside).expect("the outside directory exists");
        std::fs::create_dir_all(fixture.root.join("example")).expect("the real parent exists");
        std::os::unix::fs::symlink(&outside, fixture.root.join("example/pool"))
            .expect("the fixture link is made");

        let error = fixture.resolve_error(
            &fixture.record,
            &fixture.declaration("example/pool"),
            &fixture.platform(),
        );
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        assert_eq!(
            error.detail()["reason"],
            "the component is a symlink, and only a real directory is walked"
        );
        assert!(
            std::fs::read_dir(&outside)
                .expect("the outside directory lists")
                .next()
                .is_none(),
            "nothing was created through the link"
        );
    }

    #[test]
    fn a_foreign_owned_component_is_refused() {
        let fixture = Fixture::new("containment-foreign");
        std::fs::create_dir_all(fixture.root.join("example/pool")).expect("the folder exists");
        let dirs = Adjust::new().with_foreign_owner("example");
        let error =
            fixture.resolve_error(&fixture.record, &fixture.declaration("example/pool"), &dirs);
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        let reason = error.detail()["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("owned by uid"),
            "the refusal names the owners it saw: {reason}"
        );
    }

    #[test]
    fn a_component_on_another_device_is_refused() {
        let fixture = Fixture::new("containment-device");
        std::fs::create_dir_all(fixture.root.join("example/pool")).expect("the folder exists");
        let dirs = Adjust::new().with_device_shift("example", DeviceId::new(999_999));
        let error =
            fixture.resolve_error(&fixture.record, &fixture.declaration("example/pool"), &dirs);
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        assert!(
            error.detail()["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("device")),
            "the refusal names the device: {:?}",
            error.detail()
        );
    }

    #[test]
    fn a_component_swapped_between_the_check_and_the_open_is_refused() {
        let fixture = Fixture::new("containment-swap");
        let pool = fixture.root.join("example/pool");
        std::fs::create_dir_all(&pool).expect("the folder exists");
        let moved = fixture.root.join("example/pool-moved");
        let dirs = Adjust::swapping("pool", &pool, &moved);
        let error =
            fixture.resolve_error(&fixture.record, &fixture.declaration("example/pool"), &dirs);
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        assert_eq!(
            error.detail()["reason"],
            "the component changed between the check and the open"
        );
        assert!(pool.is_dir(), "the injected swap really replaced the entry");
        assert!(moved.is_dir(), "the original directory is still there");
    }

    #[test]
    fn a_volume_that_is_absent_or_mismatched_refuses_before_any_effect() {
        let fixture = Fixture::new("containment-volume");
        let declaration = fixture.declaration("example/pool");
        let platform = fixture.platform();
        let mounts = fixture.mounts();
        let observer = fixture.observer();

        let absent = fixture.absent_facts();
        let error = resolve(
            &platform,
            &absent,
            &mounts,
            &observer,
            &fixture.record,
            &declaration,
            None,
        )
        .expect_err("an absent volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);

        let mismatched = fixture.mismatched_facts();
        let error = resolve(
            &platform,
            &mismatched,
            &mounts,
            &observer,
            &fixture.record,
            &declaration,
            None,
        )
        .expect_err("a different volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeMismatch);
        assert_nothing_created(&fixture.root.join("example"));
    }

    #[test]
    fn an_undeclared_durability_refuses_every_area_use() {
        let fixture = Fixture::new("containment-durability");
        let mut record = fixture.record.clone();
        record.durability = Durability::Undeclared;
        let error = fixture.resolve_error(
            &record,
            &fixture.declaration("example/pool"),
            &fixture.platform(),
        );
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["durability"], "undeclared");
        assert_nothing_created(&fixture.root.join("example"));
    }

    #[test]
    fn changing_root_dir_after_a_root_exists_moves_nothing() {
        let fixture = Fixture::new("root-dir-change");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let marker_before = std::fs::read(marker_path(&root)).expect("the marker is read");

        // The configuration now resolves somewhere else. Nothing moves.
        let changed = fixture.record_with_root_dir("managed");
        let resolved = fixture.resolved(&changed, &declaration);
        assert_eq!(resolved.path, fixture.root.join("managed/example/pool"));
        assert!(
            !resolved.exists(),
            "the new configuration resolves to nothing"
        );
        assert!(
            fixture.list(&resolved).roots.is_empty(),
            "no root is reported at the configured path"
        );
        assert!(
            root.path.join(ROOT_MARKER_FILE).is_file(),
            "the root stays put"
        );
        assert_eq!(
            std::fs::read(marker_path(&root)).expect("the marker is read"),
            marker_before,
            "the marker is unchanged"
        );
        assert!(
            !fixture.root.join("managed").exists(),
            "nothing was created"
        );

        // The recorded configuration still finds it and reports both paths.
        let recorded = fixture.resolved(&fixture.record, &declaration);
        assert_eq!(recorded.path, root.path.parent().expect("the area folder"));
        let listing = fixture.list(&recorded);
        assert_eq!(listing.roots.len(), 1);
        assert_eq!(
            listing.roots[0].recorded_path.as_deref(),
            Some(root.path.as_path())
        );
        assert_eq!(listing.roots[0].configured_path, root.path);
        assert_eq!(listing.roots[0].state, MarkerStateName::Owned);
    }

    // ---- area declarations ----------------------------------------------

    #[test]
    fn a_declaration_round_trips_through_the_state_directory() {
        let fixture = Fixture::new("area-declare");
        let declaration = fixture.declaration("example/pool");
        let written = declare(&fixture.state, &declaration, 1_000).expect("the declaration writes");
        assert_eq!(written.generation, 1);
        assert_eq!(written.schema, "barge.area/1");
        let path = super::declaration_path(&fixture.state, "example", "pool").expect("the path");
        assert!(path.is_file());
        let mode = std::fs::metadata(&path)
            .expect("the file has metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a record is private");

        let read_back = show(&fixture.state, "example", "pool")
            .expect("the declarations read")
            .expect("the declaration is there");
        assert_eq!(read_back, written);

        let again = declare(&fixture.state, &declaration, 2_000).expect("declaring again");
        assert_eq!(again.generation, 2, "a change increases the generation");
        assert!(show(&fixture.state, "example", "other")
            .expect("the declarations read")
            .is_none());
    }

    #[test]
    fn a_stored_declaration_is_read_strictly() {
        let fixture = Fixture::new("area-strict");
        let declaration = fixture.declaration("example/pool");
        declare(&fixture.state, &declaration, 1_000).expect("the declaration writes");
        let path = super::declaration_path(&fixture.state, "example", "pool").expect("the path");

        rewrite_marker(&path, |value| {
            value["schema"] = "barge.area/2".into();
        });
        let error = read_declarations(&fixture.state).expect_err("an unknown major refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        rewrite_marker(&path, |value| {
            value["schema"] = "barge.area/1".into();
            value["unknown"] = true.into();
        });
        assert!(
            read_declarations(&fixture.state).is_err(),
            "an unknown field refuses"
        );

        std::fs::write(
            &path,
            vec![b'x'; crate::contract::AREA_DECLARATION_CAP_BYTES + 1],
        )
        .expect("the oversized file is written");
        let error = read_declarations(&fixture.state).expect_err("an oversized document refuses");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);
    }

    #[test]
    fn two_declarations_over_one_folder_are_refused() {
        let fixture = Fixture::new("area-collision");
        fixture.register(&fixture.record);
        declare(&fixture.state, &fixture.declaration("example/pool"), 1_000)
            .expect("the first declaration writes");
        let mut second = fixture.declaration("example/pool");
        second.area = "other".to_owned();
        let error = declare(&fixture.state, &second, 2_000).expect_err("the collision refuses");
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        assert_eq!(error.detail()["conflicts_with"], "example/pool");
        let path = super::declaration_path(&fixture.state, "example", "other").expect("the path");
        assert_nothing_created(&path);

        // Nesting is refused too: one folder inside another has two owners.
        let mut nested = fixture.declaration("example");
        nested.area = "outer".to_owned();
        let error = declare(&fixture.state, &nested, 3_000).expect_err("nesting refuses");
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
    }

    #[test]
    fn a_declaration_over_scratch_or_caches_is_refused() {
        let fixture = Fixture::new("area-managed");
        fixture.register(&fixture.record);
        for folder in ["scratch", "caches", "scratch/project", "caches/nested"] {
            let mut declaration = fixture.declaration(folder);
            declaration.area = "managed".to_owned();
            let error = declare(&fixture.state, &declaration, 1_000)
                .expect_err("a managed folder is refused");
            assert_eq!(error.kind(), ErrorKind::RefusedContainment, "{folder}");
        }
        // A per-volume override that lands on scratch is refused the same way.
        let mut override_declaration = fixture.declaration("example/pool");
        override_declaration.overrides.insert(
            fixture.uuid.as_str().to_owned(),
            "scratch/example".to_owned(),
        );
        let error = declare(&fixture.state, &override_declaration, 2_000)
            .expect_err("an override over scratch is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
    }

    #[test]
    fn a_namespace_or_folder_that_escapes_the_volume_is_refused() {
        let fixture = Fixture::new("area-escape");
        for namespace in ["../escape", "a/b", "..", "Example", ""] {
            let mut declaration = fixture.declaration("example/pool");
            declaration.namespace = namespace.to_owned();
            assert!(
                declaration.validate().is_err(),
                "namespace `{namespace}` is refused"
            );
            assert_eq!(
                declare(&fixture.state, &declaration, 1_000)
                    .expect_err("the declaration is refused")
                    .kind(),
                ErrorKind::RefusedContainment,
                "{namespace}"
            );
        }
        for name in ["../x", "a/b", "Capacity", ""] {
            assert!(validate_name("area", name).is_err(), "{name}");
        }
        validate_name("area", "a-valid.area_name").expect("a valid name is accepted");
        assert!(!fixture.state.areas_dir().exists(), "nothing was written");
    }

    #[test]
    fn the_effective_declaration_is_the_callers_and_is_never_widened() {
        let fixture = Fixture::new("area-effective");
        let mut stored_declaration = fixture.declaration("example/pool");
        stored_declaration.prune = super::PrunePolicy::OldestIdleAfter { age_seconds: 60 };
        stored_declaration.absent = super::AbsentPolicy::FallbackLocal {
            path: PathBuf::from("/Users/a-fixture-account/fallback"),
        };
        let stored = declare(&fixture.state, &stored_declaration, 1_000)
            .expect("the stored declaration writes");

        // The caller's policies are narrower than the stored document's: the
        // stored declaration must not widen them.
        let mut caller = fixture.declaration("example/pool");
        caller.prune = super::PrunePolicy::None;
        caller.absent = super::AbsentPolicy::Fail {
            attach: "the build volume".to_owned(),
        };
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();
        let resolved = resolve(
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.record,
            &caller,
            Some(&stored),
        )
        .expect("the caller's declaration resolves");
        assert_eq!(resolved.declaration.prune, super::PrunePolicy::None);
        assert!(matches!(
            resolved.declaration.absent,
            super::AbsentPolicy::Fail { .. }
        ));
        assert_eq!(
            resolved.declaration.reserve,
            super::ReserveParticipation::Counts
        );
        assert!(
            resolved.declaration.stored_differs,
            "the difference is reported, and the caller's policy still stands"
        );

        // The created root's marker records the caller's effective declaration
        // exactly.
        let root = super::create_root(
            &platform,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the root is created");
        let marker = match read_owned(&fixture, &resolved, &root.root_id) {
            MarkerState::Owned(marker) => marker,
            other => panic!("the marker is owned: {other:?}"),
        };
        let effective = marker
            .area
            .effective
            .as_ref()
            .expect("the effective declaration is recorded");
        assert_eq!(effective.prune, super::PrunePolicy::None);
        assert!(matches!(effective.absent, super::AbsentPolicy::Fail { .. }));
    }

    #[test]
    fn an_invalid_declaration_is_refused_without_writing() {
        let fixture = Fixture::new("area-invalid");
        let mut declaration = fixture.declaration("example/pool");
        declaration
            .overrides
            .insert("not-a-uuid".to_owned(), "elsewhere".to_owned());
        assert!(declare(&fixture.state, &declaration, 1_000).is_err());
        let mut declaration = fixture.declaration("example/pool");
        declaration.prune = super::PrunePolicy::OldestIdleAfter { age_seconds: 0 };
        assert!(declare(&fixture.state, &declaration, 1_000).is_err());
        let mut declaration = fixture.declaration("example/pool");
        declaration.absent = super::AbsentPolicy::FallbackLocal {
            path: PathBuf::from("relative"),
        };
        assert!(declare(&fixture.state, &declaration, 1_000).is_err());
        assert!(read_declarations(&fixture.state)
            .expect("the declarations read")
            .documents
            .is_empty());
    }

    // ---- owned roots, markers and holds ---------------------------------

    #[test]
    fn creating_a_root_writes_its_marker_and_identity() {
        let fixture = Fixture::new("root-create");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        assert_eq!(root.root_id.len(), 32);
        assert_eq!(root.nonce.len(), 32);
        assert_eq!(root.identity.device, fixture.device);
        assert!(root.path.is_dir());
        assert_eq!(root.path.parent(), Some(resolved.path.as_path()));
        let mode = std::fs::metadata(&root.path)
            .expect("the root has metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, ROOT_DIRECTORY_MODE, "a root is private");
        assert_eq!(root.area.namespace, "example");
        assert_eq!(root.area.area, "pool");
        let marker_bytes = std::fs::read(marker_path(&root)).expect("the marker is read");
        assert_eq!(root.marker_digest, sha256_hex(&marker_bytes));
        let marker = match read_owned(&fixture, &resolved, &root.root_id) {
            MarkerState::Owned(marker) => marker,
            other => panic!("the marker is owned: {other:?}"),
        };
        assert_eq!(marker.root_id, root.root_id);
        assert_eq!(marker.kind, RootKind::AreaRoot);
        assert_eq!(marker.volume_uuid, fixture.uuid);
        assert_eq!(marker.origin, super::RootOrigin::Created);
        assert_eq!(marker.resolved_path, root.path);
        assert!(marker.conclusion.is_none());

        // A second creation gets a different root, never a shared one.
        let (second, _) = fixture.create_root(&fixture.record, &declaration);
        assert_ne!(second.root_id, root.root_id);
        assert_ne!(second.nonce, root.nonce);
    }

    #[test]
    fn an_absent_marker_is_not_an_owned_root() {
        let fixture = Fixture::new("marker-absent");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        std::fs::remove_file(marker_path(&root)).expect("the marker is removed");
        assert_eq!(
            read_owned(&fixture, &resolved, &root.root_id),
            MarkerState::Absent
        );
        let listing = fixture.list(&resolved);
        assert_eq!(listing.roots[0].state, MarkerStateName::Absent);
        assert!(listing.roots[0].root_id.is_none());
    }

    #[test]
    fn a_symlinked_or_foreign_marker_is_not_owned() {
        let fixture = Fixture::new("marker-not-owned");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let path = marker_path(&root);
        let elsewhere = fixture.dir.join("elsewhere.json");
        std::fs::write(&elsewhere, std::fs::read(&path).expect("the marker reads"))
            .expect("the copy is written");

        std::fs::remove_file(&path).expect("the marker is removed");
        std::os::unix::fs::symlink(&elsewhere, &path).expect("the fixture link is made");
        match read_owned(&fixture, &resolved, &root.root_id) {
            MarkerState::Unknown { reason } => assert!(reason.contains("symlink"), "{reason}"),
            other => panic!("a symlinked marker is unknown: {other:?}"),
        }

        std::fs::remove_file(&path).expect("the link is removed");
        std::fs::write(&path, std::fs::read(&elsewhere).expect("the copy reads"))
            .expect("the marker is restored");
        let dirs = Adjust::new().with_foreign_owner(ROOT_MARKER_FILE);
        let platform = fixture.platform();
        let observer = fixture.observer();
        let opened = platform.open_path(&root.path).expect("the root opens");
        match read_marker(
            &dirs,
            &opened,
            &MarkerExpectation {
                volume_uuid: &fixture.uuid,
                resolved_path: &root.path,
                effective_uid: observer.effective_uid().expect("the uid"),
                now_ms: crate::state::now_ms(),
            },
        ) {
            MarkerState::Unknown { reason } => assert!(reason.contains("uid"), "{reason}"),
            other => panic!("a foreign-owned marker is unknown: {other:?}"),
        }
    }

    #[test]
    fn an_oversized_or_future_dated_marker_is_not_owned() {
        let fixture = Fixture::new("marker-bounds");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let path = marker_path(&root);
        let valid = std::fs::read(&path).expect("the marker reads");

        std::fs::write(&path, vec![b'x'; crate::contract::MARKER_CAP_BYTES + 1])
            .expect("the oversized marker is written");
        assert!(matches!(
            read_owned(&fixture, &resolved, &root.root_id),
            MarkerState::Unknown { .. }
        ));

        // Restore a valid marker, then date it in the future.
        std::fs::write(&path, &valid).expect("the marker is restored");
        rewrite_marker(&path, |value| {
            value["created_ms"] = (crate::state::now_ms() + 3_600_000).into();
        });
        match read_owned(&fixture, &resolved, &root.root_id) {
            MarkerState::Unknown { reason } => assert!(reason.contains("future"), "{reason}"),
            other => panic!("a future-dated marker is unknown: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_version_or_divergent_marker_is_not_owned() {
        let fixture = Fixture::new("marker-version");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let path = marker_path(&root);
        let valid = std::fs::read(&path).expect("the marker reads");

        for edit in [
            (|value: &mut serde_json::Value| {
                value["schema"] = "barge.root/2".into();
            }) as fn(&mut serde_json::Value),
            |value: &mut serde_json::Value| {
                value["owner"] = "someone-else".into();
            },
            |value: &mut serde_json::Value| {
                value["unknown"] = true.into();
            },
            |value: &mut serde_json::Value| {
                value["volume_uuid"] = OTHER_UUID.into();
            },
            |value: &mut serde_json::Value| {
                value["resolved_path"] = "/somewhere/else".into();
            },
            |value: &mut serde_json::Value| {
                value["root_id"] = "not-a-root-id".into();
            },
            |value: &mut serde_json::Value| {
                value["conclusion"] = serde_json::json!({
                    "concluded_ms": 1,
                    "outcome": "closed",
                    "closure": "positive",
                    "kept_reason": "none",
                    "nested_scope": "exclusive"
                });
            },
        ] {
            std::fs::write(&path, &valid).expect("the marker is restored");
            rewrite_marker(&path, edit);
            let state = read_owned(&fixture, &resolved, &root.root_id);
            assert!(
                matches!(state, MarkerState::Unknown { .. }),
                "a divergent marker is never owned: {state:?}"
            );
        }
        std::fs::write(&path, &valid).expect("the marker is restored");
        assert!(matches!(
            read_owned(&fixture, &resolved, &root.root_id),
            MarkerState::Owned(_)
        ));
    }

    #[test]
    fn a_root_listing_reports_owned_unmarked_and_symlinked_entries() {
        let fixture = Fixture::new("root-list");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        std::fs::create_dir(resolved.path.join("plain-directory"))
            .expect("the unmarked directory is made");
        std::os::unix::fs::symlink(&root.path, resolved.path.join("linked-root"))
            .expect("the fixture link is made");
        std::fs::write(resolved.path.join("a-file"), b"x").expect("the file is written");

        let listing = fixture.list(&resolved);
        assert_eq!(
            listing.skipped_non_directories, 1,
            "the file is only counted"
        );
        assert_eq!(listing.roots.len(), 3);
        let owned = listing
            .roots
            .iter()
            .find(|row| row.name == root.root_id)
            .expect("the owned root is listed");
        assert_eq!(owned.state, MarkerStateName::Owned);
        assert!(owned.idle, "a root with no holds is idle");
        let unmarked = listing
            .roots
            .iter()
            .find(|row| row.name == "plain-directory")
            .expect("the unmarked directory is listed");
        assert_eq!(unmarked.state, MarkerStateName::Absent);
        assert!(unmarked.root_id.is_none());
        let linked = listing
            .roots
            .iter()
            .find(|row| row.name == "linked-root")
            .expect("the symlink is listed, never followed");
        assert_eq!(linked.state, MarkerStateName::Unknown);
        assert!(linked
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("symbolic link")));
    }

    #[test]
    fn a_listing_is_bounded_and_reports_what_it_left_out() {
        let fixture = Fixture::new("root-list-bound");
        let declaration = fixture.declaration("example/pool");
        let (_, resolved) = fixture.create_root(&fixture.record, &declaration);
        for index in 0..3 {
            std::fs::create_dir(resolved.path.join(format!("extra-{index}")))
                .expect("the directory is made");
        }
        let platform = fixture.platform();
        let observer = fixture.observer();
        let listing = list_roots(&platform, &observer, &fixture.state, &resolved, 2)
            .expect("the listing reads");
        assert_eq!(listing.roots.len(), 2);
        assert!(listing.truncated, "the listing says it was cut");
        assert!(listing.examined >= 4);
    }

    #[test]
    fn a_hold_is_recorded_decided_released_and_makes_a_root_busy() {
        let fixture = Fixture::new("hold-lifecycle");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let request = HoldRequest {
            root_id: root.root_id.clone(),
            holder: "prover".to_owned(),
            pid: std::process::id(),
            scope: HoldScope::Write,
            purpose: "a fixture holder".to_owned(),
        };
        let held = hold(&fixture.state, &observer, &request).expect("the hold is written");
        assert!(held.path.is_file());
        let mode = std::fs::metadata(&held.path)
            .expect("the hold has metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a hold record is private");
        assert_eq!(
            hold(&fixture.state, &observer, &request)
                .expect("re-holding is idempotent")
                .record
                .holder,
            "prover"
        );

        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(reading.holds.len(), 1);
        assert_eq!(reading.holds[0].1, HoldState::Live);
        assert!(!reading.is_idle(), "a live hold makes the root busy");
        let listing = fixture.list(&resolved);
        assert_eq!(listing.roots[0].holds.len(), 1);
        assert!(!listing.roots[0].idle);

        release(&fixture.state, &root.root_id, "prover").expect("the hold releases");
        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert!(reading.holds.is_empty());
        assert!(reading.is_idle(), "no live hold is idle");
        // Releasing again is not an error: it is already released.
        release(&fixture.state, &root.root_id, "prover").expect("a second release");
    }

    #[test]
    fn a_hold_records_are_bounded_and_only_this_holders_release_removes_one() {
        let fixture = Fixture::new("hold-bounds");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let pid = std::process::id();
        for index in 0..crate::contract::HOLD_ROWS {
            let request = HoldRequest {
                root_id: root.root_id.clone(),
                holder: format!("h{index}"),
                pid,
                scope: HoldScope::Read,
                purpose: "a fixture holder".to_owned(),
            };
            hold(&fixture.state, &observer, &request).expect("the hold is written");
        }
        let error = hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "overflow".to_owned(),
                pid,
                scope: HoldScope::Read,
                purpose: "one too many".to_owned(),
            },
        )
        .expect_err("the per-root cap refuses");
        assert_eq!(error.kind(), ErrorKind::ResourceBound);

        // A record that names a different holder is not removed by this release.
        let path = fixture
            .state
            .holds_dir()
            .join(&root.root_id)
            .join("h1.json");
        rewrite_marker(&path, |value| {
            value["holder"] = "someone-else".into();
        });
        let error = release(&fixture.state, &root.root_id, "h1").expect_err("the mismatch refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(path.is_file(), "the unknown hold is kept");
        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(reading.total, crate::contract::HOLD_ROWS);
        let unknown = reading
            .holds
            .iter()
            .find(|(record, state, _)| record.holder == "h1" && *state == HoldState::Unknown)
            .is_some();
        assert!(unknown, "the mismatched record is unknown, not stale");
    }

    #[test]
    fn a_hold_from_another_boot_is_stale_and_an_unreadable_process_is_unknown() {
        let fixture = Fixture::new("hold-decisions");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let mut observer = fixture.observer();
        observer.boot = "1600000000.0".to_owned();
        let held = hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "previous-boot".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "from before the reboot".to_owned(),
            },
        )
        .expect("the hold is written");
        let reading =
            read_holds(&fixture.state, &fixture.observer(), &root.root_id).expect("the holds read");
        assert_eq!(reading.holds[0].1, HoldState::Stale);
        assert!(reading.is_idle(), "a stale hold does not keep a root busy");

        // A pid whose start identity cannot be read is unknown, never stale.
        let path = held.path.clone();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("the hold reads"))
                .expect("the hold parses");
        value["pid"] = 4242.into();
        value["boot_identity"] = BOOT.into();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&value).expect("the hold encodes"),
        )
        .expect("the hold is written");
        let mut observer = fixture.observer();
        observer.unreadable.insert(4242);
        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(reading.holds[0].1, HoldState::Unknown);
        assert!(!reading.is_idle(), "unknown blocks idleness");
    }

    #[test]
    fn a_hold_for_a_gone_process_is_refused_at_write_time() {
        let fixture = Fixture::new("hold-gone");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let error = hold(
            &fixture.state,
            &fixture.observer(),
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "gone".to_owned(),
                pid: 999_999,
                scope: HoldScope::Write,
                purpose: "a gone process".to_owned(),
            },
        )
        .expect_err("a gone process cannot hold");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
    }

    #[test]
    fn a_root_is_created_with_the_area_directory_mode() {
        let fixture = Fixture::new("root-modes");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        assert_eq!(resolved.path, fixture.root.join("example/pool"));
        let mode = std::fs::metadata(&resolved.path)
            .expect("the area has metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, AREA_DIRECTORY_MODE,
            "a folder Barge creates is a plain directory"
        );
        assert!(root.path.is_dir());
    }

    #[test]
    fn a_root_directory_that_is_already_taken_is_retried_not_adopted() {
        // The fresh-id loop is the design's race answer; here the first chosen
        // name is planted by an injected random source so the retry is proved.
        struct PlantedRandom {
            inner: MacOs,
            calls: RefCell<usize>,
        }
        impl DirOps for PlantedRandom {
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
                self.inner.child_facts(parent, name)
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
                kind: crate::os::RemoveKind,
            ) -> Result<crate::os::ChildRemoval> {
                self.inner.remove_child(parent, name, kind)
            }
            fn rename_child(
                &self,
                parent: &DirHandle,
                from: &str,
                to: &str,
            ) -> Result<crate::os::RenameOutcome> {
                self.inner.rename_child(parent, from, to)
            }
            fn dir_facts(&self, handle: &DirHandle) -> Result<EntryFacts> {
                self.inner.dir_facts(handle)
            }
            fn set_dir_mode(&self, handle: &DirHandle, mode: u32) -> Result<()> {
                self.inner.set_dir_mode(handle, mode)
            }
            fn recheck_dir(&self, handle: &DirHandle) -> Result<DirId> {
                self.inner.recheck_dir(handle)
            }
            fn random_bytes(&self, length: usize) -> Result<Vec<u8>> {
                let mut calls = self.calls.borrow_mut();
                *calls += 1;
                // The root id is asked for first, the nonce second.
                if *calls == 1 {
                    return Ok(vec![0u8; length]);
                }
                self.inner.random_bytes(length)
            }
        }

        let fixture = Fixture::new("root-retry");
        let declaration = fixture.declaration("example/pool");
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let dirs = PlantedRandom {
            inner: fixture.platform(),
            calls: RefCell::new(0),
        };
        let observer = fixture.observer();
        let planted = resolved.path.join("0".repeat(32));
        // The area folder is created first, then the planted root name exists.
        let first = super::create_root(
            &dirs,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the first root is created");
        assert_eq!(first.root_id, "0".repeat(32));

        let dirs = PlantedRandom {
            inner: fixture.platform(),
            calls: RefCell::new(0),
        };
        let second = super::create_root(
            &dirs,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the taken name is retried with a fresh id");
        assert_ne!(
            second.root_id.as_str(),
            planted
                .file_name()
                .expect("a name")
                .to_str()
                .expect("an ASCII name")
        );
        assert_ne!(second.root_id, first.root_id);
        assert!(planted.is_dir(), "the first root is untouched");
    }

    #[test]
    fn a_fixture_volume_never_writes_to_a_real_drive() {
        // The whole module's fixtures live inside a fresh task-owned `run.*` root
        // under the scratch base; this asserts the path the walk used is inside it.
        let fixture = Fixture::new("root-scratch");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        assert!(
            root.path.starts_with(fixture.dir.path()),
            "{} is inside the fixture",
            root.path.display()
        );
        assert!(
            root.path.starts_with(crate::testkit::scratch_base()),
            "{} is inside the task-owned scratch root",
            root.path.display()
        );
    }
    #[test]
    fn a_declaration_on_an_undeclared_durability_volume_is_refused() {
        let fixture = Fixture::new("area-declare-durability");
        let mut record = fixture.record.clone();
        record.durability = Durability::Undeclared;
        fixture.register(&record);
        let error = declare(&fixture.state, &fixture.declaration("example/pool"), 1_000)
            .expect_err("an undeclared durability refuses a declaration");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        let path = super::declaration_path(&fixture.state, "example", "pool").expect("the path");
        assert_nothing_created(&path);
    }
    #[test]
    fn a_hold_file_that_is_not_private_or_ours_is_unknown() {
        let fixture = Fixture::new("hold-private");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let held = hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Read,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        std::fs::set_permissions(&held.path, std::fs::Permissions::from_mode(0o644))
            .expect("the fixture mode is set");
        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(reading.holds[0].1, HoldState::Unknown);
        assert!(!reading.is_idle(), "an unknown hold blocks idleness");
    }
    #[test]
    fn a_component_swapped_behind_a_competing_create_is_refused_not_adopted() {
        let fixture = Fixture::new("area-competing-create");
        let declaration = fixture.declaration("example/pool");
        std::fs::create_dir_all(fixture.root.join("example")).expect("the parent exists");
        let pool = fixture.root.join("example/pool");
        let moved = fixture.root.join("example/pool-moved");
        // The competing creator wins the mkdir, then the entry it left behind is
        // swapped between the check and the open. The adoption must refuse it.
        let dirs = Adjust::swapping("pool", &pool, &moved).with_competing_create("pool");
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let observer = fixture.observer();
        let error = super::create_root(
            &dirs,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect_err("a swapped competing directory is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);
        assert_eq!(
            error.detail()["reason"],
            "the component changed between the check and the open"
        );
        assert!(moved.is_dir(), "the competing directory is preserved");
        assert!(pool.is_dir(), "the injected swap really replaced the entry");
        let roots = std::fs::read_dir(&pool).expect("the area lists");
        assert_eq!(
            roots.count(),
            0,
            "no root was created inside a directory the walk did not prove"
        );
    }

    #[test]
    fn root_creation_proves_the_volume_before_creating_state_or_a_lock() {
        let fixture = Fixture::new("root-volume-preflight");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let platform = fixture.platform();
        let mounts = fixture.mounts();
        let observer = fixture.observer();
        let lock = fixture.state.volume_lock_path(&fixture.uuid);
        assert!(!lock.exists(), "the fixture starts with no volume lock");

        let absent = fixture.absent_facts();
        let error = super::create_root_on_volume(
            &fixture.state,
            &platform,
            &absent,
            &mounts,
            &observer,
            &fixture.uuid,
            &declaration,
            None,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect_err("an absent volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
        assert!(!lock.exists(), "an absent volume creates no volume lock");
        assert!(
            !fixture.root.join("example").exists(),
            "an absent volume creates no folder"
        );

        let mismatched = fixture.mismatched_facts();
        let error = super::create_root_on_volume(
            &fixture.state,
            &platform,
            &mismatched,
            &mounts,
            &observer,
            &fixture.uuid,
            &declaration,
            None,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect_err("a different volume refuses");
        assert_eq!(error.kind(), ErrorKind::VolumeMismatch);
        assert!(!lock.exists(), "a mismatched volume creates no volume lock");
        assert!(!fixture.root.join("example").exists());

        // The same call succeeds once the volume is present, which proves the
        // refusals above were the volume and not the fixture.
        let present = fixture.facts();
        let (volume, root) = super::create_root_on_volume(
            &fixture.state,
            &platform,
            &present,
            &mounts,
            &observer,
            &fixture.uuid,
            &declaration,
            None,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the present volume creates the root");
        assert_eq!(volume.record.volume_uuid, fixture.uuid);
        assert!(root.path.is_dir());
        assert!(lock.exists(), "a successful creation takes the volume lock");
    }
}
