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
//! - **The stored declaration governs.** A `barge.area/1` record is the
//!   authority once it exists: a library caller presenting the same declaration
//!   proceeds, and one presenting a different folder, override, `absent`,
//!   `prune` or `reserve` policy is refused before any effect, naming every
//!   field that differs. The first creation, when no record exists, publishes
//!   the caller's validated declaration under the same rules `barge area
//!   declare` uses. The effective declaration recorded in a root's marker is
//!   always the governing record's, so no report implies that a differing caller
//!   policy was applied (stage 4 F6 contract D2; Q-G).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::{
    RowSet, AREA_DECLARATION_CAP_BYTES, AREA_ROWS, HOLD_CAP_BYTES, HOLD_ROWS, LISTING_NAME_ROWS,
    MARKER_CAP_BYTES, ROOT_ROWS,
};
use crate::digest::sha256_hex;
use crate::error::{Error, ErrorKind, Result};
use crate::os::{
    self, DeviceId, DirHandle, DirId, DirOps, EntryKind, MountRoot, ProcessObserver, VolumeFacts,
    VolumeIdentity, VolumeLocator, VolumeUuid,
};
use crate::registry::{self, VolumeRecord};
use crate::state::{self, Bounds, Deadline, ReadOutcome, StatePaths, READ_DEADLINE};

/// The owned-root marker's file name inside every root Barge creates.
pub const ROOT_MARKER_FILE: &str = ".barge-root.json";

/// The mode a root directory is created with.
pub const ROOT_DIRECTORY_MODE: u32 = 0o700;

/// How many entries one root's last-activity scan may examine (stage 4 design,
/// "Last activity").
///
/// The prune plan's own bound is 200 000 names per area listing; this is the
/// same number spent inside one root, so a root that holds more than its
/// listing bound reports that it was not fully scanned instead of claiming an
/// age it did not observe.
pub const ACTIVITY_ENTRIES: usize = 200_000;

/// How deep the last-activity scan walks. It is the deletion walk's own depth
/// bound, so a scan and a delete of one root agree about what they reached.
pub const ACTIVITY_DEPTH: usize = 64;

/// How long one root's last-activity scan may take.
pub const ACTIVITY_WALL: Duration = Duration::from_secs(5);

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
///
/// This is an additive `barge.area/1` field value: `release_on_reference` is
/// one a reader that knows only `none` and `oldest_idle_after` refuses as
/// unknown, rather than approximating it with a policy it never declared.
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
    /// A root is released only by the release gate, when nothing references it.
    ///
    /// This is not an age policy: `maintain` never plans one of its roots and
    /// the space path never takes one for the reserve, whatever the area's
    /// `reserve` says. Only the release gate may release it, and only when that
    /// gate reads this policy as permission by release.
    ReleaseOnReference,
}

impl PrunePolicy {
    /// The declared age, when the policy has one.
    pub const fn age(&self) -> Option<Duration> {
        match self {
            Self::None | Self::ReleaseOnReference => None,
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

impl ReserveParticipation {
    /// The participation's name, as a report spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Counts => "counts",
            Self::Excluded => "excluded",
        }
    }
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
/// It is the governing declaration with the folder resolved for the volume: the
/// stored `barge.area/1` record's values whenever one exists, and the caller's
/// declaration only on first creation, when that declaration is what the state
/// directory received. A caller that presents a different declaration is refused
/// rather than substituted, so this record never describes a policy the state
/// directory did not hold (stage 4 F6 contract D2; Q-G).
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
}

impl EffectiveDeclaration {
    /// The governing declaration as the effective policy, with `folder` already
    /// resolved for one volume.
    pub fn new(governing: &AreaDeclaration, folder: String) -> Self {
        Self {
            namespace: governing.namespace.clone(),
            area: governing.area.clone(),
            folder,
            absent: governing.absent.clone(),
            prune: governing.prune.clone(),
            reserve: governing.reserve,
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

/// One versioned marker: the identity and provenance of an owned root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootMarker {
    /// `barge.root/1` or `barge.root/2`.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<PathBuf>,
    /// The root path below the recorded mount, for version 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_relative_path: Option<String>,
    /// The mount observed when version 2 was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_at_record: Option<PathBuf>,
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
    /// The verified live mount, when path validation is required.
    pub mount_point: Option<&'a Path>,
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

/// How a registered volume's live mount is found for one operation.
///
/// `Hint` verifies the stored mount point, the lookup every ordinary command uses.
/// `ByUuid` finds the one live mount that carries the record's UUID through a
/// complete census, as stage 4's "Finding the volume after a remount" requires. The
/// stored point is never identity under `ByUuid`: a stale point that another volume
/// now occupies is not the registered volume, and a UUID mounted twice is refused.
/// Only the release gate and the deletion it authorises use `ByUuid`.
#[derive(Clone, Copy)]
pub(crate) enum MountLookup<'a> {
    /// Verify the registry's stored mount point.
    Hint,
    /// Find the live mount that carries the record's UUID.
    ByUuid(&'a dyn VolumeLocator),
}

impl std::fmt::Debug for MountLookup<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The locator is a trait object with no useful text, so only the choice shows.
        match self {
            Self::Hint => formatter.write_str("Hint"),
            Self::ByUuid(_) => formatter.write_str("ByUuid"),
        }
    }
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
    /// Verify the record's volume and open its mount root as a descriptor, through the
    /// stored mount point. Every ordinary command uses this lookup.
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
        Self::open_with(dirs, facts, mounts, MountLookup::Hint, record)
    }

    /// [`VolumeRoot::open`] through the given lookup. The containment that follows is
    /// the same for both: the descriptor's device must match the verified volume.
    pub(crate) fn open_with(
        dirs: &dyn DirOps,
        facts: &dyn VolumeFacts,
        mounts: &dyn MountRoot,
        lookup: MountLookup<'_>,
        record: &VolumeRecord,
    ) -> Result<Self> {
        let (mount, identity) = match lookup {
            MountLookup::Hint => {
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
                let verified =
                    os::verify_volume(facts, mounts, mount_hint, &record.expected_volume())?;
                #[cfg(test)]
                crate::tape::volume(
                    "area::VolumeRoot::open_with",
                    crate::tape::Identity::volume(
                        Some(record.volume_uuid.as_str()),
                        Some(verified.path()),
                    ),
                    crate::tape::Outcome::facts("verified"),
                );
                (verified.path().to_path_buf(), verified.identity().clone())
            }
            MountLookup::ByUuid(locator) => {
                let located = os::locate_volume(locator, facts, mounts, &record.volume_uuid)?;
                #[cfg(test)]
                crate::tape::volume(
                    "area::VolumeRoot::open_with",
                    crate::tape::Identity::volume(
                        Some(record.volume_uuid.as_str()),
                        Some(located.path()),
                    ),
                    crate::tape::Outcome::facts("located"),
                );
                (located.path().to_path_buf(), located.identity().clone())
            }
        };
        let handle = dirs.open_path(&mount)?;
        if handle.identity().device != identity.device {
            #[cfg(test)]
            crate::tape::volume(
                "area::VolumeRoot::open_with",
                crate::tape::Identity::volume(Some(record.volume_uuid.as_str()), Some(&mount)),
                crate::tape::Outcome::refused("device_mismatch"),
            );
            return Err(Error::new(
                ErrorKind::VolumeMismatch,
                format!(
                    "the mount root {} opened on device {}, but the volume reported {}",
                    mount.display(),
                    handle.identity().device,
                    identity.device
                ),
            )
            .with_detail("path", mount.display().to_string().into())
            .with_detail("opened_device", handle.identity().device.to_string().into())
            .with_detail("volume_device", identity.device.to_string().into()));
        }
        #[cfg(test)]
        crate::tape::volume(
            "area::VolumeRoot::open_with",
            crate::tape::Identity::volume(Some(record.volume_uuid.as_str()), Some(&mount)),
            crate::tape::Outcome::facts("opened"),
        );
        Ok(Self {
            handle,
            identity,
            mount_point: mount,
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

/// Walk `root_dir` and then one feature folder from an opened mount root.
///
/// Every component goes through the same descriptor walk: checked with `lstat`,
/// refused unless it is this account's real directory on this device, opened
/// without following a link and compared with the checked identity. The first
/// absent component ends the walk and the rest are reported as still to create,
/// so resolution creates nothing and a caller that creates knows exactly what is
/// missing.
fn walk_folder_chain(
    dirs: &dyn DirOps,
    root_handle: DirHandle,
    base: &Path,
    device: DeviceId,
    effective_uid: u32,
    root_dir: &str,
    folder: &str,
) -> Result<Walk> {
    let root_dir_components = if root_dir.is_empty() {
        Vec::new()
    } else {
        folder_components("root_dir", root_dir)?
    };
    let walked_root = walk_folder(
        dirs,
        root_handle,
        &root_dir_components,
        device,
        base,
        effective_uid,
    )?;
    let folder_components = folder_components("folder", folder)?;
    if walked_root.exists() {
        return walk_folder(
            dirs,
            walked_root.handle,
            &folder_components,
            device,
            &walked_root.path,
            effective_uid,
        );
    }
    // The root_dir itself is missing, so everything under it is still to
    // create; the path is the composed observation either way.
    let mut path = walked_root.path.clone();
    for component in &folder_components {
        path.push(component);
    }
    Ok(Walk {
        handle: walked_root.handle,
        remaining: walked_root
            .remaining
            .iter()
            .chain(folder_components.iter())
            .cloned()
            .collect(),
        path,
    })
}

/// Barge's own feature folders under `root_dir` (architecture, "Locations on a
/// volume"): the two folders Barge implements itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Per-project scratch areas.
    Scratch,
    /// Package-cache placements.
    Caches,
}

/// The folder name one volume gives one of Barge's own features: the record's
/// override, else the design's default name.
pub fn feature_folder(record: &VolumeRecord, feature: Feature) -> Result<String> {
    let (override_folder, default) = match feature {
        Feature::Scratch => (record.folders.scratch.clone(), "scratch"),
        Feature::Caches => (record.folders.caches.clone(), "caches"),
    };
    let folder = override_folder.unwrap_or_else(|| default.to_owned());
    folder_components("folder", &folder)?;
    Ok(folder)
}

/// One of Barge's own feature folders, resolved for one volume.
///
/// It is the cache placement's half of [`ResolvedArea`]: the same descriptor
/// walk, the same containment, and no declaration, because `caches/` is Barge's
/// own folder rather than a consumer area.
#[derive(Debug)]
pub struct FeatureFolder {
    /// The verified volume's UUID.
    pub volume_uuid: VolumeUuid,
    /// The proved mount root.
    pub mount_point: PathBuf,
    /// The volume's recorded `root_dir`; `""` is the mount root.
    pub root_dir: String,
    /// The effective folder name for this volume.
    pub folder: String,
    /// The absolute path the configuration resolves to.
    pub path: PathBuf,
    dirs: Direction,
}

impl FeatureFolder {
    /// Whether the folder exists. Resolution creates nothing.
    pub fn exists(&self) -> bool {
        matches!(self.dirs, Direction::Existing { .. })
    }
}

/// Resolve one of Barge's own feature folders for one volume, creating nothing.
pub fn resolve_feature(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    feature: Feature,
) -> Result<FeatureFolder> {
    // An undeclared durability refuses every placement use, exactly as it
    // refuses an area use: expending data is never inferred.
    record.declared_durability()?;
    let root = VolumeRoot::open(dirs, facts, mounts, record)?;
    let effective_uid = observer.effective_uid()?;
    let device = root.identity().device;
    let base = root.mount_point().to_path_buf();
    let folder = feature_folder(record, feature)?;
    let walked = walk_folder_chain(
        dirs,
        root.into_handle(),
        &base,
        device,
        effective_uid,
        &record.root_dir,
        &folder,
    )?;
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
    Ok(FeatureFolder {
        volume_uuid: record.volume_uuid.clone(),
        mount_point: base,
        root_dir: record.root_dir.clone(),
        folder,
        path: walked.path,
        dirs: dirs_state,
    })
}

/// Duplicate the verified existing feature walk without creating anything.
pub(crate) fn open_existing_feature(
    dirs: &dyn DirOps,
    folder: &FeatureFolder,
) -> Result<Option<DirHandle>> {
    match &folder.dirs {
        Direction::Existing { handle } => dirs.duplicate_dir(handle).map(Some),
        Direction::Missing { .. } => Ok(None),
    }
}

/// Open one of Barge's own feature folders, creating the folder chain when it is
/// missing.
///
/// The creation path is the same one the area roots use: every existing
/// component must be this account's real directory on this device, and every
/// created one is opened again and compared with the identity the platform
/// returned.
pub fn open_or_create_feature(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    folder: &FeatureFolder,
) -> Result<(DirHandle, PathBuf)> {
    let effective_uid = observer.effective_uid()?;
    let handle = ensure_folder(dirs, &folder.dirs, effective_uid)?;
    Ok((handle, folder.path.clone()))
}

/// Ensure one child directory of an opened folder exists.
///
/// An entry that is already there is adopted only when it is a real directory of
/// this account on `device` and the opened directory is the one that was
/// checked; anything else is a containment refusal rather than an adoption.
pub fn ensure_child_directory(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    parent: &DirHandle,
    name: &str,
    device: DeviceId,
) -> Result<DirHandle> {
    validate_name("folder", name)?;
    let effective_uid = observer.effective_uid()?;
    match dirs.child_facts(parent, name)? {
        Some(facts) => adopt_existing_component(dirs, parent, name, &facts, device, effective_uid),
        None => match dirs.create_child_dir(parent, name, AREA_DIRECTORY_MODE) {
            Ok(identity) => {
                let opened = dirs.open_child_dir(parent, name)?;
                if opened.identity() != identity {
                    return Err(containment(
                        "folder",
                        name,
                        "the component changed while it was created",
                    ));
                }
                Ok(opened)
            }
            Err(error) => {
                let Some(facts) = dirs.child_facts(parent, name)? else {
                    return Err(error);
                };
                adopt_existing_component(dirs, parent, name, &facts, device, effective_uid)
            }
        },
    }
}

/// Create one owned root inside one of Barge's own feature folders.
///
/// The folder chain is created when it is missing, through the same adoption
/// proof the area path uses, and the root is created exclusively with a fresh
/// 128-bit id and `0700`. The marker records the area identity the caller names
/// and no effective declaration, because a feature folder has no consumer
/// policies to apply.
pub fn create_root_in_feature(
    state: &StatePaths,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    folder: &FeatureFolder,
    kind: RootKind,
    area: MarkerArea,
) -> Result<OwnedRoot> {
    create_root_in_feature_with(
        state,
        dirs,
        observer,
        folder,
        kind,
        area,
        ActivityIo::real(),
    )
}

/// [`create_root_in_feature`] with the activity record's writes as seams, so a
/// test can prove what a failed activity publication leaves behind.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_root_in_feature_with(
    state: &StatePaths,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    folder: &FeatureFolder,
    kind: RootKind,
    area: MarkerArea,
    io: ActivityIo,
) -> Result<OwnedRoot> {
    let effective_uid = observer.effective_uid()?;
    state.initialize()?;
    let folder_handle = ensure_folder(dirs, &folder.dirs, effective_uid)?;
    let mut last_error: Option<Error> = None;
    for _ in 0..ROOT_ID_ATTEMPTS {
        let root_id = hex(16, dirs.random_bytes(16)?);
        let mut locks = state::LockSet::new(state.clone());
        let root_lock = locks.lock_root(&root_id)?;
        match dirs.create_child_dir(&folder_handle, &root_id, ROOT_DIRECTORY_MODE) {
            Ok(identity) => {
                let root = dirs.open_child_dir(&folder_handle, &root_id)?;
                if root.identity() != identity {
                    return Err(Error::new(
                        ErrorKind::RefusedContainment,
                        format!("the root `{root_id}` changed while it was created"),
                    )
                    .with_detail("root_id", root_id.into()));
                }
                let path = folder.path.join(&root_id);
                let write = MarkerWrite {
                    root_id: Some(&root_id),
                    volume_uuid: &folder.volume_uuid,
                    resolved_path: &path,
                    mount_point: &folder.mount_point,
                    area: area.clone(),
                    kind,
                    origin: RootOrigin::Created,
                };
                let marker = write_marker(dirs, &root, &write, effective_uid)?;
                let owned = OwnedRoot {
                    root_id,
                    nonce: marker.nonce,
                    kind,
                    area,
                    volume_uuid: folder.volume_uuid.clone(),
                    path,
                    identity: root.identity(),
                    marker_digest: marker.marker_digest,
                    created_ms: marker.created_ms,
                };
                let identity = crate::identity::from_owned(
                    &owned,
                    &folder.mount_point,
                    crate::identity::IdentityOrigin::Created,
                )?;
                crate::identity::write_once(state, dirs, &root_lock, effective_uid, &identity)?;
                // The root's activity record starts at generation zero with no
                // release: a root that was never released is never idle, so a
                // consumer that dies holding it keeps it (stage 3 design,
                // "Activity records").
                record_activity_creation(
                    state,
                    &folder.volume_uuid,
                    &owned.root_id,
                    owned.created_ms,
                    io,
                )
                .map_err(|error| creation_unsettled(&owned, error))?;
                return Ok(owned);
            }
            Err(error) => {
                if dirs.child_facts(&folder_handle, &root_id)?.is_some() {
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

/// The policy fields two declarations for one area disagree on, in the fixed
/// order a refusal names them.
///
/// Identity is compared before this and reported on its own. These are the fields
/// that decide what an area may do: the folder it resolves to, its per-volume
/// overrides, when a caller sees it as absent, whether its roots may be pruned and
/// whether its bytes count toward the reserve. `prune` and `reserve` are the
/// fields that decide deletion authority.
fn declaration_differences(
    caller: &AreaDeclaration,
    stored: &AreaDeclaration,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if caller.folder != stored.folder {
        fields.push("folder");
    }
    if caller.overrides != stored.overrides {
        fields.push("overrides");
    }
    if caller.absent != stored.absent {
        fields.push("absent");
    }
    if caller.prune != stored.prune {
        fields.push("prune");
    }
    if caller.reserve != stored.reserve {
        fields.push("reserve");
    }
    fields
}

/// Refuse a caller declaration that is not the stored one, naming the fields.
///
/// Nothing has been opened, created or published when this is returned: the
/// stored record is the authority, so a caller can never widen an `absent`,
/// `prune` or `reserve` policy, or move a folder, by passing its own object.
fn refuse_declaration_mismatch(
    caller: &AreaDeclaration,
    stored: &AreaDocument,
    fields: &[&'static str],
) -> Error {
    Error::new(
        ErrorKind::Usage,
        format!(
            "the caller's declaration for {}/{} differs from the stored `{}` record in {}; the stored declaration governs and nothing was changed",
            caller.namespace,
            caller.area,
            crate::contract::AREA_SCHEMA,
            fields.join(", ")
        ),
    )
    .with_detail("namespace", caller.namespace.clone().into())
    .with_detail("area", caller.area.clone().into())
    .with_detail("stored_generation", stored.generation.into())
    .with_detail(
        "fields",
        fields
            .iter()
            .map(|field| (*field).into())
            .collect::<Vec<Value>>()
            .into(),
    )
}

/// Resolve one declaration for one volume, creating nothing.
///
/// The volume is verified, `root_dir` and then the effective folder are walked
/// from the verified mount root. A stored document is the authority: a caller
/// presenting a different declaration is refused here, before the volume is
/// opened and before anything is created or deleted, and the effective
/// declaration is built from the stored record. With no stored document the
/// caller's declaration is the governing one, which is the first-creation case.
pub fn resolve(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    caller: &AreaDeclaration,
    stored: Option<&AreaDocument>,
) -> Result<ResolvedArea> {
    resolve_by(
        MountLookup::Hint,
        dirs,
        facts,
        mounts,
        observer,
        record,
        caller,
        stored,
    )
}

/// [`resolve`] through the given volume lookup.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_by(
    lookup: MountLookup<'_>,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    caller: &AreaDeclaration,
    stored: Option<&AreaDocument>,
) -> Result<ResolvedArea> {
    caller.validate()?;
    let governing = if let Some(document) = stored {
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
        let declaration = document.declaration();
        let fields = declaration_differences(caller, &declaration);
        if !fields.is_empty() {
            return Err(refuse_declaration_mismatch(caller, document, &fields));
        }
        declaration
    } else {
        caller.clone()
    };
    // An undeclared durability refuses every area use, before a byte is read or
    // written: expending data is never inferred.
    record.declared_durability()?;
    let root = VolumeRoot::open_with(dirs, facts, mounts, lookup, record)?;
    let effective_uid = observer.effective_uid()?;
    let device = root.identity().device;
    let base = root.mount_point().to_path_buf();
    let root_handle = root.into_handle();

    let folder = effective_folder(record, &governing)?;
    let walked = walk_folder_chain(
        dirs,
        root_handle,
        &base,
        device,
        effective_uid,
        &record.root_dir,
        &folder,
    )?;

    let declaration = EffectiveDeclaration::new(&governing, folder);
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
    #[cfg(test)]
    crate::tape::verdict(
        "area::resolve_by",
        crate::tape::Identity::volume(Some(record.volume_uuid.as_str()), Some(&base)),
        if matches!(dirs_state, Direction::Existing { .. }) {
            "resolved;folder-exists"
        } else {
            "resolved;folder-missing"
        },
    );
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
    open_folder_by(
        MountLookup::Hint,
        dirs,
        facts,
        mounts,
        observer,
        record,
        root_dir,
        folder,
    )
}

/// [`open_folder`] through the given volume lookup.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_folder_by(
    lookup: MountLookup<'_>,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    record: &VolumeRecord,
    root_dir: &str,
    folder: &str,
) -> Result<Option<(PathBuf, DirHandle)>> {
    record.declared_durability()?;
    let volume = VolumeRoot::open_with(dirs, facts, mounts, lookup, record)?;
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

/// Open one area folder through a mount the caller located and verified itself.
///
/// [`open_folder`] finds the volume through the record's mount hint, which is an
/// observation and can be stale after a reconnect. A replay locates the live mount
/// by the recorded UUID instead - refusing a duplicated one - and then needs the
/// same containment walk from *that* mount: `root_dir`'s components and then the
/// area folder's, each a real directory of this user on the verified volume. A
/// missing component answers `None`, exactly as [`open_folder`] does; nothing is
/// created.
pub fn open_folder_on_mount(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    mount: &Path,
    device: DeviceId,
    root_dir: &str,
    folder: &str,
) -> Result<Option<(PathBuf, DirHandle)>> {
    let effective_uid = observer.effective_uid()?;
    let handle = dirs.open_path(mount)?;
    if handle.identity().device != device {
        return Err(containment(
            "mount",
            &mount.display().to_string(),
            &format!(
                "the mount root opened on device {}, not the verified {}",
                handle.identity().device,
                device
            ),
        ));
    }
    let walked = walk_folder_chain(dirs, handle, mount, device, effective_uid, root_dir, folder)?;
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
    state: &StatePaths,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    resolved: &ResolvedArea,
    spec: RootSpec,
) -> Result<OwnedRoot> {
    create_root_with(state, dirs, observer, resolved, spec, ActivityIo::real())
}

/// [`create_root`] with the activity record's writes as seams, so a test can prove
/// what a failed activity publication leaves behind.
pub(crate) fn create_root_with(
    state: &StatePaths,
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    resolved: &ResolvedArea,
    spec: RootSpec,
    io: ActivityIo,
) -> Result<OwnedRoot> {
    let effective_uid = observer.effective_uid()?;
    state.initialize()?;
    let area_handle = ensure_area_folder(dirs, resolved, effective_uid)?;
    let mut last_error: Option<Error> = None;
    for _ in 0..ROOT_ID_ATTEMPTS {
        let root_id = hex(16, dirs.random_bytes(16)?);
        let mut locks = state::LockSet::new(state.clone());
        let root_lock = locks.lock_root(&root_id)?;
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
                let owned =
                    write_root_marker(dirs, resolved, &root, &root_id, spec, effective_uid)?;
                let identity = crate::identity::from_owned(
                    &owned,
                    &resolved.mount_point,
                    crate::identity::IdentityOrigin::Created,
                )?;
                crate::identity::write_once(state, dirs, &root_lock, effective_uid, &identity)?;
                // The root's activity record starts at generation zero with no
                // release: a root that was never released is never idle, so a
                // consumer that dies holding it keeps it (stage 3 design,
                // "Activity records").
                record_activity_creation(
                    state,
                    &resolved.volume_uuid,
                    &root_id,
                    owned.created_ms,
                    io,
                )
                .map_err(|error| creation_unsettled(&owned, error))?;
                return Ok(owned);
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
/// the volume must be registered and its durability declared, the state
/// directory's record for the area is read here (a caller cannot present or
/// withhold it), and the area is resolved through the descriptor walk. A caller
/// declaration that differs from that record is refused, naming the fields. No
/// folder is created here.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
) -> Result<VolumeArea> {
    prepare_by(
        MountLookup::Hint,
        state,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        declaration,
    )
}

/// [`prepare`] through the given volume lookup. Only the release path's deletion uses
/// `ByUuid`; the public `prepare` keeps the stored-point lookup and its signature.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_by(
    lookup: MountLookup<'_>,
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
) -> Result<VolumeArea> {
    let record = registered_volume(state, volume_uuid)?;
    let stored = stored_declaration(state, declaration)?;
    prepare_with(
        &record,
        dirs,
        facts,
        mounts,
        observer,
        lookup,
        declaration,
        stored.as_ref(),
    )
}

/// Resolve one caller declaration against a store snapshot the caller did not
/// supply.
#[allow(clippy::too_many_arguments)]
fn prepare_with(
    record: &VolumeRecord,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    lookup: MountLookup<'_>,
    declaration: &AreaDeclaration,
    stored: Option<&AreaDocument>,
) -> Result<VolumeArea> {
    let resolved = resolve_by(
        lookup,
        dirs,
        facts,
        mounts,
        observer,
        record,
        declaration,
        stored,
    )?;
    Ok(VolumeArea {
        record: record.clone(),
        resolved,
    })
}

/// The state directory's record for the caller's area, when the area is
/// declared.
///
/// The store is read here, never taken from the caller: a library caller cannot
/// withhold, replace or override the declaration that governs its call.
fn stored_declaration(
    state: &StatePaths,
    declaration: &AreaDeclaration,
) -> Result<Option<AreaDocument>> {
    Ok(read_declarations(state)?
        .find(&declaration.namespace, &declaration.area)
        .cloned())
}

/// Where a root creation's declaration authority comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclarationSource {
    /// A consumer declaration: the state directory's stored record governs, and a
    /// first creation publishes the caller's declaration.
    Store,
    /// A declaration Barge derives itself for each call (scratch): it is neither
    /// a consumer declaration nor an area document, so the store neither governs
    /// nor records it.
    Derived,
}

/// Create one owned root inside a declared area on a registered volume.
///
/// The caller's declaration is first resolved against the state directory's
/// record: a stored record governs and must match, and an area with no record
/// receives the caller's declaration once ([`ensure_declared`]), so the first
/// creation stores what the caller presented and a later call can only match it.
/// The volume lock is then held across the resolution and the creation, so two
/// Barge commands cannot create folder structure on one volume at the same time.
/// The registry lock is not held across the walk. Nothing is created when the
/// volume is absent or mismatched, the durability is undeclared, the registry is
/// invalid, the declaration does not contain its folder or the caller's
/// declaration disagrees with the stored one.
#[allow(clippy::too_many_arguments)]
pub fn create_root_on_volume(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    spec: RootSpec,
) -> Result<(VolumeArea, OwnedRoot)> {
    create_root_on_volume_with(
        state,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        declaration,
        spec,
        DeclarationSource::Store,
    )
}

/// Create one owned root under a declaration Barge derives for each call.
///
/// This is Barge's own scratch allocation: `barge/scratch`'s folder is derived
/// from the volume's `folders.scratch` and the project name, so it is never a
/// stored consumer declaration and the store neither governs nor records it
/// (`scratch::scratch_declaration`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_root_on_volume_derived(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    spec: RootSpec,
) -> Result<(VolumeArea, OwnedRoot)> {
    create_root_on_volume_with(
        state,
        dirs,
        facts,
        mounts,
        observer,
        volume_uuid,
        declaration,
        spec,
        DeclarationSource::Derived,
    )
}

#[allow(clippy::too_many_arguments)]
fn create_root_on_volume_with(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
    spec: RootSpec,
    source: DeclarationSource,
) -> Result<(VolumeArea, OwnedRoot)> {
    // Every refusal the volume itself can cause happens before any effect: an
    // unregistered UUID, an undeclared durability, an absent volume and a
    // different volume at the recorded mount are all proved here, so a refused
    // creation leaves no state layout, no lock file and no folder behind.
    let record = registered_volume(state, volume_uuid)?;
    record.declared_durability()?;
    VolumeRoot::open(dirs, facts, mounts, &record)?;
    // A consumer declaration is stored first, so the record the creation is
    // authorized by exists before any folder does. `ensure_declared` refuses a
    // declaration that disagrees with an existing record without writing
    // anything, and it publishes the caller's declaration the first time.
    let stored = match source {
        DeclarationSource::Store => Some(ensure_declared(state, declaration, state::now_ms())?),
        DeclarationSource::Derived => None,
    };
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let _volume_lock = locks.lock_volume(volume_uuid)?;
    let volume = prepare_with(
        &record,
        dirs,
        facts,
        mounts,
        observer,
        MountLookup::Hint,
        declaration,
        stored.as_ref(),
    )?;
    let root = create_root(state, dirs, observer, &volume.resolved, spec)?;
    Ok((volume, root))
}

/// List one declared area's roots on a registered volume, mutating nothing.
///
/// The state directory's record governs, exactly as it does for creation: a
/// caller declaration that differs from it is refused, naming the fields. A
/// listing creates and stores nothing.
#[allow(clippy::too_many_arguments)]
pub fn list_roots_on_volume(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    volume_uuid: &VolumeUuid,
    declaration: &AreaDeclaration,
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
    ensure_folder(dirs, &resolved.dirs, effective_uid)
}

/// Create any missing folder component, refusing to adopt anything that is
/// already there but is not this account's directory.
fn ensure_folder(
    dirs: &dyn DirOps,
    dirs_state: &Direction,
    effective_uid: u32,
) -> Result<DirHandle> {
    match dirs_state {
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

#[derive(Debug, Clone)]
pub struct MarkerWrite<'a> {
    /// The root id, when the directory already has one. Creation names the
    /// directory before its marker exists, so the two cannot disagree; `None`
    /// means this write chooses a fresh id.
    pub root_id: Option<&'a str>,
    /// The verified volume's UUID.
    pub volume_uuid: &'a VolumeUuid,
    /// The absolute path the descriptor walk actually reached for the root.
    pub resolved_path: &'a Path,
    /// The verified mount containing the root.
    pub mount_point: &'a Path,
    /// The area identity the root belongs to.
    pub area: MarkerArea,
    /// What the root is for.
    pub kind: RootKind,
    /// Whether Barge created the directory or adopted it in place.
    pub origin: RootOrigin,
}

/// The identity one marker write established, as its caller records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkerIdentity {
    /// The fresh 128-bit root id the marker records.
    pub root_id: String,
    /// The fresh nonce the marker records.
    pub nonce: String,
    /// The SHA-256 of the exact marker bytes written.
    pub marker_digest: String,
    /// When the marker was written.
    pub created_ms: u64,
}

/// Write one `barge.root/2` marker into an already opened root directory.
///
/// This is the one marker writer: creation and in-place adoption both use it, so
/// the field set, the caps, the mode and the read-back proof cannot drift apart.
/// The marker is validated against what was actually walked before it is
/// published through a private synced file and an exclusive rename. A raced-in
/// entry is kept, with the staging file available as recovery evidence. Readback
/// proves the published inode, nonce and digest rather than just the root id.
pub fn write_marker(
    dirs: &dyn DirOps,
    root: &DirHandle,
    write: &MarkerWrite<'_>,
    effective_uid: u32,
) -> Result<MarkerIdentity> {
    let root_id = match write.root_id {
        Some(root_id) => {
            state::validate_identifier("root_id", root_id)?;
            root_id.to_owned()
        }
        None => hex(16, dirs.random_bytes(16)?),
    };
    let nonce = hex(16, dirs.random_bytes(16)?);
    let created_ms = state::now_ms();
    let relative = write
        .resolved_path
        .strip_prefix(write.mount_point)
        .map_err(|_| unknown_marker("root path is outside the verified mount".to_owned()))?
        .to_str()
        .ok_or_else(|| unknown_marker("root path is not UTF-8".to_owned()))?;
    folder_components("mount_relative_path", relative)?;
    let marker = RootMarker {
        schema: crate::contract::ROOT_SCHEMA.to_owned(),
        root_id: root_id.clone(),
        nonce: nonce.clone(),
        volume_uuid: write.volume_uuid.clone(),
        owner: "barge".to_owned(),
        kind: write.kind,
        area: write.area.clone(),
        resolved_path: None,
        mount_relative_path: Some(relative.to_owned()),
        mount_at_record: Some(write.mount_point.to_path_buf()),
        created_ms,
        generation: 1,
        origin: write.origin,
        conclusion: None,
    };
    let expectation = MarkerExpectation {
        volume_uuid: write.volume_uuid,
        resolved_path: write.resolved_path,
        mount_point: Some(write.mount_point),
        effective_uid,
        now_ms: created_ms,
    };
    marker.validate(&expectation)?;
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
    // The intended root id is already durable in an in-place adoption's intent.
    // Keep this private staging name on uncertainty; never replace the marker
    // that another writer may have created since the caller observed absence.
    let staged = format!(".barge-marker-{root_id}");
    if dirs.child_facts(root, &staged)?.is_some() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "marker staging name is already occupied",
        ));
    }
    dirs.publish_child_file(root, &staged, &bytes, RECORD_MODE)?;
    let staged_facts = dirs
        .child_facts(root, &staged)?
        .ok_or_else(|| Error::new(ErrorKind::UnknownState, "staged marker disappeared"))?;
    if staged_facts.kind != EntryKind::File
        || staged_facts.uid != effective_uid
        || staged_facts.device != root.identity().device
        || dirs.recheck_dir(root)? != root.identity()
    {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "staged marker or parent binding changed",
        ));
    }
    if dirs.rename_child(root, &staged, ROOT_MARKER_FILE)? == os::RenameOutcome::TargetExists {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            "a marker appeared before publication; no existing entry was replaced",
        )
        .with_detail(
            "path",
            write
                .resolved_path
                .join(ROOT_MARKER_FILE)
                .display()
                .to_string()
                .into(),
        )
        .with_detail(
            "staged_marker",
            write
                .resolved_path
                .join(&staged)
                .display()
                .to_string()
                .into(),
        ));
    }
    dirs.sync_dir(root)?;
    let published = dirs.child_facts(root, ROOT_MARKER_FILE)?;
    if published.as_ref() != Some(&staged_facts) {
        return Err(Error::new(
            ErrorKind::UnknownState,
            "published marker identity changed",
        ));
    }
    match read_marker_with_digest(dirs, root, &expectation) {
        (MarkerState::Owned(back), Some(back_digest))
            if back.root_id == root_id && back.nonce == nonce && back_digest == digest => {}
        (other, _) => {
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
    Ok(MarkerIdentity {
        root_id,
        nonce,
        marker_digest: digest,
        created_ms,
    })
}

fn write_root_marker(
    dirs: &dyn DirOps,
    resolved: &ResolvedArea,
    root: &DirHandle,
    root_id: &str,
    spec: RootSpec,
    effective_uid: u32,
) -> Result<OwnedRoot> {
    let path = resolved.path.join(root_id);
    let write = MarkerWrite {
        root_id: Some(root_id),
        volume_uuid: &resolved.volume_uuid,
        resolved_path: &path,
        mount_point: &resolved.mount_point,
        area: MarkerArea {
            namespace: resolved.declaration.namespace.clone(),
            area: resolved.declaration.area.clone(),
            effective: Some(resolved.declaration.clone()),
        },
        kind: spec.kind,
        origin: RootOrigin::Created,
    };
    let identity = write_marker(dirs, root, &write, effective_uid)?;
    Ok(OwnedRoot {
        root_id: identity.root_id,
        nonce: identity.nonce,
        kind: spec.kind,
        area: write.area,
        volume_uuid: resolved.volume_uuid.clone(),
        path,
        identity: root.identity(),
        marker_digest: identity.marker_digest,
        created_ms: identity.created_ms,
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
/// cap, and must parse with exactly the fields its version defines. It is
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
    read_marker_inner(dirs, root, expectation, None)
}

/// Read a v1 marker after locating and verifying the recorded volume's live mount.
pub fn read_remounted_marker(
    dirs: &dyn DirOps,
    root: &DirHandle,
    expectation: &MarkerExpectation<'_>,
    verified: &os::LocatedVolume,
    recorded_mount: &Path,
) -> (MarkerState, Option<String>) {
    if root.identity().device != verified.identity().device {
        return (
            MarkerState::Unknown {
                reason: "root is not on the located volume".to_owned(),
            },
            None,
        );
    }
    read_marker_inner(dirs, root, expectation, Some((verified, recorded_mount)))
}

/// What reading one root's marker bytes found, before any verdict about the path
/// the directory sits at.
enum MarkerRead {
    /// No marker file at all.
    Absent,
    /// The bytes, already proved to be a private regular file of this user inside
    /// its cap and unchanged by the read.
    Bytes(Vec<u8>),
    /// A marker that is there and could not be read: a foreign owner, a link or
    /// another object, a private-mode failure, an oversized file, or a read that
    /// failed or found the marker gone.
    Unreadable(String),
}

/// Read the bounded bytes of one marker file, with the file's own facts checked.
fn read_marker_bytes(dirs: &dyn DirOps, root: &DirHandle, effective_uid: u32) -> MarkerRead {
    let facts = match dirs.child_facts(root, ROOT_MARKER_FILE) {
        Ok(Some(facts)) => facts,
        Ok(None) => return MarkerRead::Absent,
        Err(error) => return MarkerRead::Unreadable(error.to_string()),
    };
    if facts.kind != EntryKind::File {
        return MarkerRead::Unreadable(format!(
            "the marker is a {}, and Barge only owns a regular file",
            facts.kind.as_str()
        ));
    }
    if facts.uid != effective_uid {
        return MarkerRead::Unreadable(format!(
            "the marker is owned by uid {}, not this account's {effective_uid}",
            facts.uid
        ));
    }
    if !facts.is_private() {
        return MarkerRead::Unreadable(format!(
            "the marker's mode {:o} is not private",
            facts.mode
        ));
    }
    if facts.size > MARKER_CAP_BYTES as u64 {
        return MarkerRead::Unreadable(format!(
            "the marker is {} bytes, over the {MARKER_CAP_BYTES}-byte cap",
            facts.size
        ));
    }
    match dirs.read_child_file(root, ROOT_MARKER_FILE, MARKER_CAP_BYTES) {
        Ok(Some(bytes)) => MarkerRead::Bytes(bytes),
        Ok(None) => MarkerRead::Unreadable("the marker disappeared while it was read".to_owned()),
        Err(error) => MarkerRead::Unreadable(error.to_string()),
    }
}

/// A marker document as its own bytes state it, with no verdict about where it
/// was found.
///
/// Custody and replay need a captured object's marker identity after it has been
/// renamed under its private capture name, where the recorded path can never
/// match; they compare the root id, nonce, volume and the SHA-256 themselves. This
/// reader makes no ownership claim: an absent marker, a failed read and an
/// unparseable document are each a distinct answer, and the caller's own refusal
/// is what decides.
#[derive(Debug)]
pub enum MarkerDocument {
    /// No marker file, which is a positive observation.
    Absent,
    /// The document and the SHA-256 of the exact bytes read.
    Read {
        /// The parsed document, unvalidated against any path.
        marker: Box<RootMarker>,
        /// The SHA-256 of the bytes it was parsed from.
        digest: String,
    },
    /// A marker that is there and could not be read.
    Unreadable {
        /// Why, for a human.
        reason: String,
    },
}

/// Read one marker's own document without deciding whether its path is the one it
/// records.
pub fn read_marker_document(
    dirs: &dyn DirOps,
    root: &DirHandle,
    effective_uid: u32,
) -> MarkerDocument {
    match read_marker_bytes(dirs, root, effective_uid) {
        MarkerRead::Absent => MarkerDocument::Absent,
        MarkerRead::Unreadable(reason) => MarkerDocument::Unreadable { reason },
        MarkerRead::Bytes(bytes) => {
            let digest = sha256_hex(&bytes);
            match serde_json::from_slice::<RootMarker>(&bytes) {
                Ok(marker) => MarkerDocument::Read {
                    marker: Box::new(marker),
                    digest,
                },
                Err(error) => MarkerDocument::Unreadable {
                    reason: format!("the marker is not a root marker document: {error}"),
                },
            }
        }
    }
}

fn read_marker_inner(
    dirs: &dyn DirOps,
    root: &DirHandle,
    expectation: &MarkerExpectation<'_>,
    remounted: Option<(&os::LocatedVolume, &Path)>,
) -> (MarkerState, Option<String>) {
    let unknown = |reason: String| (MarkerState::Unknown { reason }, None);
    let bytes = match read_marker_bytes(dirs, root, expectation.effective_uid) {
        MarkerRead::Absent => return (MarkerState::Absent, None),
        MarkerRead::Unreadable(reason) => return unknown(reason),
        MarkerRead::Bytes(bytes) => bytes,
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
    // Serde's optional fields accept explicit null as well as absence. The two
    // versions have disjoint path fields, so reject even null cross-version keys.
    let shape: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(shape) => shape,
        Err(error) => return unknown(format!("marker shape is unreadable: {error}")),
    };
    let cross_version_field = match marker.schema.as_str() {
        "barge.root/1" => {
            shape.get("mount_relative_path").is_some() || shape.get("mount_at_record").is_some()
        }
        "barge.root/2" => shape.get("resolved_path").is_some(),
        _ => false,
    };
    if cross_version_field {
        return unknown("marker carries path fields from another version".to_owned());
    }
    // The bytes were read, so the digest is a fact even when the marker does not
    // validate where it was found: custody and replay carry the expected digest
    // themselves, and a quarantined leaf's marker necessarily fails the path
    // check after the rename.
    let verdict = match remounted {
        Some((verified, recorded_mount)) => {
            marker.validate_remounted(verified, recorded_mount, expectation)
        }
        None => marker.validate(expectation),
    };
    match verdict {
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
    /// Validate a v1 marker after a verified UUID has moved to a new mount.
    /// The caller supplies the recorded mount and the path reached by its walk.
    pub fn validate_remounted(
        &self,
        verified: &os::LocatedVolume,
        recorded_mount: &Path,
        expectation: &MarkerExpectation<'_>,
    ) -> Result<()> {
        // A v1 marker must carry its absolute path. A missing path is unknown,
        // never defaulted, so the prefix rule has nothing to compare.
        let Some(recorded_path) = self.resolved_path.as_deref() else {
            return Err(unknown_marker(
                "the v1 marker records no resolved path".to_owned(),
            ));
        };
        let canonical = |path: &Path| {
            path.to_str().is_some_and(|value| {
                value.starts_with('/')
                    && value
                        .split('/')
                        .skip(1)
                        .all(|part| !part.is_empty() && part != "." && part != "..")
            })
        };
        if !os::recorded_mount_path(recorded_mount)
            || !canonical(recorded_mount)
            || !canonical(recorded_path)
            || !canonical(expectation.resolved_path)
            || verified.identity().volume_uuid.as_ref() != Some(expectation.volume_uuid)
            || verified.path() != verified.identity().mount_point
        {
            return Err(unknown_marker(
                "the live mount does not verify the recorded UUID".to_owned(),
            ));
        }
        let old = recorded_path
            .strip_prefix(recorded_mount)
            .map_err(|_| unknown_marker("marker is outside the recorded mount".to_owned()))?;
        let live = expectation
            .resolved_path
            .strip_prefix(verified.path())
            .map_err(|_| unknown_marker("walk is outside the verified mount".to_owned()))?;
        if old.as_os_str().is_empty()
            || old != live
            || old
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(unknown_marker(
                "the path below the mount changed".to_owned(),
            ));
        }
        let original = MarkerExpectation {
            resolved_path: recorded_path,
            ..*expectation
        };
        self.validate(&original)
    }

    /// Validate every field of a marker against what was actually walked.
    fn validate(&self, expectation: &MarkerExpectation<'_>) -> Result<()> {
        if self.schema != crate::contract::ROOT_SCHEMA && self.schema != "barge.root/1" {
            return Err(unknown_marker(format!(
                "`{}` is not the `{}` major this build reads",
                self.schema,
                crate::contract::ROOT_SCHEMA
            )));
        }
        state::validate_identifier("root_id", &self.root_id)?;
        state::validate_identifier("nonce", &self.nonce)?;
        if self.root_id.bytes().any(|byte| byte.is_ascii_uppercase())
            || self.nonce.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err(unknown_marker(
                "marker identifiers must be lowercase".to_owned(),
            ));
        }
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
        match self.schema.as_str() {
            "barge.root/1" => {
                if self.mount_relative_path.is_some()
                    || self.mount_at_record.is_some()
                    || self.resolved_path.as_deref() != Some(expectation.resolved_path)
                {
                    return Err(unknown_marker(
                        "v1 marker path differs from the walk".to_owned(),
                    ));
                }
            }
            "barge.root/2" => {
                let relative = self
                    .mount_relative_path
                    .as_deref()
                    .ok_or_else(|| unknown_marker("v2 marker lacks relative path".to_owned()))?;
                folder_components("mount_relative_path", relative)?;
                let mount = self
                    .mount_at_record
                    .as_ref()
                    .ok_or_else(|| unknown_marker("v2 marker lacks recorded mount".to_owned()))?;
                if !mount.is_absolute()
                    || self.resolved_path.is_some()
                    || expectation
                        .mount_point
                        .and_then(|mount| expectation.resolved_path.strip_prefix(mount).ok())
                        != Some(Path::new(relative))
                {
                    return Err(unknown_marker(
                        "v2 marker path differs from the walk".to_owned(),
                    ));
                }
            }
            _ => return Err(unknown_marker("unknown marker version".to_owned())),
        }
        if self.created_ms > expectation.now_ms {
            return Err(unknown_marker(format!(
                "the marker is dated {} ms in the future",
                self.created_ms - expectation.now_ms
            )));
        }
        if !matches!(
            (self.kind, self.generation, self.conclusion.is_some()),
            (_, 1, false) | (RootKind::ScratchRun, 2, true)
        ) {
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
pub(crate) fn read_directory(path: &Path) -> Result<Option<Vec<String>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        #[cfg(not(test))]
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        #[cfg(test)]
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::tape::read_absent("area::read_directory", path);
            return Ok(None);
        }
        Err(error) => {
            #[cfg(test)]
            crate::tape::stat_refused(
                "area::read_directory",
                path,
                &crate::tape::errno_class(&error),
            );
            #[cfg(test)]
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into()));
            #[cfg(not(test))]
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        #[cfg(test)]
        crate::tape::read_refused("area::read_directory", path, "not_a_real_directory");
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("{} is not a real directory", path.display()),
        )
        .with_detail("path", path.display().to_string().into()));
    }
    let mut names = Vec::new();
    let entries = std::fs::read_dir(path).map_err(|error| {
        #[cfg(test)]
        crate::tape::listing_refused(
            "area::read_directory",
            path,
            &crate::tape::errno_class(&error),
        );
        Error::new(
            ErrorKind::Io,
            format!("cannot list {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            #[cfg(test)]
            crate::tape::listing_refused(
                "area::read_directory",
                path,
                &crate::tape::errno_class(&error),
            );
            Error::new(
                ErrorKind::Io,
                format!("cannot list {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })?;
        #[cfg(test)]
        crate::tape::dir_entry("area::read_directory", dir_facts(path), &entry.file_name());
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    #[cfg(test)]
    crate::tape::listing(
        "area::read_directory",
        path,
        names.len(),
        names.len(),
        false,
    );
    Ok(Some(names))
}

/// The directory's own device and inode, as the cfg(test) tape binds an
/// enumeration's parent identity. A directory that cannot be stated binds no
/// parent, which never reconciles against a manifest entry.
#[cfg(test)]
fn dir_facts(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some((metadata.dev(), metadata.ino())),
        Err(_) => None,
    }
}

/// Why one declaration write happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclarationWrite {
    /// `barge area declare`: the caller's declaration is published, and a
    /// re-declaration of the same area bumps the generation.
    Declare,
    /// The store's first creation: an existing record governs and must match,
    /// and an area with no record receives the caller's declaration once, never
    /// a replacement.
    FirstWins,
}

/// Declare one area, refusing a collision before anything is written.
///
/// The declaration is validated, the registry and every stored declaration are
/// read under the exclusive registry lock, the effective folders of every
/// declaration are compared per registered volume against each other and
/// against Barge's own `scratch/` and `caches/` folders, and only then is the
/// document published atomically. Two owners of one folder is not a state Barge
/// writes. A re-declaration of the same area replaces its record and bumps the
/// generation; the library's [`ensure_declared`] is the first-wins form.
pub fn declare(state: &StatePaths, caller: &AreaDeclaration, now_ms: u64) -> Result<AreaDocument> {
    write_declaration(state, caller, now_ms, DeclarationWrite::Declare)
}

/// Store the caller's declaration the first time, and match it afterwards.
///
/// This is the library form of `barge area declare`. A `barge.area/1` record is
/// the authority once it exists, so a caller that presents a different
/// declaration is refused before anything is written or created, naming the
/// fields that differ; one that presents the same declaration receives the
/// stored record. An area with no record receives the caller's declaration under
/// the same publication rules as `declare` (validation, collision check under the
/// registry lock and one atomic publication) with generation 1, and a first
/// creation never replaces an existing record.
pub fn ensure_declared(
    state: &StatePaths,
    caller: &AreaDeclaration,
    now_ms: u64,
) -> Result<AreaDocument> {
    write_declaration(state, caller, now_ms, DeclarationWrite::FirstWins)
}

fn write_declaration(
    state: &StatePaths,
    caller: &AreaDeclaration,
    now_ms: u64,
    write: DeclarationWrite,
) -> Result<AreaDocument> {
    caller.validate()?;
    // A first creation decides a stored record before it creates anything, so a
    // caller that disagrees with the store leaves no state layout behind.
    if write == DeclarationWrite::FirstWins {
        if let Some(document) = match_stored(caller, &read_declarations(state)?)? {
            return Ok(document);
        }
    }
    // Refuse an invalid registry before any effect, including creating the
    // state layout. It is then re-read under the lock, so the collision check
    // sees the registry the write is authorized by.
    registry::read(state)?;
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let lock = locks.lock_registry()?;
    let registry = registry::read(state)?;
    let stored = read_declarations(state)?;
    if write == DeclarationWrite::FirstWins {
        // The store is re-read under the lock: a competing first creation either
        // already published the same declaration or is refused here.
        if let Some(document) = match_stored(caller, &stored)? {
            return Ok(document);
        }
    }
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

/// The stored document for the caller's area when the store already rules it.
///
/// `Ok(None)` means the area has no record, which is the only case a first
/// creation may publish into.
fn match_stored(caller: &AreaDeclaration, stored: &DeclarationSet) -> Result<Option<AreaDocument>> {
    let Some(document) = stored.find(&caller.namespace, &caller.area) else {
        return Ok(None);
    };
    let fields = declaration_differences(caller, &document.declaration());
    if !fields.is_empty() {
        return Err(refuse_declaration_mismatch(caller, document, &fields));
    }
    Ok(Some(document.clone()))
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

/// One completed release: where the hold was, whether this call removed one, and
/// the root's activity record as it stands afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The hold file's path, whether or not it was there.
    pub path: PathBuf,
    /// Whether this call removed a hold file.
    pub released: bool,
    /// The root's activity record as it reads after the call.
    pub activity: ActivityReading,
}

/// Release one holder's hold. A hold that is already absent is already released.
///
/// One invariant decides the order, because it is what idleness can be concluded
/// from: **an absent hold file implies a recorded release**. So the activity
/// record is written first and the hold file is removed second, both under the
/// root lock the hold was written under. A crash or a failure between the two can
/// therefore only leave a hold that is *still present*, which the idleness matrix
/// already gates on — never an absent hold beside an older record that a reader
/// would trust as current, which would make an unsettled root look idle and order
/// it on a stale release time.
///
/// A release that fails records nothing. The hold, the previous record and the
/// previous record's exact bytes are read before any effect; a failed activity
/// write refuses with the hold intact, and a failed hold removal **restores the
/// previous record byte for byte** before the failure is reported. If that
/// restore itself fails, the failure is `unknown_state` naming the record path and
/// the hold, and the hold is still there, so nothing can be concluded idle from
/// the record alone. The residual window is the crash the ordering allows: the
/// record has the release, the hold file remains, and the matrix keeps the root on
/// the hold until an operator's release or [`idle_baseline`] settles it.
///
/// A root with no activity record keeps none (design, O-013), never an inferred
/// creation time or volume.
pub fn release(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    root_id: &str,
    holder: &str,
) -> Result<Release> {
    release_with(state, observer, root_id, holder, ActivityIo::real())
}

/// Remove one account-state record, classified as `io` when the platform refuses.
fn remove_state_file(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("cannot remove {}: {error}", path.display()),
        )
        .with_detail("path", path.display().to_string().into())
    })
}

/// Make one account-state record invalid in place, so it can never be read as an
/// authoritative fact.
///
/// This is the last recovery step for a record whose publication was not confirmed
/// durable. It truncates the record rather than renaming or unlinking it, so it
/// needs only the file's own write permission and not the directory's: it is
/// available exactly where a restore and a removal are not. A truncated record is
/// an invalid record, which the design already defines as `activity_unknown` and
/// never idle (O-013), and a crash part-way through leaves an invalid record too,
/// never a valid one.
fn invalidate_state_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map(|_| ())
        .map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("cannot invalidate {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })
}

/// [`release`] with its two record writes as seams — the fact's publish and the
/// restore — so a test can prove a publish that fails after its rename, or a
/// restore that fails, is reported rather than silently accepted (the same seam
/// shape as [`state::publish_atomic_with`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn release_with(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    root_id: &str,
    holder: &str,
    io: ActivityIo,
) -> Result<Release> {
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
        ReadOutcome::Absent => {
            return Ok(Release {
                path,
                released: false,
                activity: read_activity(state, observer, root_id)?,
            })
        }
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
    let activity_path = state.activity_path(root_id)?;
    // Everything the release needs is read before any effect, under the root
    // lock: the previous record, its exact bytes, and the record's own validity.
    let previous = read_activity(state, observer, root_id)?;
    let previous_bytes = activity_record_bytes(state, root_id)?;
    // The fact comes first: an absent hold must always imply a recorded release.
    // A root with no record, or with one that does not verify, gets no new record:
    // a release is a release fact, not a licence to invent a creation time or a
    // volume (design, O-013).
    let had_record = previous.record.is_some();
    let activity = match previous.record.clone() {
        None => previous,
        Some(record) => {
            let next = released_record(&record);
            publish_fact(
                state,
                &next,
                previous_bytes.as_deref(),
                io,
                &format!("the release of `{holder}` on root `{root_id}`"),
            )
            .map_err(|error| error.with_detail("hold_path", path.display().to_string().into()))?;
            ActivityReading {
                state: ActivityStateName::Released,
                record: Some(next),
                reason: None,
            }
        }
    };
    if let Err(remove_error) = std::fs::remove_file(&path) {
        // The release did not complete, so it records nothing: put the previous
        // record's exact bytes back before reporting the failure.
        // Fail closed: a record that was written but whose previous bytes were
        // never read cannot be restored, and that is reported rather than
        // guessed away.
        let restore_error = match (&had_record, &previous_bytes) {
            (false, _) => None,
            (true, Some(bytes)) => (io.restore)(&activity_path, bytes).err(),
            (true, None) => Some(Error::new(
                ErrorKind::UnknownState,
                "the previous record's bytes were not read, so they could not be restored",
            )),
        };
        let hold_path = path.display().to_string();
        let activity_path = activity_path.display().to_string();
        return Err(match restore_error {
            None => Error::new(
                ErrorKind::Io,
                format!(
                    "cannot remove {hold_path}: {remove_error}; nothing was recorded, so the hold still stands"
                ),
            )
            .with_detail("hold_path", hold_path.into())
            .with_detail("activity_path", activity_path.into())
            .with_detail("restored", true.into()),
            Some(restore_error) => Error::new(
                ErrorKind::UnknownState,
                format!(
                    "cannot remove {hold_path}: {remove_error}; the release fact was written but the previous record could not be restored ({restore_error}). The hold is still there, so the root is kept on the hold and not on the record, and its next release or `barge area idle-baseline` settles it"
                ),
            )
            .with_detail("hold_path", hold_path.into())
            .with_detail("activity_path", activity_path.into())
            .with_detail("restored", false.into())
            .with_detail("restore_error", restore_error.to_string().into()),
        });
    }
    if let Some(directory) = path.parent() {
        // Best effort: the removal is what matters, and a directory sync the
        // platform refuses must not turn a completed release into a failure.
        let _ = std::fs::File::open(directory).and_then(|handle| handle.sync_all());
    }
    Ok(Release {
        path,
        released: true,
        activity,
    })
}

/// The exact bytes of a root's activity record, bounded, when one exists.
///
/// A release keeps them so an effect that fails can restore the record byte for
/// byte rather than leaving a release recorded that never completed.
fn activity_record_bytes(state: &StatePaths, root_id: &str) -> Result<Option<Vec<u8>>> {
    let path = state.activity_path(root_id)?;
    match state::read_bounded(
        &path,
        &Bounds::new(
            crate::contract::ACTIVITY_CAP_BYTES,
            1,
            Deadline::after(READ_DEADLINE),
        ),
    ) {
        ReadOutcome::Present(bytes) => Ok(Some(bytes)),
        ReadOutcome::Absent => Ok(None),
        ReadOutcome::Refused(error) => {
            Err(error.with_detail("path", path.display().to_string().into()))
        }
    }
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
    read_hold_directory(state, observer, root_id)
}

/// Read and decide the holds recorded for one deletion unit's name.
///
/// It is [`read_holds`] for a name that is an ordinary area name rather than a
/// Barge root id: S4's units include delegated and claimed names Barge never
/// allocated, and a hold that is somehow recorded under one is decided, never
/// skipped. A name that is not an area name at all cannot be a hold's directory
/// name (a hold is written only by [`hold`], which refuses anything else), so the
/// caller decides that case itself.
pub fn read_unit_holds(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    unit_name: &str,
) -> Result<HoldReading> {
    validate_name("unit_name", unit_name)?;
    read_hold_directory(state, observer, unit_name)
}

/// Read one hold directory, keyed by the name the unit is recorded under.
fn read_hold_directory(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    root_id: &str,
) -> Result<HoldReading> {
    let directory = state.holds_dir().join(root_id);
    let Some(names) = read_directory(&directory)? else {
        #[cfg(test)]
        crate::tape::read_absent("area::read_holds", &directory);
        return Ok(HoldReading::empty());
    };
    let effective_uid = observer.effective_uid()?;
    #[cfg(test)]
    crate::tape::scalar(
        "area::read_holds",
        "uid",
        crate::tape::Identity::none(),
        &effective_uid.to_string(),
    );
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
            #[cfg(test)]
            crate::tape::stat_refused("area::read_holds", &path, &crate::tape::errno_class(&error));
            Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into())
        })?;
        #[cfg(test)]
        crate::tape::stat(
            "area::read_holds",
            &path,
            Some((metadata.dev(), metadata.ino(), "hold-file")),
        );
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

// ---- Activity records, the idleness matrix and the LRU order (slice T3) ----
//
// Holds are released by deleting the hold file, so the release itself leaves no
// trace. The activity record is that trace: one small authoritative file per
// root in account state, written when Barge creates the root and updated on
// every release. Idleness under the space policy is decided from this record, the
// root's holds, its marker and its area's `reserve` participation, and nothing
// else: filesystem mtimes, entry counts and a consumer's own ledgers are not
// inputs (stage 3 design, "Idleness, activity and least-recently-used order").

/// One `barge.activity/1` record: the durable, positive fact that a root was
/// released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityRecord {
    /// Always `barge.activity/1`.
    pub schema: String,
    /// The root this record is about.
    pub root_id: String,
    /// The verified volume the root is on, as canonical UUID text.
    pub volume_uuid: String,
    /// When the root was last recorded as released, which an operator's
    /// [`idle_baseline`] also records. `0` means no release or baseline has been
    /// recorded, and such a root is never idle.
    pub last_release_ms: u64,
    /// When the record's root was created. `0` means the creation time was never
    /// recorded, which only a record first written by a release can say.
    pub first_created_ms: u64,
    /// How many releases the record has recorded. An idle baseline records the
    /// release time without claiming a release happened, so this count is a
    /// separate fact from `last_release_ms`.
    pub releases: u64,
    /// Increases on every write after the creation record, which is generation
    /// zero.
    pub generation: u64,
    /// When this record itself was written.
    pub recorded_ms: u64,
}

impl ActivityRecord {
    /// Validate the record's own identity and internal consistency.
    ///
    /// A record that names another root, another format, a non-canonical volume
    /// uuid, a release count that disagrees with its timestamp or a creation time
    /// ahead of its own write is unknown: it is never repaired and never
    /// defaulted.
    pub fn validate(&self) -> Result<()> {
        let invalid = |reason: &str| {
            Error::new(
                ErrorKind::UnknownState,
                format!("the activity record is not usable: {reason}"),
            )
        };
        if self.schema != crate::contract::ACTIVITY_SCHEMA {
            return Err(invalid("it is not a barge.activity/1 record"));
        }
        if !state::is_identifier(&self.root_id) {
            return Err(invalid("its root id is not 32 hex digits"));
        }
        let uuid = VolumeUuid::parse(&self.volume_uuid)
            .map_err(|_| invalid("its volume uuid is not a uuid"))?;
        if uuid.as_str() != self.volume_uuid {
            return Err(invalid("its volume uuid is not canonical"));
        }
        if self.releases > 0 && self.last_release_ms == 0 {
            return Err(invalid("it records releases but no last release time"));
        }
        if self.first_created_ms > self.recorded_ms && self.first_created_ms != 0 {
            return Err(invalid("its creation time is after its own write"));
        }
        if self.last_release_ms > self.recorded_ms {
            return Err(invalid("its last release is after its own write"));
        }
        Ok(())
    }
}

/// How one root's activity record reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStateName {
    /// The record verifies and records at least one release.
    Released,
    /// The record verifies but records no release: the root was created and
    /// never released.
    Unreleased,
    /// There is no record at all.
    Missing,
    /// A record is there but does not verify.
    Unverified,
}

/// One root's activity as a document reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityRow {
    /// Whether the record verifies, records no release, is missing or is not
    /// readable.
    pub state: ActivityStateName,
    /// The last recorded release, when a record was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_release_ms: Option<u64>,
    /// The recorded creation time, when a record was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_created_ms: Option<u64>,
    /// The recorded release count, when a record was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub releases: Option<u64>,
    /// The record's generation, when a record was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// When the record was written, when one was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_ms: Option<u64>,
    /// Why a missing or unverified record is reported that way, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Default for ActivityRow {
    /// A row a document from before this slice does not carry. Absence is
    /// reported as unverified, never as a release.
    fn default() -> Self {
        Self {
            state: ActivityStateName::Unverified,
            last_release_ms: None,
            first_created_ms: None,
            releases: None,
            generation: None,
            recorded_ms: None,
            reason: Some("the document does not report this root's activity record".to_owned()),
        }
    }
}

/// What reading one root's activity record found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityReading {
    /// How the record reads.
    pub state: ActivityStateName,
    /// The record, when it verified.
    pub record: Option<ActivityRecord>,
    /// Why it is missing or unverified, when a reason exists.
    pub reason: Option<String>,
}

impl ActivityReading {
    /// No record exists.
    pub fn missing() -> Self {
        Self {
            state: ActivityStateName::Missing,
            record: None,
            reason: Some(
                "no activity record exists for the root, so Barge has recorded no release for it"
                    .to_owned(),
            ),
        }
    }

    /// A record that did not verify.
    pub fn unverified(reason: impl Into<String>) -> Self {
        Self {
            state: ActivityStateName::Unverified,
            record: None,
            reason: Some(reason.into()),
        }
    }

    /// The row a document reports.
    pub fn row(&self) -> ActivityRow {
        ActivityRow {
            state: self.state,
            last_release_ms: self.record.as_ref().map(|record| record.last_release_ms),
            first_created_ms: self.record.as_ref().map(|record| record.first_created_ms),
            releases: self.record.as_ref().map(|record| record.releases),
            generation: self.record.as_ref().map(|record| record.generation),
            recorded_ms: self.record.as_ref().map(|record| record.recorded_ms),
            reason: self.reason.clone(),
        }
    }
}

/// Read one root's activity record, bounded and strict.
///
/// A missing file is reported as missing, an unreadable, oversized, symlinked,
/// foreign-owned or schema-divergent one as unverified. A read failure of the
/// state directory itself is an error a caller reports per object rather than a
/// reason to fail unrelated work.
pub fn read_activity(
    state: &StatePaths,
    observer: &dyn ProcessObserver,
    root_id: &str,
) -> Result<ActivityReading> {
    state::validate_identifier("root_id", root_id)?;
    if !state.is_initialized()? {
        return Ok(ActivityReading::missing());
    }
    let path = state.activity_path(root_id)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ActivityReading::missing())
        }
        Err(error) => {
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot read {}: {error}", path.display()),
            )
            .with_detail("path", path.display().to_string().into()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(ActivityReading::unverified(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let effective_uid = observer.effective_uid()?;
    if metadata.uid() != effective_uid {
        return Ok(ActivityReading::unverified(format!(
            "{} is owned by uid {}, not this account's {effective_uid}",
            path.display(),
            metadata.uid()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        return Ok(ActivityReading::unverified(format!(
            "{} has mode {:o}, which is not private",
            path.display(),
            metadata.mode() & 0o7777
        )));
    }
    let bytes = match state::read_bounded(
        &path,
        &Bounds::new(
            crate::contract::ACTIVITY_CAP_BYTES,
            1,
            Deadline::after(READ_DEADLINE),
        ),
    ) {
        ReadOutcome::Present(bytes) => bytes,
        ReadOutcome::Absent => return Ok(ActivityReading::missing()),
        ReadOutcome::Refused(error) => return Ok(ActivityReading::unverified(error.to_string())),
    };
    let record: ActivityRecord = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => {
            return Ok(ActivityReading::unverified(format!(
                "the activity record is unreadable: {error}"
            )))
        }
    };
    if let Err(error) = record.validate() {
        return Ok(ActivityReading::unverified(error.to_string()));
    }
    if record.root_id != root_id {
        return Ok(ActivityReading::unverified(format!(
            "the activity record names root `{}`, not `{root_id}`",
            record.root_id
        )));
    }
    let state_name = if record.last_release_ms == 0 {
        ActivityStateName::Unreleased
    } else {
        ActivityStateName::Released
    };
    Ok(ActivityReading {
        state: state_name,
        record: Some(record),
        reason: None,
    })
}

/// Publish one activity record atomically and privately, through a publisher seam
/// so a test can prove what a publish that fails *after* its rename does.
/// [`state::publish_atomic`] renames and then syncs the directory, so its failure
/// does not always mean the new bytes are absent.
fn publish_activity_with(
    state: &StatePaths,
    record: &ActivityRecord,
    write: &dyn Fn(&Path, &[u8]) -> Result<()>,
) -> Result<PathBuf> {
    record.validate()?;
    let directory = state.activity_dir();
    ensure_private_directory(&directory)?;
    let path = state.activity_path(&record.root_id)?;
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|error| {
        Error::new(
            ErrorKind::Io,
            format!("could not encode the activity record: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > crate::contract::ACTIVITY_CAP_BYTES {
        return Err(Error::new(
            ErrorKind::ResourceBound,
            format!(
                "the activity record is over the {}-byte cap",
                crate::contract::ACTIVITY_CAP_BYTES
            ),
        )
        .with_detail("cap", crate::contract::ACTIVITY_CAP_BYTES.into()));
    }
    write(&path, &bytes)?;
    Ok(path)
}

/// Record a root's creation: no release yet, generation zero.
///
/// The caller holds this root's lock (rank 3), which is the lock every later
/// write to the same record takes.
fn record_activity_creation(
    state: &StatePaths,
    volume_uuid: &VolumeUuid,
    root_id: &str,
    created_ms: u64,
    io: ActivityIo,
) -> Result<ActivityRecord> {
    let now = state::now_ms();
    let record = ActivityRecord {
        schema: crate::contract::ACTIVITY_SCHEMA.to_owned(),
        root_id: root_id.to_owned(),
        volume_uuid: volume_uuid.as_str().to_owned(),
        last_release_ms: 0,
        first_created_ms: created_ms.min(now),
        releases: 0,
        generation: 0,
        recorded_ms: now,
    };
    publish_fact(
        state,
        &record,
        None,
        io,
        &format!("the creation of root `{root_id}`"),
    )?;
    Ok(record)
}

/// Account for the one step that runs after a root, its marker and its identity
/// record exist: publishing its activity record.
///
/// Creation never deletes a root it created, so a failure here is a created root
/// whose activity is unsettled. The report names the root, its path and its
/// volume, and says the root is kept — never idle, because its activity is absent
/// or invalid — until an operator removes it or records a baseline.
fn creation_unsettled(owned: &OwnedRoot, error: Error) -> Error {
    error
        .prefixed(&format!(
            "root {} was created and kept, but its activity record could not be recorded",
            owned.root_id
        ))
        .with_detail("created_root", true.into())
        .with_detail("root_id", owned.root_id.clone().into())
        .with_detail("path", owned.path.display().to_string().into())
        .with_detail("volume_uuid", owned.volume_uuid.as_str().into())
        .with_detail(
            "next",
            format!(
                "the root is kept and never idle until `barge area idle-baseline --root {}` records its activity, or an operator removes the root",
                owned.root_id
            )
            .into(),
        )
}

/// The record one release writes, from the record it read.
fn released_record(previous: &ActivityRecord) -> ActivityRecord {
    let now = state::now_ms();
    ActivityRecord {
        last_release_ms: now,
        releases: previous.releases.saturating_add(1),
        generation: previous.generation.saturating_add(1),
        recorded_ms: now,
        ..previous.clone()
    }
}

/// Why a root is not idle under the space policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleReason {
    /// The marker did not verify as an owned root of this volume and path.
    MarkerUnverified,
    /// The activity record is missing or invalid.
    ActivityUnknown,
    /// The activity record verifies but records no release.
    ActivityUnreleased,
    /// The activity record's last release is in the future (clock skew).
    ActivityFuture,
    /// A live hold covers the root.
    HoldLive,
    /// A hold cannot be decided.
    HoldUnknown,
    /// The area's `reserve` is `excluded`, or the folder is not an area.
    ParticipationExcluded,
    /// The root's area is not declared at all.
    ParticipationUndeclared,
    /// A scratch root is not positively concluded, so it is never pressure
    /// pruned.
    UnconcludedScratch,
    /// The activity record or the hold reading is not bound to the root the
    /// verified marker proves: another root's record, another volume's record, or
    /// a hold key that is not the marker's root id.
    IdentityMismatch,
}

impl IdleReason {
    /// The name a document reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MarkerUnverified => "marker_unverified",
            Self::ActivityUnknown => "activity_unknown",
            Self::ActivityUnreleased => "activity_unreleased",
            Self::ActivityFuture => "activity_future",
            Self::HoldLive => "hold_live",
            Self::HoldUnknown => "hold_unknown",
            Self::ParticipationExcluded => "participation_excluded",
            Self::ParticipationUndeclared => "participation_undeclared",
            Self::UnconcludedScratch => "unconcluded_scratch",
            Self::IdentityMismatch => "identity_mismatch",
        }
    }
}

/// The two states the pressure idleness matrix decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleState {
    /// Every input positively concluded idleness.
    Idle,
    /// At least one input did not, so the root is kept.
    NotIdle,
}

/// One root's idleness under the space policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleRow {
    /// Idle or not idle.
    pub state: IdleState,
    /// The first input that kept the root, when it is not idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<IdleReason>,
}

impl IdleRow {
    /// A root that is not idle for this reason.
    pub const fn blocked(reason: IdleReason) -> Self {
        Self {
            state: IdleState::NotIdle,
            reason: Some(reason),
        }
    }

    /// Whether the root may yield under the reserve.
    pub const fn is_idle(&self) -> bool {
        matches!(self.state, IdleState::Idle)
    }
}

impl Default for IdleRow {
    /// A row a document from before this slice does not carry. Absence is never
    /// idleness, so the default keeps the root.
    fn default() -> Self {
        Self {
            state: IdleState::NotIdle,
            reason: None,
        }
    }
}

/// Why a root's bytes may or may not yield under the reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleParticipation {
    /// A declared area whose `reserve` is `counts`: its idle roots yield.
    Counts,
    /// A declared area whose `reserve` is `excluded`: its roots never yield for
    /// space.
    Excluded,
    /// Barge's own scratch feature, which participates by construction.
    Scratch,
    /// A folder Barge has no declaration for: never Barge's to prune.
    Undeclared,
}

impl IdleParticipation {
    /// The participation a declared area's `reserve` value gives.
    pub const fn of_area(reserve: ReserveParticipation) -> Self {
        match reserve {
            ReserveParticipation::Counts => Self::Counts,
            ReserveParticipation::Excluded => Self::Excluded,
        }
    }
}

/// Everything the pressure idleness matrix decides on.
#[derive(Debug, Clone, Copy)]
pub struct IdleInputs<'a> {
    /// What the marker read decided.
    pub marker: &'a MarkerState,
    /// The root id the activity record and the holds were read under. Evidence is
    /// only this root's when that key is the root id the marker proves.
    pub root_id: &'a str,
    /// The verified volume's canonical uuid text as the marker was read on it. A
    /// record written on another volume is not this root's release.
    pub volume_uuid: &'a str,
    /// What the activity record read decided.
    pub activity: &'a ActivityReading,
    /// What the holds read decided.
    pub holds: &'a HoldReading,
    /// Whether the root's area participates.
    pub participation: IdleParticipation,
    /// The current time, so a future-dated release is not idle.
    pub now_ms: u64,
}

/// Decide one root's idleness under the space policy.
///
/// A root is idle only when its marker verifies, its activity record verifies and
/// records a release, every hold is positively absent or stale, its area
/// participates, and (for a scratch root) its run is positively concluded. The
/// first input that does not conclude keeps the root and is reported; anything
/// undecidable is not idle, and there is no "probably done" widening (stage 3
/// design, "The only source of idleness").
pub fn assess_idleness(inputs: &IdleInputs<'_>) -> IdleRow {
    let marker = match inputs.marker {
        MarkerState::Owned(marker) => marker,
        MarkerState::Absent | MarkerState::Unknown { .. } => {
            return IdleRow::blocked(IdleReason::MarkerUnverified)
        }
    };
    // The evidence must belong to the root the verified marker proves. A record
    // read under another key, or one written on another volume, is not this
    // root's release, and a hold read under another key is not this root's hold.
    if inputs.root_id != marker.root_id {
        return IdleRow::blocked(IdleReason::IdentityMismatch);
    }
    match inputs.participation {
        IdleParticipation::Counts | IdleParticipation::Scratch => {}
        IdleParticipation::Excluded => return IdleRow::blocked(IdleReason::ParticipationExcluded),
        IdleParticipation::Undeclared => {
            return IdleRow::blocked(IdleReason::ParticipationUndeclared)
        }
    }
    // Holds come before the activity record: a root a consumer is writing right
    // now is kept because it is held, which is the fact the stop's attribution
    // reports, not because its release has not been recorded yet.
    for (_, state, _) in &inputs.holds.holds {
        match state {
            HoldState::Live => return IdleRow::blocked(IdleReason::HoldLive),
            HoldState::Unknown => return IdleRow::blocked(IdleReason::HoldUnknown),
            HoldState::Stale => {}
        }
    }
    if inputs.holds.truncated {
        // A hold list cut by its bound cannot prove the absence of a live hold.
        return IdleRow::blocked(IdleReason::HoldUnknown);
    }
    match inputs.activity.state {
        ActivityStateName::Released => {}
        ActivityStateName::Unreleased => return IdleRow::blocked(IdleReason::ActivityUnreleased),
        ActivityStateName::Missing | ActivityStateName::Unverified => {
            return IdleRow::blocked(IdleReason::ActivityUnknown)
        }
    }
    let Some(record) = inputs.activity.record.as_ref() else {
        return IdleRow::blocked(IdleReason::ActivityUnknown);
    };
    if record.root_id != marker.root_id || record.volume_uuid != inputs.volume_uuid {
        return IdleRow::blocked(IdleReason::IdentityMismatch);
    }
    if record.last_release_ms > inputs.now_ms || record.recorded_ms > inputs.now_ms {
        return IdleRow::blocked(IdleReason::ActivityFuture);
    }
    if marker.kind == RootKind::ScratchRun && !positively_concluded(marker.conclusion) {
        return IdleRow::blocked(IdleReason::UnconcludedScratch);
    }
    IdleRow {
        state: IdleState::Idle,
        reason: None,
    }
}

/// Whether a scratch root's conclusion is the one the expiry predicate accepts:
/// closed, positive, exclusive and kept as evidence. The pressure predicate
/// relaxes only the age, never these four facts (design, O-015).
pub fn positively_concluded(conclusion: Option<RootConclusion>) -> bool {
    conclusion.is_some_and(|conclusion| {
        conclusion.outcome == ConclusionOutcome::Closed
            && conclusion.closure == ConclusionClosure::Positive
            && conclusion.nested_scope == NestedScope::Exclusive
            && conclusion.kept_reason == KeptReason::Evidence
    })
}

/// One idle root's position in the volume-wide least-recently-used order.
///
/// The order is the design's: the older recorded release first, then the older
/// recorded creation, then the root id ascending as a stable, arbitrary
/// tie-break. `first_created_ms == 0` is the record's "never recorded" value and
/// is ordered after every known creation time, so a tie is never resolved by
/// treating an unknown age as the oldest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LruKey {
    /// The last recorded release.
    pub last_release_ms: u64,
    /// The recorded creation time, `0` when it was never recorded.
    pub first_created_ms: u64,
    /// The root the key orders.
    pub root_id: String,
}

impl LruKey {
    /// The key a released record gives, or none when the record records no
    /// release: a root that is not idle is never a candidate.
    pub fn of(record: &ActivityRecord) -> Option<Self> {
        if record.last_release_ms == 0 {
            return None;
        }
        Some(Self {
            last_release_ms: record.last_release_ms,
            first_created_ms: record.first_created_ms,
            root_id: record.root_id.clone(),
        })
    }

    /// The creation time with an unknown value ordered last.
    const fn creation_rank(&self) -> u64 {
        if self.first_created_ms == 0 {
            u64::MAX
        } else {
            self.first_created_ms
        }
    }
}

impl Ord for LruKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.last_release_ms
            .cmp(&other.last_release_ms)
            .then_with(|| self.creation_rank().cmp(&other.creation_rank()))
            .then_with(|| self.root_id.cmp(&other.root_id))
    }
}

impl PartialOrd for LruKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// What a caller asks [`idle_baseline`] to repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleBaselineRequest {
    /// The root to baseline.
    pub root_id: String,
    /// Where the root is, when Barge has no durable identity record for it. Both
    /// this and `mount_relative_path` are given, or neither.
    pub volume_uuid: Option<VolumeUuid>,
    /// The root's path below the volume's mount point, as an explicit
    /// alternative to the durable identity record.
    pub mount_relative_path: Option<String>,
}

/// The record operations a fact publication performs: the publish itself, the read
/// that verifies what it left behind, and its three fail-closed recovery steps.
///
/// They are seams (the shape [`state::publish_atomic_with`] already uses) so a test
/// can prove what a publish that fails *after* its rename does, what a verification
/// read that cannot complete does, and what each recovery step does when it fails
/// too.
#[derive(Clone, Copy)]
pub(crate) struct ActivityIo {
    /// The fact's own publish.
    pub write: fn(&Path, &[u8]) -> Result<()>,
    /// Put the previous record's exact bytes back.
    pub restore: fn(&Path, &[u8]) -> Result<()>,
    /// Remove the unconfirmed record.
    pub remove: fn(&Path) -> Result<()>,
    /// Make the unconfirmed record invalid in place.
    pub invalidate: fn(&Path) -> Result<()>,
    /// Read the record's exact bytes back, bounded, to verify a failed publish.
    pub read: fn(&StatePaths, &str) -> Result<Option<Vec<u8>>>,
}

impl ActivityIo {
    /// The real operations.
    pub(crate) const fn real() -> Self {
        Self {
            write: state::publish_atomic,
            restore: state::publish_atomic,
            remove: remove_state_file,
            invalidate: invalidate_state_file,
            read: activity_record_bytes,
        }
    }
}

/// Publish one activity fact, with the fail-closed recovery every fact write needs.
///
/// [`state::publish_atomic`] renames and then syncs the directory, so a failure
/// does not always mean the new bytes are absent. The record is read back after a
/// failure and only bytes identical to the previous record count as "nothing
/// changed"; anything else is recovered in three steps — restore the previous
/// bytes, else remove the unconfirmed record, else invalidate it in place — so an
/// unconfirmed fact is never left readable as a release. A verification read that
/// itself cannot complete is **not** evidence that nothing changed: unknown is
/// never taken as "no effect", so the recovery runs and the record is restored,
/// removed or invalidated even though the read failed. The only state this cannot
/// mark is one whose volume refuses every write, and that is reported as a limit
/// rather than assumed durable (Q-012).
fn publish_fact(
    state: &StatePaths,
    record: &ActivityRecord,
    previous_bytes: Option<&[u8]>,
    io: ActivityIo,
    context: &str,
) -> Result<()> {
    let root_id = record.root_id.as_str();
    let path = state.activity_path(root_id)?;
    let error = match publish_activity_with(state, record, &io.write) {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    let record_path = path.display().to_string();
    let failed_kind = error.kind();
    // Only a record whose bytes are provably unchanged settles this as "nothing
    // happened". A read that cannot complete proves nothing, so it is treated as
    // an unconfirmed publication and recovered.
    let landed = match (io.read)(state, root_id) {
        Ok(None) => false,
        Ok(Some(now)) => previous_bytes.is_none_or(|before| now != before),
        Err(_) => true,
    };
    if !landed {
        return Err(Error::new(
            failed_kind,
            format!("{context} was refused and the activity record is unchanged: {error}"),
        )
        .with_detail("activity_path", record_path.into())
        .with_detail("restored", true.into())
        .with_detail("cause", error.to_string().into()));
    }
    let restore_error = match previous_bytes {
        Some(bytes) => match (io.restore)(&path, bytes) {
            Ok(()) => {
                return Err(Error::new(
                    failed_kind,
                    format!(
                    "{context} was refused and the advanced activity record was restored: {error}"
                ),
                )
                .with_detail("activity_path", record_path.into())
                .with_detail("restored", true.into())
                .with_detail("cause", error.to_string().into()))
            }
            Err(restore_error) => restore_error,
        },
        None => Error::new(
            ErrorKind::UnknownState,
            "there was no previous record to restore",
        ),
    };
    let removed = |cause: Error, restore_error: Error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "{context} could not be recorded: its publish was not confirmed durable ({cause}) and the previous record could not be restored ({restore_error}), so the unconfirmed record for root `{root_id}` was removed. The root reads as activity unknown and is never idle until `barge area idle-baseline` records it"
            ),
        )
        .with_detail("activity_path", record_path.clone().into())
        .with_detail("restored", false.into())
        .with_detail("removed", true.into())
        .with_detail("cause", cause.to_string().into())
        .with_detail("restore_error", restore_error.to_string().into())
    };
    let invalidated = |cause: Error, restore_error: Error, remove_error: Error| {
        Error::new(
            ErrorKind::UnknownState,
            format!(
                "{context} could not be recorded: its publish was not confirmed durable ({cause}), the previous record could not be restored ({restore_error}) and the unconfirmed record for root `{root_id}` could not be removed ({remove_error}), so it was invalidated in place. The root reads as activity unknown and is never idle until `barge area idle-baseline` records it"
            ),
        )
        .with_detail("activity_path", record_path.clone().into())
        .with_detail("restored", false.into())
        .with_detail("removed", false.into())
        .with_detail("invalidated", true.into())
        .with_detail("cause", cause.to_string().into())
        .with_detail("restore_error", restore_error.to_string().into())
        .with_detail("remove_error", remove_error.to_string().into())
    };
    match (io.remove)(&path) {
        Ok(()) => Err(removed(error, restore_error)),
        Err(remove_error) => {
            // A removal that errored but left the record positively absent is
            // settled: the record is gone, which is all the removal had to achieve.
            if matches!((io.read)(state, root_id), Ok(None)) {
                return Err(removed(error, restore_error));
            }
            match (io.invalidate)(&path) {
                Ok(()) => Err(invalidated(error, restore_error, remove_error)),
                Err(invalidate_error) => match (io.read)(state, root_id) {
                    // A removal or invalidation that errored but left the record
                    // positively absent, or present but no longer a readable
                    // record, has done its job as well.
                    Ok(None) => Err(removed(error, restore_error)),
                    Ok(Some(bytes))
                        if serde_json::from_slice::<ActivityRecord>(&bytes).is_err() =>
                    {
                        Err(invalidated(error, restore_error, remove_error))
                    }
                    _ => Err(Error::new(
                        ErrorKind::UnknownState,
                        format!(
                            "{context} could not be recorded: its publish was not confirmed durable ({error}), the previous record could not be restored ({restore_error}), the unconfirmed record for root {root_id} could not be removed ({remove_error}) and it could not be invalidated ({invalidate_error}). A state volume that refuses every write cannot be marked, so the record may still be readable until an operator repairs the state volume and runs `barge area idle-baseline`"
                        ),
                    )
                    .with_detail("activity_path", record_path.into())
                    .with_detail("restored", false.into())
                    .with_detail("removed", false.into())
                    .with_detail("invalidated", false.into())
                    .with_detail("cause", error.to_string().into())
                    .with_detail("restore_error", restore_error.to_string().into())
                    .with_detail("remove_error", remove_error.to_string().into())
                    .with_detail("invalidate_error", invalidate_error.to_string().into())),
                },
            }
        }
    }
}

/// The `barge.area.idle-baseline/1` document's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdleBaselineReport {
    /// The root that was repaired.
    pub root_id: String,
    /// The verified volume it is on.
    pub volume_uuid: VolumeUuid,
    /// The path the walk reached.
    pub path: PathBuf,
    /// The activity record as written.
    pub activity: ActivityRow,
    /// The custody verdict the write was gated on. Only `safe` is ever written.
    pub custody: String,
}

/// Write a fresh activity record asserting a root is idle as of now.
///
/// This is the design's only repair for a lost or invalid activity record: Barge
/// never infers idleness from an absent record, and without this command the root
/// stays kept, forever if need be. It re-verifies the volume and the root's
/// marker, refuses if any hold is live or undecidable, takes the stage 1 custody
/// verdict for the root, and then writes the record while holding the root lock.
#[allow(clippy::too_many_arguments)]
pub fn idle_baseline(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &IdleBaselineRequest,
) -> Result<IdleBaselineReport> {
    idle_baseline_with(
        state,
        dirs,
        facts,
        mounts,
        observer,
        request,
        ActivityIo::real(),
    )
}

/// [`idle_baseline`] with its record writes as seams, so a test can prove that a
/// baseline whose publish fails after its rename leaves no readable idle fact.
#[allow(clippy::too_many_arguments)]
pub(crate) fn idle_baseline_with(
    state: &StatePaths,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    observer: &dyn ProcessObserver,
    request: &IdleBaselineRequest,
    io: ActivityIo,
) -> Result<IdleBaselineReport> {
    state::validate_identifier("root_id", &request.root_id)?;
    if request.volume_uuid.is_some() != request.mount_relative_path.is_some() {
        return Err(Error::new(
            ErrorKind::Usage,
            "an explicit location needs both the volume and the root's mount-relative path, or neither",
        ));
    }
    // Every mutating path starts at the registry: an invalid registry refuses
    // before any effect, and it is never rebuilt from markers. The state layout
    // is created only once the root is located and verified, so a refusal for a
    // root Barge cannot find leaves nothing behind at all.
    registry::read(state)?;
    let effective_uid = observer.effective_uid()?;
    if let Some(relative) = &request.mount_relative_path {
        // Containment is decided before any record is compared: a path outside
        // the volume is refused whatever the records say.
        folder_components("mount_relative_path", relative)?;
    }
    let (volume_uuid, relative) = match (&request.volume_uuid, &request.mount_relative_path) {
        (Some(volume_uuid), Some(relative)) => {
            // An explicit location never overrules Barge's own durable record: a
            // disagreement is reported, not resolved in either direction.
            if let Some(durable) =
                crate::identity::read(state, dirs, &request.root_id, effective_uid)?
            {
                if durable.volume_uuid != volume_uuid.as_str()
                    || &durable.mount_relative_path != relative
                {
                    return Err(Error::new(
                        ErrorKind::UnknownState,
                        format!(
                            "root {}'s durable identity record names {} at {}, not the given {volume_uuid} at {relative}; the disagreement is reported, not resolved",
                            request.root_id, durable.volume_uuid, durable.mount_relative_path
                        ),
                    )
                    .with_detail("record_volume_uuid", durable.volume_uuid.clone().into())
                    .with_detail("record_path", durable.mount_relative_path.clone().into()));
                }
            }
            (volume_uuid.clone(), relative.clone())
        }
        _ => match crate::identity::read(state, dirs, &request.root_id, effective_uid)? {
            Some(record) => (VolumeUuid::parse(&record.volume_uuid)?, record.mount_relative_path),
            None => {
                return Err(Error::new(
                    ErrorKind::UnknownState,
                    format!(
                        "Barge has no durable record of where root {} is, so it cannot re-verify the root; give its volume and mount-relative path explicitly",
                        request.root_id
                    ),
                )
                .with_detail("root_id", request.root_id.clone().into()))
            }
        },
    };
    let components = folder_components("mount_relative_path", &relative)?;
    let (name, parent) = components.split_last().ok_or_else(|| {
        Error::new(
            ErrorKind::RefusedContainment,
            "a root is never the volume's own root directory",
        )
        .with_detail("value", relative.clone().into())
    })?;
    if parent.is_empty() {
        return Err(Error::new(
            ErrorKind::RefusedContainment,
            "a root is never created directly at a volume's mount root",
        )
        .with_detail("value", relative.clone().into()));
    }
    let folder = parent.join("/");
    let record = registered_volume(state, &volume_uuid)?;
    record.declared_durability()?;
    let volume = VolumeRoot::open(dirs, facts, mounts, &record)?;
    let mount_point = volume.mount_point().to_path_buf();
    let device = volume.identity().device;
    let walked = walk_folder(
        dirs,
        volume.into_handle(),
        parent,
        device,
        &mount_point,
        effective_uid,
    )?;
    if !walked.exists() {
        return Err(Error::new(
            ErrorKind::UnknownState,
            format!("the root's folder `{folder}` is not on volume {volume_uuid}"),
        )
        .with_detail("volume_uuid", volume_uuid.as_str().into())
        .with_detail("folder", folder.clone().into()));
    }
    let root_path = walked.path.join(name);
    let root = dirs.open_child_dir(&walked.handle, name)?;
    let expectation = MarkerExpectation {
        volume_uuid: &volume_uuid,
        resolved_path: &root_path,
        mount_point: Some(&mount_point),
        effective_uid,
        now_ms: state::now_ms(),
    };
    let (marker, marker_digest) = read_marker_with_digest(dirs, &root, &expectation);
    let marker = match marker {
        MarkerState::Owned(marker) => marker,
        MarkerState::Absent => {
            return Err(Error::new(
                ErrorKind::RefusedBoundary,
                format!(
                    "{} carries no marker, so it is not a root Barge owns",
                    root_path.display()
                ),
            )
            .with_detail("path", root_path.display().to_string().into()))
        }
        MarkerState::Unknown { reason } => {
            return Err(Error::new(
                ErrorKind::RefusedBoundary,
                format!(
                    "the marker at {} does not verify: {reason}",
                    root_path.display()
                ),
            )
            .with_detail("path", root_path.display().to_string().into())
            .with_detail("reason", reason.into()))
        }
    };
    if marker.root_id != request.root_id {
        return Err(Error::new(
            ErrorKind::RefusedBoundary,
            format!(
                "{} is root {}, not the requested {}",
                root_path.display(),
                marker.root_id,
                request.root_id
            ),
        )
        .with_detail("path", root_path.display().to_string().into())
        .with_detail("root_id", marker.root_id.clone().into()));
    }
    let marker_digest = marker_digest.ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "the root's marker digest was not read, so custody cannot be proved",
        )
    })?;
    state.initialize()?;
    let mut locks = state::LockSet::new(state.clone());
    let _lock = locks.lock_root(&request.root_id)?;
    // The design refuses a baseline while **any** hold exists, not only a live
    // one: a present hold file — live, stale or undecidable — means the root's
    // custody is not positively settled, so the operator releases it first. The
    // root lock above is what a racing `hold` takes too.
    let holds = read_holds(state, observer, &request.root_id)?;
    if let Some((hold, hold_state, reason)) = holds.holds.first() {
        let state_name = match hold_state {
            HoldState::Live => "live",
            HoldState::Stale => "stale",
            HoldState::Unknown => "undecidable",
        };
        let message = format!(
            "root {} carries a {state_name} hold `{}` on pid {}; an idle baseline needs no hold at all, so release it first",
            request.root_id, hold.holder, hold.pid
        );
        return Err(match hold_state {
            HoldState::Live | HoldState::Stale => Error::new(ErrorKind::RefusedBoundary, message)
                .with_detail("holder", hold.holder.clone().into())
                .with_detail("pid", u64::from(hold.pid).into())
                .with_detail("hold_state", state_name.into()),
            HoldState::Unknown => Error::new(
                ErrorKind::CustodyNotProven,
                format!(
                    "{message}; it cannot be decided: {}",
                    reason.as_deref().unwrap_or("no reason was recorded")
                ),
            )
            .with_detail("holder", hold.holder.clone().into())
            .with_detail("hold_state", state_name.into()),
        });
    }
    if holds.truncated {
        return Err(Error::new(
            ErrorKind::CustodyNotProven,
            format!(
                "root {} carries more hold files than the {HOLD_ROWS}-hold bound, so none can be proved absent",
                request.root_id
            ),
        ));
    }
    let intents = state::read_intents(state, crate::contract::INTENT_ROWS)?;
    if intents.truncated {
        return Err(Error::new(
            ErrorKind::CustodyNotProven,
            "the intent census is truncated, so custody cannot be proved",
        ));
    }
    let unit = crate::unit::DeletionUnit {
        volume_uuid: volume_uuid.clone(),
        root_dir: String::new(),
        folder: folder.clone(),
        parent: String::new(),
        name: name.clone(),
        root_id: None,
        kind: crate::unit::UnitKind::Directory,
        device: root.identity().device,
        inode: root.identity().inode,
        marker: Some(crate::unit::UnitMarker::new(
            crate::unit::MarkerFamily::Barge,
            marker_digest,
        )),
        grant: crate::unit::UnitGrant::native_root(&request.root_id),
        plan_digest: "0".repeat(64),
    };
    let pending = crate::custody::PendingIntent::pending(&intents.records);
    crate::custody::take_custody(
        dirs,
        facts,
        mounts,
        observer,
        state,
        &unit,
        &pending,
        crate::custody::CustodyBounds::default(),
    )
    .into_evidence()?;
    let previous = read_activity(state, observer, &request.root_id)?;
    let previous_bytes = activity_record_bytes(state, &request.root_id)?;
    if let Some(existing) = previous.record.as_ref() {
        if existing.volume_uuid != volume_uuid.as_str() {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "root {}'s activity record names volume {}, but the root is on {volume_uuid}; the disagreement is reported, not resolved",
                    request.root_id, existing.volume_uuid
                ),
            )
            .with_detail("record_volume_uuid", existing.volume_uuid.clone().into())
            .with_detail("volume_uuid", volume_uuid.as_str().into()));
        }
    }
    let now = state::now_ms();
    let first_created_ms = previous
        .record
        .as_ref()
        .map(|record| record.first_created_ms)
        .filter(|value| *value != 0)
        .unwrap_or(marker.created_ms);
    let record = ActivityRecord {
        schema: crate::contract::ACTIVITY_SCHEMA.to_owned(),
        root_id: request.root_id.clone(),
        volume_uuid: volume_uuid.as_str().to_owned(),
        last_release_ms: now,
        first_created_ms: first_created_ms.min(now),
        releases: previous.record.as_ref().map_or(0, |record| record.releases),
        generation: previous
            .record
            .as_ref()
            .map_or(1, |record| record.generation.saturating_add(1)),
        recorded_ms: now,
    };
    publish_fact(
        state,
        &record,
        previous_bytes.as_deref(),
        io,
        &format!("the idle baseline for root `{}`", request.root_id),
    )?;
    Ok(IdleBaselineReport {
        root_id: request.root_id.clone(),
        volume_uuid,
        path: root_path,
        activity: ActivityReading {
            state: ActivityStateName::Released,
            record: Some(record),
            reason: None,
        }
        .row(),
        custody: crate::custody::CAPABILITY.to_owned(),
    })
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
    /// The newest activity Barge observed: the newest modification time found by
    /// the bounded scan of the root's own subtree, with the root directory's own
    /// modification time as the floor (stage 4 design, "Last activity"). `None`
    /// means the scan could not decide the age, which is never "idle".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    /// The scan's own evidence: the bytes it counted and whether it reached the
    /// whole subtree. It is an optional addition to this document's field set, so
    /// a row written before it still reads (stage 4 design, "Last activity").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<RootActivity>,
    /// Whether no live hold covers the root, which is stage 1's idleness for
    /// `maintain`'s age policy: its holds are the only input it reads.
    pub idle: bool,
    /// The root's `barge.activity/1` record as this listing read it, which is the
    /// evidence the pressure idleness matrix reads. The row's `activity` is the
    /// separate last-activity scan above, so the record has its own name. A
    /// document from a build before this field reports `unverified`, never a
    /// release.
    #[serde(default)]
    pub activity_record: ActivityRow,
    /// Whether the root is idle under the stage 3 pressure idleness matrix,
    /// which adds the activity record and the area's `reserve` participation to
    /// the marker and holds. Absent in an older document is never idleness.
    #[serde(default)]
    pub space_idle: IdleRow,
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

/// How much one listing observes about a root's activity.
///
/// A decision about a root's age must use [`ActivityMode::Scan`]: the stage 4
/// design fixes `last_activity_ms` as the newest modification time found by a
/// bounded scan of the root's subtree, never a marker's `created_ms` alone
/// ("Last activity"). A read-only report carries the cheap floor instead, so a
/// `describe` or a listing never walks a consumer's whole tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityMode {
    /// The root directory's own modification time and the marker's creation
    /// time, whichever is newer. Cheap, and enough for a report that decides
    /// nothing.
    Floor,
    /// The bounded scan of the root's subtree, with the root directory's own
    /// modification time as its floor.
    Scan,
}

/// One entry an area folder holds, as a names-only enumeration reports it.
///
/// It is a name and the path the current configuration resolves to, and nothing
/// else: no marker, no hold, no opened descriptor, no device, no inode and no
/// entry-type check. A caller that must observe the entry takes its own lock
/// first and reads it there, which is also where a symbolic link is discovered
/// (it is never followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootName {
    /// The entry's name inside the area folder.
    pub name: String,
    /// The path the current configuration resolves for it.
    pub path: PathBuf,
}

/// One bounded, names-only enumeration of an area folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameListing {
    /// The entries enumerated, in the platform's listing order.
    pub names: Vec<RootName>,
    /// How many entries the folder held.
    pub examined: usize,
    /// Whether the enumeration was cut by the row or name bound.
    pub truncated: bool,
}

/// Enumerate one resolved area's entries by name and observe nothing else.
///
/// It uses [`DirOps::list_names`], which reads the directory's names and no
/// entry's own facts: no marker, no hold, no device, no inode and no entry type.
/// A caller that decides anything about one of these entries must take that
/// entry's lock first and observe it under it. Because no entry is stated here, a
/// child that cannot be stated cannot fail the enumeration either: that entry is
/// reported by the caller's own row for it.
pub fn list_names(
    dirs: &dyn DirOps,
    resolved: &ResolvedArea,
    max_roots: usize,
) -> Result<NameListing> {
    let handle = match &resolved.dirs {
        Direction::Existing { handle } => dirs.duplicate_dir(handle)?,
        Direction::Missing { .. } => {
            #[cfg(test)]
            crate::tape::listing("area::list_names", resolved.path.as_path(), 0, 0, false);
            #[cfg(test)]
            return Ok(NameListing {
                names: Vec::new(),
                examined: 0,
                truncated: false,
            });
            #[cfg(not(test))]
            return Ok(NameListing {
                names: Vec::new(),
                examined: 0,
                truncated: false,
            })
        }
    };
    let name_cap = max_roots.clamp(1, LISTING_NAME_ROWS);
    let listing = dirs.list_names(&handle, name_cap)?;
    let mut names = Vec::new();
    let mut truncated = listing.truncated;
    for name in listing.names {
        if names.len() >= max_roots {
            truncated = true;
            break;
        }
        names.push(RootName {
            path: resolved.path.join(&name),
            name,
        });
    }
    #[cfg(test)]
    crate::tape::listing(
        "area::list_names",
        resolved.path.as_path(),
        listing.total,
        names.len(),
        truncated,
    );
    Ok(NameListing {
        names,
        examined: listing.total,
        truncated,
    })
}

/// List the roots of one resolved area, bounded.
///
/// Only directories and symlinks are observed; every directory is a row whose
/// marker state says whether it is owned, unmarked or unknown. A marker is never
/// adopted, a symlink is never followed and no consumer content is read. Holds
/// and activity records come from the state directory: they are the only basis
/// for idleness, and a row whose holds or activity cannot be read is reported
/// kept rather than failing the listing.
pub fn list_roots(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    resolved: &ResolvedArea,
    max_roots: usize,
) -> Result<RootListing> {
    list_roots_with(
        dirs,
        observer,
        state,
        resolved,
        max_roots,
        ActivityMode::Floor,
    )
}

/// [`list_roots`] with an explicit activity mode.
///
/// A caller that decides a root's age asks for [`ActivityMode::Scan`], which
/// walks each root's subtree under the last-activity bounds and reports the
/// scan's own evidence on the row.
pub fn list_roots_with(
    dirs: &dyn DirOps,
    observer: &dyn ProcessObserver,
    state: &StatePaths,
    resolved: &ResolvedArea,
    max_roots: usize,
    activity: ActivityMode,
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
                IdleReason::MarkerUnverified,
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
                    IdleReason::MarkerUnverified,
                ));
                continue;
            }
        };
        let expectation = MarkerExpectation {
            volume_uuid: &resolved.volume_uuid,
            resolved_path: &configured_path,
            mount_point: Some(&resolved.mount_point),
            effective_uid,
            now_ms,
        };
        let (marker, marker_digest) = read_marker_with_digest(dirs, &root, &expectation);
        // The activity record and the holds are keyed by the root id the marker
        // proves, never by the directory's name: an adopted root's name can differ
        // from its root id, and another root's record must not decide this one.
        let root_key = match &marker {
            MarkerState::Owned(marker) => Some(marker.root_id.clone()),
            MarkerState::Absent | MarkerState::Unknown { .. } => {
                state::is_identifier(&entry.name).then(|| entry.name.clone())
            }
        };
        let record = match &root_key {
            Some(key) => match read_activity(state, observer, key) {
                Ok(activity) => activity,
                // One undecidable record never blocks the other rows: this root
                // is reported kept and the listing continues.
                Err(error) => ActivityReading::unverified(error.to_string()),
            },
            None => ActivityReading::missing(),
        };
        let holds = match &root_key {
            Some(key) => match read_holds(state, observer, key) {
                Ok(holds) => holds,
                Err(error) => {
                    roots.push(unreadable_root(
                        &entry.name,
                        &configured_path,
                        format!("the root's holds are unreadable: {error}"),
                        IdleReason::HoldUnknown,
                    ));
                    continue;
                }
            },
            None => HoldReading::empty(),
        };
        roots.push(observe_root(
            dirs,
            &entry.name,
            &root,
            &configured_path,
            entry.facts.modified_ms,
            marker,
            marker_digest,
            &holds,
            activity,
            record,
            root_key.as_deref(),
            resolved.volume_uuid.as_str(),
            IdleParticipation::of_area(resolved.declaration.reserve),
            now_ms,
        ));
    }
    Ok(RootListing {
        roots,
        examined: listing.total,
        truncated: listing.truncated,
        skipped_non_directories,
    })
}

/// One root's last-activity scan, as a listing reports it.
///
/// The age a `PrunePolicy::OldestIdleAfter` compares is `now - last_activity_ms`
/// (stage 4 design, "Last activity"). The value is the newest modification time
/// found by a bounded scan of the root's subtree, never an entry's birth time, a
/// marker's `created_ms` alone, or a cached value. A future time, an unreadable
/// subtree, a depth or entry bound and a missed deadline all leave it unknown,
/// which keeps the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootActivity {
    /// The newest modification time the scan found, or `None` when it could not
    /// decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    /// The logical bytes the scan counted: the sum of every regular file's own size.
    /// Directories and symbolic links contribute nothing here, and the root directory
    /// itself contributes no logical bytes either — only allocation.
    pub logical_bytes: u64,
    /// The bytes allocated to the entries the scan counted.
    pub allocated_bytes: u64,
    /// How many entries the scan examined, the root itself included.
    pub entries: usize,
    /// Whether the scan reached the whole subtree inside its bounds.
    pub complete: bool,
    /// Why the age is unknown, when it is. It is one of the design's reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RootActivity {
    /// A scan that could not decide the age at all.
    ///
    /// It exists for the scan itself and for a caller that could not scan (an
    /// entry that is never followed, for instance): the document then says the
    /// age is unknown instead of pretending nothing was found.
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            last_activity_ms: None,
            logical_bytes: 0,
            allocated_bytes: 0,
            entries: 0,
            complete: false,
            reason: Some(reason.into()),
        }
    }
}

/// One scan's running budget.
struct ActivityBudget {
    entries: usize,
    deadline: std::time::Instant,
}

impl ActivityBudget {
    /// The deadline's own check, so a scan that spent all its time inside one
    /// listing cannot report a complete observation afterwards.
    fn check(&self) -> std::result::Result<(), String> {
        if std::time::Instant::now() >= self.deadline {
            return Err("scanning the root did not finish inside its wall-clock bound".to_owned());
        }
        Ok(())
    }

    fn spend(&mut self) -> std::result::Result<(), String> {
        if self.entries >= ACTIVITY_ENTRIES {
            return Err(format!(
                "the root holds more than the {ACTIVITY_ENTRIES} entries this scan examines"
            ));
        }
        self.check()?;
        self.entries = self.entries.saturating_add(1);
        Ok(())
    }
}

/// Scan one opened root for its newest modification time, bounded.
///
/// The walk is descriptor-relative from the root's own handle, never follows a
/// symbolic link, and counts every entry it examines. A directory it cannot list
/// or open, a symlink (which is never followed, so what is behind it is not
/// activity Barge can see), a reached bound and a deadline all make the age
/// unknown: this function never guesses.
pub fn scan_activity(dirs: &dyn DirOps, root: &DirHandle, now_ms: u64) -> RootActivity {
    scan_activity_within(dirs, root, now_ms, ACTIVITY_WALL)
}

/// [`scan_activity`] with an explicit wall clock, so a test can prove what a scan
/// that reaches its own bound reports.
pub fn scan_activity_within(
    dirs: &dyn DirOps,
    root: &DirHandle,
    now_ms: u64,
    wall: Duration,
) -> RootActivity {
    let own = match dirs.dir_facts(root) {
        Ok(facts) => facts,
        Err(error) => {
            return RootActivity::unknown(format!(
                "the root directory's own facts cannot be read: {error}"
            ))
        }
    };
    // The floor is the root directory's own modification time, read **now** through
    // the descriptor this scan holds. An earlier observation of the same directory —
    // a listing taken before a hold was released, an effect-time re-read — is never
    // used in its place: an older timestamp would make a root that changed since look
    // old enough to delete, and a fresh one must not be masked by it. Without the
    // fresh time there is no age at all, and a child's timestamp can be arbitrarily
    // old while the directory itself was changed recently, so a missing root time is
    // never replaced by its entries' times and never read as "very old".
    let Some(floor) = own.modified_ms else {
        return RootActivity::unknown(
            "the root directory's own modification time cannot be read now, so its age cannot be floored",
        );
    };
    let mut activity = RootActivity {
        last_activity_ms: Some(floor),
        // The root directory itself has allocation but no logical bytes: logical size
        // is a regular file's own length, and a directory or a link has none.
        logical_bytes: 0,
        allocated_bytes: own.allocated_bytes,
        entries: 1,
        complete: true,
        reason: None,
    };
    let mut budget = ActivityBudget {
        entries: 1,
        deadline: std::time::Instant::now() + wall,
    };
    if let Err(reason) = walk_activity(dirs, root, 0, &mut budget, &mut activity) {
        activity.complete = false;
        activity.last_activity_ms = None;
        activity.reason = Some(reason);
        #[cfg(test)]
        crate::tape::verdict(
            "area::scan_activity_within",
            crate::tape::Identity::none(),
            "activity_incomplete",
        );
        return activity;
    }
    if activity.last_activity_ms.is_none() {
        activity.complete = false;
        activity.reason =
            Some("neither the root directory nor any entry reports a modification time".to_owned());
        return activity;
    }
    if let Some(newest) = activity.last_activity_ms {
        if newest > now_ms {
            activity.complete = false;
            activity.last_activity_ms = None;
            activity.reason = Some(format!(
                "the newest modification time {newest} is ahead of this run's clock {now_ms}"
            ));
        }
    }
    #[cfg(test)]
    crate::tape::verdict(
        "area::scan_activity_within",
        crate::tape::Identity::none(),
        &match activity.last_activity_ms {
            Some(last) => format!("activity_complete;last_ms={last}"),
            None => "activity_unavailable".to_owned(),
        },
    );
    activity
}

fn walk_activity(
    dirs: &dyn DirOps,
    handle: &DirHandle,
    depth: usize,
    budget: &mut ActivityBudget,
    activity: &mut RootActivity,
) -> std::result::Result<(), String> {
    if depth > ACTIVITY_DEPTH {
        return Err(format!(
            "the root is deeper than the {ACTIVITY_DEPTH} level bound"
        ));
    }
    budget.spend()?;
    let listing = dirs
        .list_children(handle, LISTING_NAME_ROWS)
        .map_err(|error| format!("the root's subtree cannot be listed: {error}"))?;
    // A listing can consume the whole bound, including one that answers empty: the
    // deadline is checked again before anything is concluded from it.
    budget.check()?;
    if listing.truncated {
        return Err(format!(
            "a directory in the root holds more than the {LISTING_NAME_ROWS} names one listing reads"
        ));
    }
    for entry in &listing.entries {
        budget.spend()?;
        activity.entries = activity.entries.saturating_add(1);
        // Logical size counts a regular file's own length only; a directory's or a
        // symbolic link's `size` is not data and never goes in. Allocation is counted
        // separately, for everything the scan reaches.
        if entry.facts.kind == EntryKind::File {
            activity.logical_bytes = activity.logical_bytes.saturating_add(entry.facts.size);
        }
        activity.allocated_bytes = activity
            .allocated_bytes
            .saturating_add(entry.facts.allocated_bytes);
        let Some(modified) = entry.facts.modified_ms else {
            return Err(format!(
                "the entry `{}` reports no modification time, so the root's newest activity cannot be decided",
                entry.name
            ));
        };
        let newest = activity.last_activity_ms.unwrap_or(0);
        activity.last_activity_ms = Some(newest.max(modified));
        if entry.facts.kind != EntryKind::Directory {
            // A symbolic link is never followed: what is behind it is not
            // activity Barge can see, and following it would leave the root.
            continue;
        }
        let child = dirs
            .open_child_dir(handle, &entry.name)
            .map_err(|error| format!("a directory in the root cannot be opened: {error}"))?;
        let observed = child.identity();
        let checked = crate::os::DirId {
            device: entry.facts.device,
            inode: entry.facts.inode,
        };
        if observed != checked {
            return Err(format!(
                "the directory `{}` was replaced between its reading and its open",
                entry.name
            ));
        }
        walk_activity(dirs, &child, depth + 1, budget, activity)?;
    }
    budget.check()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn observe_root(
    dirs: &dyn DirOps,
    name: &str,
    root: &DirHandle,
    configured_path: &Path,
    directory_modified_ms: Option<u64>,
    marker: MarkerState,
    marker_digest: Option<String>,
    holds: &HoldReading,
    activity: ActivityMode,
    record: ActivityReading,
    root_key: Option<&str>,
    volume_uuid: &str,
    participation: IdleParticipation,
    now_ms: u64,
) -> RootObservation {
    let space_idle = assess_idleness(&IdleInputs {
        marker: &marker,
        root_id: root_key.unwrap_or(name),
        volume_uuid,
        activity: &record,
        holds,
        participation,
        now_ms,
    });
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
        activity: None,
        idle: holds.is_idle(),
        activity_record: record.row(),
        space_idle,
        holds: holds.rows(),
        holds_truncated: holds.truncated,
    };
    match marker {
        MarkerState::Owned(marker) => {
            row.root_id = Some(marker.root_id.clone());
            row.kind = Some(marker.kind);
            row.namespace = Some(marker.area.namespace.clone());
            row.area = Some(marker.area.area.clone());
            row.recorded_path = Some(
                marker
                    .mount_at_record
                    .as_ref()
                    .zip(marker.mount_relative_path.as_ref())
                    .map_or_else(
                        || marker.resolved_path.clone().unwrap_or_default(),
                        |(mount, relative)| mount.join(relative),
                    ),
            );
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
    if activity == ActivityMode::Scan {
        let scanned = scan_activity(dirs, root, state::now_ms());
        row.last_activity_ms = scanned.last_activity_ms;
        row.activity = Some(scanned);
    }
    row
}

fn unreadable_root(
    name: &str,
    configured_path: &Path,
    reason: String,
    idle_reason: IdleReason,
) -> RootObservation {
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
        activity: None,
        // An unreadable root's marker, holds or activity are unknown, so it is
        // never reported idle under either idleness rule.
        idle: false,
        activity_record: ActivityReading::unverified(
            "the root could not be observed, so its activity is unknown".to_owned(),
        )
        .row(),
        space_idle: IdleRow::blocked(idle_reason),
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
    /// The root's activity record as it reads after the release. A root with no
    /// record keeps none: `missing` here is the `barge area idle-baseline`
    /// repair case, never an inferred release.
    pub activity: ActivityRow,
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
        assess_idleness, declare, folder_components, hold, idle_baseline, list_roots,
        list_roots_with, read_activity, read_declarations, read_holds, read_marker, release,
        resolve, scan_activity, scan_activity_within, show, validate_name, ActivityMode,
        ActivityReading, ActivityRecord, ActivityStateName, AreaDeclaration, HoldReading,
        HoldRequest, HoldScope, HoldState, IdleBaselineRequest, IdleInputs, IdleParticipation,
        IdleReason, IdleRow, IdleState, LruKey, MarkerExpectation, MarkerState, MarkerStateName,
        RootKind, RootSpec, AREA_DIRECTORY_MODE, ROOT_DIRECTORY_MODE, ROOT_MARKER_FILE,
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
        fn mount_census(&self, uuid: &VolumeUuid) -> Result<Vec<PathBuf>> {
            Ok(if self.present && &self.uuid == uuid {
                vec![self.root.clone()]
            } else {
                Vec::new()
            })
        }
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
                dead_handles: crate::os::DeadHandleEvidence::default(),
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

        fn list_names(&self, parent: &DirHandle, max_names: usize) -> Result<crate::os::DirNames> {
            self.inner.list_names(parent, max_names)
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

        fn create_child_symlink(
            &self,
            parent: &DirHandle,
            name: &str,
            target: &Path,
        ) -> Result<()> {
            self.inner.create_child_symlink(parent, name, target)
        }

        fn read_link(&self, path: &Path) -> Result<Option<PathBuf>> {
            self.inner.read_link(path)
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
                space: None,
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
                &self.state,
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

        fn list_with_activity(
            &self,
            resolved: &super::ResolvedArea,
            observer: &FakeObserver,
            activity: ActivityMode,
        ) -> super::RootListing {
            let platform = self.platform();
            list_roots_with(&platform, observer, &self.state, resolved, 4096, activity)
                .expect("the area lists")
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
                mount_point: Some(&resolved.mount_point),
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
    fn a_release_on_reference_declaration_round_trips_and_an_older_reader_refuses_it() {
        let fixture = Fixture::new("area-release-on-reference");
        let mut declaration = fixture.declaration("example/outputs");
        declaration.area = "outputs".to_owned();
        declaration.prune = super::PrunePolicy::ReleaseOnReference;
        declaration.reserve = super::ReserveParticipation::Excluded;
        let written = declare(&fixture.state, &declaration, 1_000).expect("the declaration writes");
        assert_eq!(
            written.declaration().prune,
            super::PrunePolicy::ReleaseOnReference
        );

        // The stored record carries the exact value an older reader must refuse,
        // rather than one it could read as a policy the area never declared.
        let path = super::declaration_path(&fixture.state, "example", "outputs").expect("the path");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("the record reads"))
                .expect("the record parses");
        assert_eq!(
            stored["prune"],
            serde_json::json!({"kind": "release_on_reference"})
        );
        let read_back = show(&fixture.state, "example", "outputs")
            .expect("the declarations read")
            .expect("the declaration is there");
        assert_eq!(read_back, written, "the record round-trips");
        assert_eq!(read_back.schema, "barge.area/1", "it stays inside major 1");

        /// The prune policy as it stood before `release_on_reference`.
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        #[allow(dead_code)]
        enum OlderPrunePolicy {
            None,
            OldestIdleAfter { age_seconds: u64 },
        }
        assert!(
            serde_json::from_value::<OlderPrunePolicy>(stored["prune"].clone()).is_err(),
            "an older reader refuses the value as unknown instead of approximating it"
        );
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

    /// Every path under one fixture directory, with each regular file's bytes.
    ///
    /// A mismatch refusal must leave both the state directory and the volume
    /// byte for byte as they were, so a test compares this snapshot before and
    /// after the refused call.
    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            let mut entries: Vec<_> = entries.map(|entry| entry.expect("an entry")).collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path).expect("the entry's metadata");
                let relative = path
                    .strip_prefix(base)
                    .expect("the entry is under the fixture")
                    .display()
                    .to_string();
                if metadata.is_dir() {
                    out.push((format!("{relative}/"), Vec::new()));
                    walk(base, &path, out);
                } else if metadata.is_file() {
                    out.push((relative, std::fs::read(&path).expect("the file's bytes")));
                } else {
                    out.push((relative, Vec::new()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out
    }

    /// A stored declaration whose `prune` and `reserve` values decide deletion
    /// authority: a root may be pruned by idle age and counts toward the reserve.
    fn stored_for_deletion(fixture: &Fixture) -> super::AreaDocument {
        let mut declaration = fixture.declaration("example/pool");
        declaration.prune = super::PrunePolicy::OldestIdleAfter { age_seconds: 600 };
        declaration.reserve = super::ReserveParticipation::Counts;
        declare(&fixture.state, &declaration, 1_000).expect("the stored declaration writes")
    }

    #[test]
    fn the_first_creation_stores_the_callers_declaration() {
        let fixture = Fixture::new("area-first-create");
        fixture.register(&fixture.record);
        let mut declaration = fixture.declaration("example/pool");
        declaration.prune = super::PrunePolicy::OldestIdleAfter { age_seconds: 600 };
        declaration.reserve = super::ReserveParticipation::Excluded;
        assert!(
            read_declarations(&fixture.state)
                .expect("the store reads")
                .documents
                .is_empty(),
            "the fixture starts with no declaration"
        );

        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();
        let (volume, root) = super::create_root_on_volume(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &declaration,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the first creation stores the declaration and creates the root");
        assert!(root.path.is_dir());

        // The store holds exactly the caller's declaration, generation 1.
        let stored = read_declarations(&fixture.state)
            .expect("the store reads")
            .find("example", "pool")
            .cloned()
            .expect("the first creation stored the caller's declaration");
        assert_eq!(stored.generation, 1);
        assert_eq!(stored.declaration(), declaration);
        assert_eq!(
            volume.resolved.declaration,
            super::EffectiveDeclaration::new(&stored.declaration(), "example/pool".to_owned()),
            "the effective declaration is the stored record's"
        );
        let marker = match read_owned(&fixture, &volume.resolved, &root.root_id) {
            MarkerState::Owned(marker) => marker,
            other => panic!("the marker is owned: {other:?}"),
        };
        let effective = marker
            .area
            .effective
            .as_ref()
            .expect("the effective declaration is recorded");
        assert_eq!(effective.prune, declaration.prune);
        assert_eq!(effective.reserve, declaration.reserve);

        // A second creation presenting the same declaration proceeds and writes
        // no new record: the first creation is the only write.
        let document_path = super::declaration_path(&fixture.state, "example", "pool")
            .expect("the declaration path");
        let before = std::fs::read(&document_path).expect("the document reads");
        let (_, second) = super::create_root_on_volume(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &declaration,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("a matching caller proceeds");
        assert!(second.path.is_dir());
        assert_eq!(
            std::fs::read(&document_path).expect("the document reads"),
            before,
            "a matching caller rewrites nothing"
        );
        assert_eq!(
            read_declarations(&fixture.state)
                .expect("the store reads")
                .find("example", "pool")
                .expect("the record is still there")
                .generation,
            1
        );
    }

    #[test]
    fn a_differing_caller_declaration_refuses_before_any_effect() {
        let fixture = Fixture::new("area-mismatch");
        fixture.register(&fixture.record);
        let stored = stored_for_deletion(&fixture);
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();

        // Every policy field differs, in the order a refusal names them.
        let mut caller = fixture.declaration("example/elsewhere");
        caller.overrides.insert(
            fixture.uuid.as_str().to_owned(),
            "example/overridden".to_owned(),
        );
        caller.absent = super::AbsentPolicy::Wait;
        caller.prune = super::PrunePolicy::None;
        caller.reserve = super::ReserveParticipation::Excluded;

        // Resolution refuses, and so does the creation path: both are checked
        // against one snapshot of the state directory and the volume, so a
        // refusal that created a folder, a lock, a registry change or a
        // declaration would show up here.
        let before = snapshot(fixture.dir.path());
        let error = resolve(
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.record,
            &caller,
            Some(&stored),
        )
        .expect_err("a different declaration refuses");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(
            error.detail()["fields"],
            serde_json::json!(["folder", "overrides", "absent", "prune", "reserve"]),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("the stored declaration governs"),
            "{}",
            error.message()
        );
        let error = super::create_root_on_volume(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &caller,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect_err("a different declaration refuses before any effect");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(
            error.detail()["fields"],
            serde_json::json!(["folder", "overrides", "absent", "prune", "reserve"])
        );
        assert_eq!(
            snapshot(fixture.dir.path()),
            before,
            "a refused mismatch writes, creates and deletes nothing"
        );
    }

    #[test]
    fn a_stored_prune_or_reserve_policy_is_not_overridden_by_a_caller() {
        let fixture = Fixture::new("area-deletion-authority");
        fixture.register(&fixture.record);
        // The stored record permits pruning by idle age and counts the area's
        // bytes toward the reserve.
        let stored = stored_for_deletion(&fixture);
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();

        // A caller that would exclude the area from the reserve and never let it
        // be pruned is refused on the pre-flight every deletion takes.
        let mut caller = stored.declaration();
        caller.prune = super::PrunePolicy::None;
        caller.reserve = super::ReserveParticipation::Excluded;
        let before = snapshot(fixture.dir.path());
        let error = super::prepare(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &caller,
        )
        .expect_err("the store's deletion-authority values govern");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(
            error.detail()["fields"],
            serde_json::json!(["prune", "reserve"])
        );
        assert_eq!(snapshot(fixture.dir.path()), before);

        // The stored declaration itself resolves, with the stored values.
        let volume = super::prepare(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &stored.declaration(),
        )
        .expect("the stored declaration resolves");
        assert_eq!(
            volume.resolved.declaration.prune,
            super::PrunePolicy::OldestIdleAfter { age_seconds: 600 }
        );
        assert_eq!(
            volume.resolved.declaration.reserve,
            super::ReserveParticipation::Counts
        );
    }

    #[test]
    fn a_stored_release_on_reference_policy_cannot_be_traded_for_an_age_by_a_caller() {
        let fixture = Fixture::new("area-release-on-reference-authority");
        fixture.register(&fixture.record);
        // The state directory holds the release-class policy the area declared.
        let mut declared = fixture.declaration("example/outputs");
        declared.area = "outputs".to_owned();
        declared.prune = super::PrunePolicy::ReleaseOnReference;
        declared.reserve = super::ReserveParticipation::Excluded;
        let stored = declare(&fixture.state, &declared, 1_000).expect("the declaration writes");
        assert_eq!(
            stored.declaration().prune,
            super::PrunePolicy::ReleaseOnReference
        );
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();

        // A caller that presents an age policy — the one that would let
        // `maintain` and the reserve consider the area — is refused on the
        // pre-flight every deletion takes, so a caller object can never widen
        // what the stored release-class policy allows.
        let mut caller = stored.declaration();
        caller.prune = super::PrunePolicy::OldestIdleAfter { age_seconds: 60 };
        let before = snapshot(fixture.dir.path());
        let error = super::prepare(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &caller,
        )
        .expect_err("the stored release-class policy governs");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(error.detail()["fields"], serde_json::json!(["prune"]));
        assert!(
            error.message().contains("the stored declaration governs"),
            "{}",
            error.message()
        );
        assert_eq!(snapshot(fixture.dir.path()), before);

        // The stored declaration itself resolves with the stored policy, so the
        // reading that decides deletion is the state directory's own record.
        let volume = super::prepare(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &fixture.uuid,
            &stored.declaration(),
        )
        .expect("the stored declaration resolves");
        assert_eq!(
            volume.resolved.declaration.prune,
            super::PrunePolicy::ReleaseOnReference
        );
    }

    #[test]
    fn the_first_declaration_is_written_once_and_then_matched() {
        let fixture = Fixture::new("area-ensure-declared");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let written = super::ensure_declared(&fixture.state, &declaration, 1_000)
            .expect("the first creation writes the caller's declaration");
        assert_eq!(written.generation, 1);
        assert_eq!(written.declaration(), declaration);
        let document_path = super::declaration_path(&fixture.state, "example", "pool")
            .expect("the declaration path");
        let bytes = std::fs::read(&document_path).expect("the document reads");

        // The same declaration is matched and nothing is rewritten.
        let matched = super::ensure_declared(&fixture.state, &declaration, 2_000)
            .expect("a matching declaration is matched");
        assert_eq!(matched, written);
        assert_eq!(
            std::fs::read(&document_path).expect("the document reads"),
            bytes,
            "a match writes nothing"
        );

        // A different declaration is refused and the record is untouched.
        let mut changed = declaration.clone();
        changed.prune = super::PrunePolicy::None;
        let error = super::ensure_declared(&fixture.state, &changed, 3_000)
            .expect_err("a different declaration is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(error.detail()["fields"], serde_json::json!(["prune"]));
        assert_eq!(
            std::fs::read(&document_path).expect("the document reads"),
            bytes
        );
    }

    #[test]
    fn the_effective_declaration_never_reports_a_differing_caller_policy() {
        let fixture = Fixture::new("area-effective");
        fixture.register(&fixture.record);
        let stored = stored_for_deletion(&fixture);
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
            &stored.declaration(),
            Some(&stored),
        )
        .expect("the stored declaration resolves");
        let root = super::create_root(
            &fixture.state,
            &platform,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the root is created");

        // The marker's effective declaration carries exactly the fields of the
        // governing record: the former `stored_differs` report path is gone, so
        // no report can imply that a differing caller policy was applied.
        let bytes = std::fs::read(marker_path(&root)).expect("the marker is read");
        let marker: serde_json::Value = serde_json::from_slice(&bytes).expect("the marker parses");
        let effective = marker["area"]["effective"]
            .as_object()
            .expect("the effective declaration is recorded");
        let mut keys: Vec<&str> = effective.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["absent", "area", "folder", "namespace", "prune", "reserve"]
        );
        assert_eq!(
            effective["prune"],
            serde_json::json!({"kind": "oldest_idle_after", "age_seconds": 600})
        );
        assert_eq!(effective["reserve"], serde_json::json!("counts"));
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
        let persisted = crate::identity::read(
            &fixture.state,
            &fixture.platform(),
            &root.root_id,
            fixture.observer().effective_uid().unwrap(),
        )
        .unwrap()
        .expect("identity is recorded");
        assert_eq!(persisted.nonce, root.nonce);
        assert_eq!(persisted.inode, root.identity.inode);
        assert_eq!(persisted.origin, crate::identity::IdentityOrigin::Created);
        let mut locks = LockSet::new(fixture.state.clone());
        let lock = locks.lock_root(&root.root_id).unwrap();
        assert!(crate::identity::write_once(
            &fixture.state,
            &fixture.platform(),
            &lock,
            fixture.observer().effective_uid().unwrap(),
            &persisted
        )
        .is_err());
        assert_eq!(marker.root_id, root.root_id);
        assert_eq!(marker.kind, RootKind::AreaRoot);
        assert_eq!(marker.volume_uuid, fixture.uuid);
        assert_eq!(marker.origin, super::RootOrigin::Created);
        assert_eq!(
            marker
                .mount_at_record
                .as_ref()
                .unwrap()
                .join(marker.mount_relative_path.as_ref().unwrap()),
            root.path
        );
        assert!(marker.conclusion.is_none());

        // A second creation gets a different root, never a shared one.
        let (second, _) = fixture.create_root(&fixture.record, &declaration);
        assert_ne!(second.root_id, root.root_id);
        assert_ne!(second.nonce, root.nonce);
    }

    #[test]
    fn durable_identity_verifies_the_opened_root_and_refuses_mismatches() {
        let fixture = Fixture::new("identity-verification");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let platform = fixture.platform();
        let observer = fixture.observer();
        let uid = observer.effective_uid().unwrap();
        let verify = || {
            crate::identity::verify(
                &fixture.state,
                &platform,
                &fixture.facts(),
                &fixture.mounts(),
                uid,
                &root.root_id,
            )
        };
        assert!(matches!(
            verify(),
            crate::identity::IdentityVerdict::Verified { .. }
        ));
        struct Duplicate(FakeVolume);
        impl VolumeFacts for Duplicate {
            fn identify(&self, path: &Path) -> Result<VolumeIdentity> {
                self.0.identify(path)
            }
            fn mount_census(&self, _: &VolumeUuid) -> Result<Vec<PathBuf>> {
                Ok(vec![self.0.root.clone(), self.0.root.clone()])
            }
        }
        assert_eq!(
            crate::identity::verify(
                &fixture.state,
                &platform,
                &Duplicate(fixture.facts()),
                &fixture.mounts(),
                uid,
                &root.root_id
            ),
            crate::identity::IdentityVerdict::Unknown {
                reason: crate::identity::IdentityReason::DuplicateVolumeUuid
            }
        );
        let identity_path = fixture
            .state
            .root()
            .join("roots")
            .join(&root.root_id)
            .join("identity.json");
        let identity_bytes = std::fs::read(&identity_path).unwrap();
        let mut identity: serde_json::Value = serde_json::from_slice(&identity_bytes).unwrap();
        identity["device_at_record"] = (root.identity.device.get() + 1).into();
        std::fs::write(&identity_path, serde_json::to_vec(&identity).unwrap()).unwrap();
        assert!(matches!(
            verify(),
            crate::identity::IdentityVerdict::Verified {
                remount: crate::identity::RemountEvidence { changed: true, .. },
                ..
            }
        ));
        assert_ne!(
            identity["device_at_record"].as_u64().unwrap(),
            root.identity.device.get()
        );
        identity["inode"] = (root.identity.inode + 1).into();
        std::fs::write(&identity_path, serde_json::to_vec(&identity).unwrap()).unwrap();
        assert_eq!(
            verify(),
            crate::identity::IdentityVerdict::Unknown {
                reason: crate::identity::IdentityReason::InodeChanged
            }
        );
        std::fs::write(&identity_path, identity_bytes).unwrap();
        let foreign = Adjust::new().with_foreign_owner(ROOT_MARKER_FILE);
        assert_eq!(
            crate::identity::verify(
                &fixture.state,
                &foreign,
                &fixture.facts(),
                &fixture.mounts(),
                uid,
                &root.root_id
            ),
            crate::identity::IdentityVerdict::Unknown {
                reason: crate::identity::IdentityReason::MarkerForeignOwner
            }
        );
        let marker_path = marker_path(&root);
        let bytes = std::fs::read(&marker_path).unwrap();
        rewrite_marker(&marker_path, |value| {
            value["nonce"] = "0123456789abcdef0123456789abcdef".into()
        });
        assert_eq!(
            verify(),
            crate::identity::IdentityVerdict::Unknown {
                reason: crate::identity::IdentityReason::MarkerIdentityMismatch
            }
        );
        std::fs::write(&marker_path, bytes).unwrap();
        std::fs::remove_dir_all(&root.path).unwrap();
        assert_eq!(verify(), crate::identity::IdentityVerdict::Gone);
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
                mount_point: Some(&resolved.mount_point),
                effective_uid: observer.effective_uid().expect("the uid"),
                now_ms: crate::state::now_ms(),
            },
        ) {
            MarkerState::Unknown { reason } => assert!(reason.contains("uid"), "{reason}"),
            other => panic!("a foreign-owned marker is unknown: {other:?}"),
        }
    }

    #[test]
    fn v1_remount_replaces_only_the_mount_prefix() {
        let fixture = Fixture::new("marker-remount");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let marker: super::RootMarker =
            serde_json::from_slice(&std::fs::read(marker_path(&root)).unwrap()).unwrap();
        let mut marker = marker;
        // The fixture writes v2; this test exercises the v1 prefix rule, so it
        // rebuilds the marker as the v1 shape that carries the absolute path.
        marker.schema = "barge.root/1".to_owned();
        marker.mount_relative_path = None;
        marker.mount_at_record = None;
        marker.resolved_path = Some(PathBuf::from("/Volumes/Old/area/root"));
        let identity = crate::os::VolumeIdentity {
            mount_point: PathBuf::from("/Volumes/New"),
            filesystem: crate::os::FileSystem::Apfs,
            device: crate::os::DeviceId::new(5),
            volume_uuid: Some(fixture.uuid.clone()),
            boot_volume: false,
        };
        let verified = crate::os::LocatedVolume::injected(crate::os::VerifiedVolume::new(
            PathBuf::from("/Volumes/New"),
            identity,
        ));
        let live = Path::new("/Volumes/New/area/root");
        let expectation = MarkerExpectation {
            volume_uuid: &fixture.uuid,
            resolved_path: live,
            mount_point: Some(Path::new("/Volumes/New")),
            effective_uid: 0,
            now_ms: crate::state::now_ms(),
        };
        marker
            .validate_remounted(&verified, Path::new("/Volumes/Old"), &expectation)
            .unwrap();
        let changed = MarkerExpectation {
            resolved_path: Path::new("/Volumes/New/other/root"),
            ..expectation
        };
        assert!(marker
            .validate_remounted(&verified, Path::new("/Volumes/Old"), &changed)
            .is_err());
        assert!(marker
            .validate_remounted(&verified, Path::new("/"), &expectation)
            .is_err());
        let other_uuid =
            crate::os::VolumeUuid::parse("0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0").unwrap();
        let foreign = MarkerExpectation {
            volume_uuid: &other_uuid,
            ..expectation
        };
        assert!(marker
            .validate_remounted(&verified, Path::new("/Volumes/Old"), &foreign)
            .is_err());
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
    fn v1_remains_readable_at_its_exact_recorded_path() {
        let fixture = Fixture::new("marker-v1");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let path = marker_path(&root);
        rewrite_marker(&path, |value| {
            value["schema"] = "barge.root/1".into();
            value["resolved_path"] = root.path.to_string_lossy().to_string().into();
            value.as_object_mut().unwrap().remove("mount_relative_path");
            value.as_object_mut().unwrap().remove("mount_at_record");
        });
        assert!(matches!(
            read_owned(&fixture, &resolved, &root.root_id),
            MarkerState::Owned(_)
        ));
        rewrite_marker(&path, |value| {
            value["resolved_path"] = "/different/mount/root".into()
        });
        assert!(matches!(
            read_owned(&fixture, &resolved, &root.root_id),
            MarkerState::Unknown { .. }
        ));
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
                value["schema"] = "barge.root/3".into();
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
        rewrite_marker(&path, |value| {
            value["mount_relative_path"] = root.root_id.clone().into()
        });
        assert!(
            matches!(
                read_owned(&fixture, &resolved, &root.root_id),
                MarkerState::Unknown { .. }
            ),
            "a matching suffix is not the whole path"
        );
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

        release(&fixture.state, &observer, &root.root_id, "prover").expect("the hold releases");
        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert!(reading.holds.is_empty());
        assert!(reading.is_idle(), "no live hold is idle");
        // Releasing again is not an error: it is already released.
        release(&fixture.state, &observer, &root.root_id, "prover").expect("a second release");
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
        let error = release(&fixture.state, &observer, &root.root_id, "h1")
            .expect_err("the mismatch refuses");
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

    // ---- slice T3: activity records, the idleness matrix and the LRU order ---

    fn record(root_id: &str, release: u64, created: u64, releases: u64) -> ActivityRecord {
        ActivityRecord {
            schema: crate::contract::ACTIVITY_SCHEMA.to_owned(),
            root_id: root_id.to_owned(),
            volume_uuid: UUID.to_owned(),
            last_release_ms: release,
            first_created_ms: created,
            releases,
            generation: releases,
            recorded_ms: release.max(created).max(1),
        }
    }

    fn released(root_id: &str, release: u64, created: u64) -> ActivityReading {
        ActivityReading {
            state: ActivityStateName::Released,
            reason: None,
            record: Some(record(root_id, release, created, 1)),
        }
    }

    fn owned_marker(kind: RootKind) -> MarkerState {
        let mut value = serde_json::json!({
            "schema": crate::contract::ROOT_SCHEMA,
            "root_id": "0".repeat(32),
            "nonce": "1".repeat(32),
            "volume_uuid": UUID,
            "owner": "barge",
            "kind": kind,
            "area": {"namespace": "example", "area": "pool", "effective": null},
            "created_ms": 10,
            "generation": 1,
            "origin": "created",
            "conclusion": null
        });
        if kind == RootKind::ScratchRun {
            value["conclusion"] = serde_json::json!({
                "concluded_ms": 10,
                "outcome": "closed",
                "closure": "positive",
                "kept_reason": "evidence",
                "nested_scope": "exclusive"
            });
        }
        MarkerState::Owned(Box::new(
            serde_json::from_value(value).expect("the fixture marker parses"),
        ))
    }

    /// The root id the marker fixture proves.
    const FIXTURE_ROOT: &str = "00000000000000000000000000000000";

    fn idle_inputs<'a>(
        marker: &'a MarkerState,
        activity: &'a ActivityReading,
        holds: &'a HoldReading,
        participation: IdleParticipation,
    ) -> IdleInputs<'a> {
        IdleInputs {
            marker,
            root_id: FIXTURE_ROOT,
            volume_uuid: UUID,
            activity,
            holds,
            participation,
            now_ms: 1_000,
        }
    }

    fn assert_blocked(row: IdleRow, reason: IdleReason) {
        assert_eq!(row.state, IdleState::NotIdle);
        assert_eq!(row.reason, Some(reason));
    }

    /// The identity record a created root must leave behind, whatever the activity
    /// record's fate.
    fn assert_created_root_kept(fixture: &Fixture, path: &Path, root_id: &str, error: &Error) {
        assert_eq!(error.detail()["created_root"], true);
        assert_eq!(error.detail()["root_id"], root_id);
        assert_eq!(error.detail()["path"], path.display().to_string().as_str());
        assert!(error.message().contains("was created and kept"));
        assert!(
            path.join(ROOT_MARKER_FILE).is_file(),
            "the created root keeps its marker"
        );
        let platform = fixture.platform();
        let uid = fixture.observer().effective_uid().expect("the uid");
        assert!(
            crate::identity::read(&fixture.state, &platform, root_id, uid)
                .expect("the identity reads")
                .is_some(),
            "the created root keeps its identity record"
        );
    }

    #[test]
    fn an_area_root_whose_activity_cannot_be_recorded_is_reported_and_kept() {
        // Creation writes the marker and the identity record before the activity
        // record. A failed publication therefore leaves a root Barge created, and
        // the report must name it: creation never deletes a root, and the root is
        // kept — never idle, because its activity is missing — until an operator
        // records a baseline or removes it.
        let fixture = Fixture::new("create-activity-prefail");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let platform = fixture.platform();
        let observer = fixture.observer();
        fn refuses(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity record cannot be written",
            ))
        }
        let error = super::create_root_with(
            &fixture.state,
            &platform,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
            super::ActivityIo {
                write: refuses,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the activity record cannot be recorded");
        let root_id = error.detail()["root_id"]
            .as_str()
            .expect("the report names the root")
            .to_owned();
        let path = resolved.path.join(&root_id);
        assert_created_root_kept(&fixture, &path, &root_id, &error);

        // The creation made the area folder, so the listing resolves it again.
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let listed = fixture.list(&resolved);
        let row = listed
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root_id.as_str()))
            .expect("the created root is listed");
        assert_eq!(row.activity_record.state, ActivityStateName::Missing);
        assert!(
            !row.space_idle.is_idle(),
            "a root with no activity record is never idle"
        );

        // The explicit baseline is the repair, and then the root is idle.
        idle_baseline(
            &fixture.state,
            &platform,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &IdleBaselineRequest {
                root_id: root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect("the baseline records the root's activity");
        let listed = fixture.list(&resolved);
        let row = listed
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root_id.as_str()))
            .expect("the created root is listed");
        assert_eq!(row.space_idle.state, IdleState::Idle);
    }

    #[test]
    fn an_area_root_whose_activity_publish_fails_after_its_rename_is_kept_unknown() {
        let fixture = Fixture::new("create-activity-postfail");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let platform = fixture.platform();
        let observer = fixture.observer();
        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        let error = super::create_root_with(
            &fixture.state,
            &platform,
            &observer,
            &resolved,
            RootSpec {
                kind: RootKind::AreaRoot,
            },
            super::ActivityIo {
                write: after_rename,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the activity publish fails after its rename");
        let root_id = error.detail()["root_id"]
            .as_str()
            .expect("the report names the root")
            .to_owned();
        let path = resolved.path.join(&root_id);
        assert_created_root_kept(&fixture, &path, &root_id, &error);
        assert_eq!(
            error.detail()["removed"],
            true,
            "the unconfirmed record is removed, not left readable"
        );
        assert_eq!(
            read_activity(&fixture.state, &observer, &root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Missing
        );
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let listed = fixture.list(&resolved);
        let row = listed
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root_id.as_str()))
            .expect("the created root is listed");
        assert!(!row.space_idle.is_idle());
    }

    #[test]
    fn a_feature_root_whose_activity_cannot_be_recorded_is_reported_and_kept() {
        // The second creation route: Barge's own feature folders (scratch and
        // caches). It has the same account of a failed activity publication.
        let fixture = Fixture::new("create-feature-prefail");
        fixture.register(&fixture.record);
        let platform = fixture.platform();
        let observer = fixture.observer();
        let folder = super::resolve_feature(
            &platform,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &fixture.record,
            super::Feature::Scratch,
        )
        .expect("the scratch feature resolves");
        fn refuses(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity record cannot be written",
            ))
        }
        let error = super::create_root_in_feature_with(
            &fixture.state,
            &platform,
            &observer,
            &folder,
            RootKind::ScratchRun,
            super::MarkerArea::identity("scratch", "run"),
            super::ActivityIo {
                write: refuses,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the activity record cannot be recorded");
        let root_id = error.detail()["root_id"]
            .as_str()
            .expect("the report names the root")
            .to_owned();
        let path = folder.path.join(&root_id);
        assert_created_root_kept(&fixture, &path, &root_id, &error);
        assert_eq!(
            read_activity(&fixture.state, &observer, &root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Missing
        );
    }

    #[test]
    fn a_feature_root_whose_activity_publish_fails_after_its_rename_is_kept_unknown() {
        let fixture = Fixture::new("create-feature-postfail");
        fixture.register(&fixture.record);
        let platform = fixture.platform();
        let observer = fixture.observer();
        let folder = super::resolve_feature(
            &platform,
            &fixture.facts(),
            &fixture.mounts(),
            &observer,
            &fixture.record,
            super::Feature::Scratch,
        )
        .expect("the scratch feature resolves");
        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        let error = super::create_root_in_feature_with(
            &fixture.state,
            &platform,
            &observer,
            &folder,
            RootKind::ScratchRun,
            super::MarkerArea::identity("scratch", "run"),
            super::ActivityIo {
                write: after_rename,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the activity publish fails after its rename");
        let root_id = error.detail()["root_id"]
            .as_str()
            .expect("the report names the root")
            .to_owned();
        let path = folder.path.join(&root_id);
        assert_created_root_kept(&fixture, &path, &root_id, &error);
        assert_eq!(error.detail()["removed"], true);
        assert_eq!(
            read_activity(&fixture.state, &observer, &root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Missing
        );
    }

    #[test]
    fn a_new_root_records_activity_at_generation_zero_with_no_release() {
        let fixture = Fixture::new("activity-creation");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        assert!(path.is_file(), "creation writes the activity record");
        let mode = std::fs::metadata(&path)
            .expect("the record has metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "an activity record is private");
        let reading =
            read_activity(&fixture.state, &fixture.observer(), &root.root_id).expect("it reads");
        assert_eq!(reading.state, ActivityStateName::Unreleased);
        let record = reading.record.expect("the record parsed");
        assert_eq!(record.schema, "barge.activity/1");
        assert_eq!(record.root_id, root.root_id);
        assert_eq!(record.volume_uuid, UUID);
        assert_eq!(record.last_release_ms, 0);
        assert_eq!(record.releases, 0);
        assert_eq!(record.generation, 0);
        assert_eq!(record.first_created_ms, root.created_ms);
        // A root that was created and never released is reported at the volume
        // root's own listing, where its bytes must be *kept*.
        let listing = fixture.list(&resolved);
        let row = &listing.roots[0];
        assert_eq!(row.activity_record.state, ActivityStateName::Unreleased);
        assert!(
            row.idle,
            "stage 1's hold-only idleness is untouched by the activity record"
        );
        assert_blocked(row.space_idle, IdleReason::ActivityUnreleased);
    }

    #[test]
    fn a_release_records_one_release_fact_and_an_absent_hold_records_nothing_new() {
        let fixture = Fixture::new("activity-release");
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
        hold(&fixture.state, &observer, &request).expect("the hold is written");
        let listed = fixture.list(&resolved);
        assert_blocked(listed.roots[0].space_idle, IdleReason::HoldLive);

        let released =
            release(&fixture.state, &observer, &root.root_id, "prover").expect("the hold releases");
        assert!(released.released);
        assert_eq!(released.activity.state, ActivityStateName::Released);
        let first = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(first.releases, 1);
        assert_eq!(first.generation, 1);
        assert!(first.last_release_ms > 0);
        assert_eq!(first.first_created_ms, root.created_ms);

        // Releasing again records nothing new: the hold is already absent.
        let again = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect("a second release is not an error");
        assert!(!again.released);
        let after = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(after.generation, first.generation);
        assert_eq!(after.releases, first.releases);

        // A second hold and release is a second fact.
        hold(&fixture.state, &observer, &request).expect("the hold is written again");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("it releases");
        let twice = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(twice.releases, 2);
        assert_eq!(twice.generation, 2);
        let listed = fixture.list(&resolved);
        assert_eq!(listed.roots[0].space_idle.state, IdleState::Idle);
    }

    #[test]
    fn a_release_whose_unlink_fails_records_no_release() {
        let fixture = Fixture::new("release-unlink-fails");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let before = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");

        // The hold file cannot be removed, so the release never happens. The
        // activity record must not claim it did.
        let hold_directory = fixture.state.holds_dir().join(&root.root_id);
        std::fs::set_permissions(&hold_directory, std::fs::Permissions::from_mode(0o500))
            .expect("the hold directory is made read-only");
        let error = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect_err("the unlink fails");
        std::fs::set_permissions(&hold_directory, std::fs::Permissions::from_mode(0o700))
            .expect("the hold directory is writable again");
        assert_eq!(error.kind(), ErrorKind::Io);
        let after = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(after, before, "a failed release records nothing");
        assert_eq!(after.releases, 0);
        assert_eq!(after.generation, 0);
        assert!(
            hold_directory.join("prover.json").is_file(),
            "the hold is still there"
        );
        let listed = fixture.list(&resolved);
        assert_blocked(listed.roots[0].space_idle, IdleReason::HoldLive);

        // The ordinary retry then records exactly one release.
        let released =
            release(&fixture.state, &observer, &root.root_id, "prover").expect("the release works");
        assert!(released.released);
        let after = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(after.releases, 1);
        assert_eq!(after.generation, 1);
    }

    #[test]
    fn a_release_that_cannot_be_recorded_changes_nothing() {
        let fixture = Fixture::new("release-write-fails");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let before = std::fs::read(&path).expect("the record reads");

        // The fact cannot be written, so the release refuses before the effect:
        // the hold stays and nothing is recorded.
        let activity_directory = fixture.state.activity_dir();
        std::fs::set_permissions(&activity_directory, std::fs::Permissions::from_mode(0o500))
            .expect("the activity directory is made read-only");
        let error = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect_err("the activity write fails");
        std::fs::set_permissions(&activity_directory, std::fs::Permissions::from_mode(0o700))
            .expect("the activity directory is writable again");
        // The write failed before its rename, so the record is untouched and the
        // report says exactly that.
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(error.detail()["restored"], true);
        assert!(error
            .message()
            .contains("was refused and the activity record is unchanged"));
        assert!(
            fixture
                .state
                .holds_dir()
                .join(&root.root_id)
                .join("prover.json")
                .is_file(),
            "the hold was not removed"
        );
        assert_eq!(
            std::fs::read(&path).expect("the record reads"),
            before,
            "a release that cannot be recorded records nothing"
        );
        assert_blocked(
            fixture.list(&resolved).roots[0].space_idle,
            IdleReason::HoldLive,
        );

        // The ordinary retry then releases and records exactly one fact.
        let released =
            release(&fixture.state, &observer, &root.root_id, "prover").expect("the release works");
        assert!(released.released);
        assert_eq!(released.activity.record.expect("the record").releases, 1);
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_failed_release_never_leaves_an_earlier_release_trusted_as_current() {
        // The record already holds an earlier release. If the *next* release is
        // recorded and its effect fails, the earlier record has to come back
        // exactly: a completed-hold-less root must never be ordered on an old
        // release time.
        let fixture = Fixture::new("release-earlier");
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
        hold(&fixture.state, &observer, &request).expect("the first hold is written");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the first release");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let earlier = std::fs::read(&path).expect("the record reads");
        let earlier_record = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(earlier_record.releases, 1);
        let key = LruKey::of(&earlier_record).expect("a released record");

        // A second release whose effect fails.
        hold(&fixture.state, &observer, &request).expect("the second hold is written");
        let hold_directory = fixture.state.holds_dir().join(&root.root_id);
        std::fs::set_permissions(&hold_directory, std::fs::Permissions::from_mode(0o500))
            .expect("the hold directory is made read-only");
        let error = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect_err("the hold cannot be removed");
        std::fs::set_permissions(&hold_directory, std::fs::Permissions::from_mode(0o700))
            .expect("the hold directory is writable again");
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(error.detail()["restored"], true);
        assert_eq!(
            std::fs::read(&path).expect("the record reads"),
            earlier,
            "the previous record is restored byte for byte"
        );
        assert_eq!(
            LruKey::of(
                &read_activity(&fixture.state, &observer, &root.root_id)
                    .expect("the record reads")
                    .record
                    .expect("the record parsed")
            )
            .expect("a released record"),
            key,
            "the order still uses the last completed release"
        );
        // The hold is still there, so the root is kept on the hold, not on the
        // leftover record.
        assert_blocked(
            fixture.list(&resolved).roots[0].space_idle,
            IdleReason::HoldLive,
        );

        // The retry completes and records exactly one new release.
        let released =
            release(&fixture.state, &observer, &root.root_id, "prover").expect("the retry works");
        assert_eq!(released.activity.record.expect("the record").releases, 2);
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
        assert!(!hold_directory.join("prover.json").exists());
    }

    #[test]
    fn an_interrupted_release_leaves_a_hold_never_an_older_release() {
        // The ordering's invariant, exercised as the crash window it allows: a
        // release that recorded its fact but has not removed the hold. The record
        // is the current release (never an older one), and the root is kept on
        // the hold while the holder is live.
        //
        // This test also pins what happens once that leftover hold goes stale,
        // which is the window this slice cannot close with the frozen record: the
        // hold stops blocking, so the published fact decides and the root reads
        // idle on the release that never removed its hold. That is an open design
        // question for the design delegate (Q-011), not a claimed safety
        // property, and the assertions below are what must change when it is
        // answered.
        let fixture = Fixture::new("release-interrupted");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let held = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(held.releases, 0);

        // Exactly what the release does first: record the fact. Then stop, as an
        // interruption would.
        let current = super::released_record(&held);
        super::publish_fact(
            &fixture.state,
            &current,
            None,
            super::ActivityIo::real(),
            "the release",
        )
        .expect("the fact is written");
        let current = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(current.releases, 1);
        assert!(current.last_release_ms > 0);

        let listed = fixture.list(&resolved);
        assert_eq!(
            listed.roots[0].activity_record.state,
            ActivityStateName::Released
        );
        assert_blocked(listed.roots[0].space_idle, IdleReason::HoldLive);
        assert!(
            LruKey::of(&held).is_none(),
            "the interrupted release is the record's current fact, never an older one"
        );
        assert!(LruKey::of(&current).is_some());

        // The leftover hold goes stale: its holder is positively gone, which stage
        // 1 defines as not blocking idleness. The published fact then decides.
        let hold_path = fixture
            .state
            .holds_dir()
            .join(&root.root_id)
            .join("prover.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hold_path).expect("the hold reads"))
                .expect("the hold parses");
        document["boot_identity"] = "1600000000.0".into();
        std::fs::write(
            &hold_path,
            serde_json::to_vec_pretty(&document).expect("the hold encodes"),
        )
        .expect("the hold is written");
        std::fs::set_permissions(&hold_path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");
        let stale = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(stale.holds[0].1, HoldState::Stale);
        assert!(stale.is_idle(), "a stale hold does not block stage 1");
        let listed = fixture.list(&resolved);
        assert_eq!(
            listed.roots[0].space_idle.state,
            IdleState::Idle,
            "Q-011: an interrupted release whose hold went stale is accepted as idle on the published fact"
        );
        assert!(
            listed.roots[0].activity_record.last_release_ms == Some(current.last_release_ms),
            "the order still uses the published release, never an older one"
        );

        // Settling the hold the ordinary way completes the same fact once.
        let released = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect("the release completes");
        assert_eq!(released.activity.record.expect("the record").releases, 2);
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_publish_that_fails_after_its_rename_restores_the_record() {
        // `state::publish_atomic` renames and then syncs the directory, so its
        // failure does not always mean the new bytes are absent. A release whose
        // fact landed but whose directory sync failed must not report that it
        // changed nothing, and must not leave the advanced record behind: the
        // previous record comes back, and the hold is untouched.
        let fixture = Fixture::new("release-post-rename");
        fixture.register(&fixture.record);
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
        hold(&fixture.state, &observer, &request).expect("the first hold is written");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the first release");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let earlier = std::fs::read(&path).expect("the record reads");
        hold(&fixture.state, &observer, &request).expect("the second hold is written");

        // The publish lands the new bytes and then fails, exactly as a directory
        // sync failure after the rename does.
        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                write: after_rename,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the publish fails after its rename");
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(error.detail()["restored"], true);
        assert_eq!(
            std::fs::read(&path).expect("the record reads"),
            earlier,
            "the advanced record is rolled back, so the report and the record agree"
        );
        assert!(
            fixture
                .state
                .holds_dir()
                .join(&root.root_id)
                .join("prover.json")
                .is_file(),
            "the hold was not removed"
        );
        assert_blocked(
            fixture.list(&resolved).roots[0].space_idle,
            IdleReason::HoldLive,
        );

        // The retry releases and records exactly one new fact.
        let released =
            release(&fixture.state, &observer, &root.root_id, "prover").expect("the retry works");
        assert_eq!(released.activity.record.expect("the record").releases, 2);
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_post_rename_failure_whose_restore_fails_is_reported_as_unsettled() {
        // The baseline below needs a registered volume, as every mutating path does.
        // The same post-rename failure, but the rollback fails too: the advanced
        // record stays visible and is not confirmed durable, so the release
        // reports unsettled state and the hold is what keeps the root. Once the
        // holder is positively gone the stale hold stops blocking and the
        // published time decides (decision c9d30b41), which is what the report
        // told the caller.
        let fixture = Fixture::new("release-post-rename-unsettled");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        fn restore_fails(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(
                ErrorKind::Io,
                "injected: the previous record cannot be restored",
            ))
        }
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                write: after_rename,
                restore: restore_fails,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("both the publish and its rollback fail");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["restored"], false);
        assert_eq!(error.detail()["removed"], true);
        assert!(error.message().contains("was not confirmed durable"));
        assert!(
            error.message().contains("never idle"),
            "the report says the removed record keeps the root: {}",
            error.message()
        );
        assert!(
            !path.exists(),
            "the unconfirmed record is removed rather than trusted"
        );
        assert!(
            fixture
                .state
                .holds_dir()
                .join(&root.root_id)
                .join("prover.json")
                .is_file(),
            "the hold was not removed"
        );

        // An unconfirmed publication is never idleness, whether the leftover
        // holder is live or gone: the record is absent, so the matrix answers
        // activity unknown.
        let listed = fixture.list(&resolved);
        assert_eq!(
            listed.roots[0].activity_record.state,
            ActivityStateName::Missing
        );
        assert_blocked(listed.roots[0].space_idle, IdleReason::HoldLive);
        let hold_path = fixture
            .state
            .holds_dir()
            .join(&root.root_id)
            .join("prover.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hold_path).expect("the hold reads"))
                .expect("the hold parses");
        document["boot_identity"] = "1600000000.0".into();
        std::fs::write(
            &hold_path,
            serde_json::to_vec_pretty(&document).expect("the hold encodes"),
        )
        .expect("the hold is written");
        std::fs::set_permissions(&hold_path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");
        let listed = fixture.list(&resolved);
        assert_blocked(listed.roots[0].space_idle, IdleReason::ActivityUnknown);
        assert!(
            !listed.roots[0].space_idle.is_idle(),
            "a stale holder does not make an unconfirmed publication idle"
        );

        // The repair chain: release the leftover hold (which records nothing,
        // because the record is absent), then the explicit baseline.
        let released = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect("the stale hold is removable");
        assert!(released.released);
        assert_eq!(released.activity.state, ActivityStateName::Missing);
        assert!(
            !fixture.list(&resolved).roots[0].space_idle.is_idle(),
            "a missing record is never idle, even with no hold left"
        );
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect("the baseline repairs the record");
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_double_rollback_failure_invalidates_the_unconfirmed_record() {
        // The publish landed unconfirmed, the previous record cannot be restored
        // and the record cannot be removed. It is invalidated in place: an
        // invalid record is activity unknown under O-013, so it can never decide
        // idleness or order a root, whether the leftover holder is live or gone.
        let fixture = Fixture::new("release-double-rollback");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        fn always_fails(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(ErrorKind::Io, "injected: rollback fails"))
        }
        fn cannot_remove(_path: &Path) -> Result<()> {
            Err(Error::new(ErrorKind::Io, "injected: removal fails"))
        }
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                write: after_rename,
                restore: always_fails,
                remove: cannot_remove,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the publish, its rollback and the removal fail");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["restored"], false);
        assert_eq!(error.detail()["removed"], false);
        assert_eq!(error.detail()["invalidated"], true);
        assert!(error.message().contains("invalidated in place"));
        assert!(path.is_file(), "the record is still there, but invalid");
        assert_eq!(
            read_activity(&fixture.state, &observer, &root.root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Unverified,
            "the unconfirmed record is made unreadable, never a release"
        );

        // A live holder keeps the root, and a stale one does not make the
        // invalidated record idle either.
        let listed = fixture.list(&resolved);
        assert_eq!(
            listed.roots[0].activity_record.state,
            ActivityStateName::Unverified
        );
        assert_blocked(listed.roots[0].space_idle, IdleReason::HoldLive);
        let hold_path = fixture
            .state
            .holds_dir()
            .join(&root.root_id)
            .join("prover.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hold_path).expect("the hold reads"))
                .expect("the hold parses");
        document["boot_identity"] = "1600000000.0".into();
        std::fs::write(
            &hold_path,
            serde_json::to_vec_pretty(&document).expect("the hold encodes"),
        )
        .expect("the hold is written");
        std::fs::set_permissions(&hold_path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");
        let stale = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(stale.holds[0].1, HoldState::Stale);
        let listed = fixture.list(&resolved);
        assert_blocked(listed.roots[0].space_idle, IdleReason::ActivityUnknown);
        assert_eq!(
            listed.roots[0].activity_record.last_release_ms, None,
            "the invalidated record offers no release time to the order"
        );

        // The repair chain: the leftover hold releases (recording nothing,
        // because the record is invalid), then the explicit baseline records a
        // fresh fact and the root becomes idle.
        let released = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect("the stale hold is removable");
        assert_eq!(released.activity.state, ActivityStateName::Unverified);
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect("the baseline repairs the record");
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_verification_read_that_fails_does_not_skip_recovery() {
        // The publish lands unconfirmed and the read that would verify it cannot
        // complete. Unknown is never taken as "nothing happened", so the previous
        // record is restored even though the verification read failed.
        let fixture = Fixture::new("release-read-fails");
        fixture.register(&fixture.record);
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
        hold(&fixture.state, &observer, &request).expect("the first hold is written");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the first release");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let earlier = std::fs::read(&path).expect("the record reads");
        hold(&fixture.state, &observer, &request).expect("the second hold is written");

        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        fn unreadable(
            _state: &crate::state::StatePaths,
            _root_id: &str,
        ) -> Result<Option<Vec<u8>>> {
            Err(Error::new(
                ErrorKind::Io,
                "injected: the verification read fails",
            ))
        }
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                write: after_rename,
                read: unreadable,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the publish fails after its rename");
        assert_eq!(error.detail()["restored"], true);
        assert!(
            error.message().contains("was restored"),
            "the recovery ran even though the verification read failed: {}",
            error.message()
        );
        assert_eq!(
            std::fs::read(&path).expect("the record reads"),
            earlier,
            "the previous record was restored byte for byte"
        );
        assert!(
            fixture
                .state
                .holds_dir()
                .join(&root.root_id)
                .join("prover.json")
                .is_file(),
            "the hold was not removed"
        );
        assert_blocked(
            fixture.list(&resolved).roots[0].space_idle,
            IdleReason::HoldLive,
        );

        // The same failed read with a failing restore removes the unconfirmed
        // record, so it cannot decide idleness either.
        fn fails_to_restore(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(ErrorKind::Io, "injected: rollback fails"))
        }
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                write: after_rename,
                restore: fails_to_restore,
                read: unreadable,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the publish and its rollback fail");
        assert_eq!(error.detail()["restored"], false);
        assert_eq!(error.detail()["removed"], true);
        assert_eq!(
            read_activity(&fixture.state, &observer, &root.root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Missing
        );
        let listed = fixture.list(&resolved);
        assert!(!listed.roots[0].space_idle.is_idle());
        assert!(
            listed.roots[0].activity_record.last_release_ms.is_none(),
            "a removed record offers no release time to the order"
        );

        // The leftover hold releases (recording nothing, because the record is
        // absent) and the explicit baseline repairs it.
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the hold releases");
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect("the baseline repairs the record");
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_state_volume_that_refuses_every_write_cannot_be_marked() {
        // The residual no frozen-format step can close: the publish landed
        // unconfirmed and the record can be neither restored, removed nor
        // invalidated. The report says a store that refuses every write cannot be
        // marked and that the hold is the only thing keeping the root; a live
        // holder does keep it.
        let fixture = Fixture::new("release-triple-rollback");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        fn always_fails(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(ErrorKind::Io, "injected: rollback fails"))
        }
        fn always_fails_too(_path: &Path) -> Result<()> {
            Err(Error::new(
                ErrorKind::Io,
                "injected: the state volume refuses",
            ))
        }
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                write: after_rename,
                restore: always_fails,
                remove: always_fails_too,
                invalidate: always_fails_too,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("nothing can be written");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["restored"], false);
        assert_eq!(error.detail()["removed"], false);
        assert_eq!(error.detail()["invalidated"], false);
        assert!(error
            .message()
            .contains("refuses every write cannot be marked"));
        assert_blocked(
            fixture.list(&resolved).roots[0].space_idle,
            IdleReason::HoldLive,
        );

        // What the residual leaves, pinned so the delegate's answer is a visible
        // change (Q-012): the unconfirmed record is still readable, so once the
        // leftover hold goes stale the matrix reads it as a release. This is the
        // one state the frozen `barge.activity/1` cannot mark, and the report
        // above tells the caller so; it is not a claimed safety property.
        let hold_path = fixture
            .state
            .holds_dir()
            .join(&root.root_id)
            .join("prover.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hold_path).expect("the hold reads"))
                .expect("the hold parses");
        document["boot_identity"] = "1600000000.0".into();
        std::fs::write(
            &hold_path,
            serde_json::to_vec_pretty(&document).expect("the hold encodes"),
        )
        .expect("the hold is written");
        std::fs::set_permissions(&hold_path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");
        let listed = fixture.list(&resolved);
        assert_eq!(
            listed.roots[0].activity_record.state,
            ActivityStateName::Released,
            "Q-012: a state volume that refuses every write leaves the unconfirmed record readable"
        );
        assert!(
            listed.roots[0].space_idle.is_idle(),
            "Q-012 pins today's residual outcome for the delegate, not a claimed safety property"
        );
    }

    #[test]
    fn a_failed_restore_is_reported_and_leaves_the_hold_standing() {
        let fixture = Fixture::new("release-restore-fails");
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
        hold(&fixture.state, &observer, &request).expect("the first hold is written");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the first release");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let earlier = std::fs::read(&path).expect("the record reads");

        // A second release records its fact, cannot remove the hold, and cannot
        // put the previous record back either. The failure is reported as
        // unsettled and the hold is still there, so the root is kept on the hold.
        hold(&fixture.state, &observer, &request).expect("the second hold is written");
        let hold_directory = fixture.state.holds_dir().join(&root.root_id);
        std::fs::set_permissions(&hold_directory, std::fs::Permissions::from_mode(0o500))
            .expect("the hold directory is made read-only");
        let error = super::release_with(
            &fixture.state,
            &observer,
            &root.root_id,
            "prover",
            super::ActivityIo {
                restore: |_path: &Path, _bytes: &[u8]| {
                    Err(Error::new(
                        ErrorKind::Io,
                        "injected: the previous record cannot be restored",
                    ))
                },
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the restore fails");
        std::fs::set_permissions(&hold_directory, std::fs::Permissions::from_mode(0o700))
            .expect("the hold directory is writable again");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["restored"], false);
        assert!(error
            .message()
            .contains("kept on the hold and not on the record"));
        assert_eq!(
            error.detail()["hold_path"],
            hold_directory
                .join("prover.json")
                .display()
                .to_string()
                .as_str()
        );
        assert!(
            hold_directory.join("prover.json").is_file(),
            "the hold was not removed"
        );
        assert_ne!(
            std::fs::read(&path).expect("the record reads"),
            earlier,
            "the new fact stayed published, which is why the report is unsettled"
        );
        let published = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_blocked(
            fixture.list(&resolved).roots[0].space_idle,
            IdleReason::HoldLive,
        );

        // The holder exits: its hold is stale, which stage 1 defines as not
        // blocking idleness, so the published fact decides and the LRU order uses
        // its time (decision c9d30b41).
        let hold_path = hold_directory.join("prover.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&hold_path).expect("the hold reads"))
                .expect("the hold parses");
        document["boot_identity"] = "1600000000.0".into();
        std::fs::write(
            &hold_path,
            serde_json::to_vec_pretty(&document).expect("the hold encodes"),
        )
        .expect("the hold is written");
        std::fs::set_permissions(&hold_path, std::fs::Permissions::from_mode(0o600))
            .expect("the hold is private");
        let stale = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(stale.holds[0].1, HoldState::Stale);
        let listed = fixture.list(&resolved);
        assert_eq!(listed.roots[0].space_idle.state, IdleState::Idle);
        assert_eq!(
            listed.roots[0].activity_record.last_release_ms,
            Some(published.last_release_ms),
            "the order uses the published release time, not the restored one"
        );
        assert!(LruKey::of(&published).is_some());

        // The next release settles the leftover hold, and the root is
        // unambiguous again.
        let released = release(&fixture.state, &observer, &root.root_id, "prover")
            .expect("the retry completes the release");
        assert_eq!(released.activity.record.expect("the record").releases, 3);
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
        assert!(!hold_path.exists());
    }

    #[test]
    fn the_idleness_matrix_binds_evidence_to_the_root_the_marker_proves() {
        let owned = owned_marker(RootKind::AreaRoot);
        let activity = released(FIXTURE_ROOT, 500, 100);
        let holds = HoldReading::empty();
        assert_eq!(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &holds,
                IdleParticipation::Counts
            ))
            .state,
            IdleState::Idle
        );

        // Evidence read under another key is not this root's evidence.
        assert_blocked(
            assess_idleness(&IdleInputs {
                marker: &owned,
                root_id: &"1".repeat(32),
                volume_uuid: UUID,
                activity: &activity,
                holds: &holds,
                participation: IdleParticipation::Counts,
                now_ms: 1_000,
            }),
            IdleReason::IdentityMismatch,
        );
        // A record written on another volume is not this root's release.
        assert_blocked(
            assess_idleness(&IdleInputs {
                marker: &owned,
                root_id: FIXTURE_ROOT,
                volume_uuid: OTHER_UUID,
                activity: &activity,
                holds: &holds,
                participation: IdleParticipation::Counts,
                now_ms: 1_000,
            }),
            IdleReason::IdentityMismatch,
        );
        // A record that names another root is never this root's release.
        let foreign = released(&"2".repeat(32), 500, 100);
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &foreign,
                &holds,
                IdleParticipation::Counts,
            )),
            IdleReason::IdentityMismatch,
        );
    }

    #[test]
    fn a_listing_reads_activity_and_holds_by_the_markers_root_id() {
        let fixture = Fixture::new("activity-binding");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        rewrite_marker(&path, |value| {
            value["volume_uuid"] = OTHER_UUID.into();
        });
        let listed = fixture.list(&resolved);
        let row = &listed.roots[0];
        assert_eq!(
            row.activity_record.state,
            ActivityStateName::Unreleased,
            "the record itself still reads"
        );
        assert_blocked(row.space_idle, IdleReason::ActivityUnreleased);

        // A record that records a release on another volume is not this root's.
        let released_record = record(&root.root_id, 1, 1, 1);
        let mut document = serde_json::to_value(&released_record).expect("the record encodes");
        document["volume_uuid"] = OTHER_UUID.into();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&document).expect("the record encodes"),
        )
        .expect("the record is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the record is private");
        let listed = fixture.list(&resolved);
        let row = &listed.roots[0];
        assert_eq!(row.activity_record.state, ActivityStateName::Released);
        assert!(
            row.idle,
            "stage 1's hold-only idleness still reads the holds only"
        );
        assert_blocked(row.space_idle, IdleReason::IdentityMismatch);

        // Another root's record under this root's key does not decide this root.
        let other = record(&"3".repeat(32), 1, 1, 1);
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&other).expect("the record encodes"),
        )
        .expect("the record is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the record is private");
        let listed = fixture.list(&resolved);
        assert_blocked(listed.roots[0].space_idle, IdleReason::ActivityUnknown);
        assert!(!observed_idle(&fixture, &resolved, &root.root_id));
    }

    /// Whether a listing reports the root idle under the pressure matrix.
    fn observed_idle(fixture: &Fixture, resolved: &super::ResolvedArea, root_id: &str) -> bool {
        fixture
            .list(resolved)
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root_id))
            .is_some_and(|row| row.space_idle.is_idle())
    }

    #[test]
    fn a_baseline_whose_publish_fails_after_its_rename_leaves_no_idle_fact() {
        // The baseline's own fact can fail after its rename too. It is not
        // trusted either: the previous record cannot be restored, so the
        // unconfirmed baseline is invalidated in place and the root stays
        // activity unknown — never idle — until an operator settles it.
        let fixture = Fixture::new("baseline-post-rename");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let before = std::fs::read(&path).expect("the record reads");

        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        fn restore_fails(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(
                ErrorKind::Io,
                "injected: the previous record cannot be restored",
            ))
        }
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let error = super::idle_baseline_with(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
            super::ActivityIo {
                write: after_rename,
                restore: restore_fails,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the baseline publish fails after its rename");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["restored"], false);
        assert_eq!(error.detail()["removed"], true);
        assert!(error.message().contains("could not be recorded"));
        assert!(
            !path.exists(),
            "the unconfirmed baseline was removed, not left readable (it was {} bytes before)",
            before.len()
        );
        assert_eq!(
            read_activity(&fixture.state, &observer, &root.root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Missing,
            "the unconfirmed baseline was removed"
        );
        assert!(
            !fixture.list(&resolved).roots[0].space_idle.is_idle(),
            "a failed baseline never makes a root idle"
        );

        // The explicit command run again records the baseline and the root
        // becomes idle.
        idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect("the retry records the baseline");
        assert_eq!(
            fixture.list(&resolved).roots[0].space_idle.state,
            IdleState::Idle
        );
    }

    #[test]
    fn a_baseline_whose_rollback_and_removal_fail_is_invalidated_in_place() {
        // The same baseline failure, but the previous record cannot be restored
        // and the unconfirmed record cannot be removed: it is invalidated in
        // place, so it can never be read as an idle fact.
        let fixture = Fixture::new("baseline-triple");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");

        fn after_rename(target: &Path, bytes: &[u8]) -> Result<()> {
            crate::state::publish_atomic(target, bytes)?;
            Err(Error::new(
                ErrorKind::Io,
                "injected: the activity directory sync failed after the rename",
            ))
        }
        fn fails(_target: &Path, _bytes: &[u8]) -> Result<()> {
            Err(Error::new(ErrorKind::Io, "injected: rollback fails"))
        }
        fn fails_too(_path: &Path) -> Result<()> {
            Err(Error::new(ErrorKind::Io, "injected: removal fails"))
        }
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let error = super::idle_baseline_with(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
            super::ActivityIo {
                write: after_rename,
                restore: fails,
                remove: fails_too,
                ..super::ActivityIo::real()
            },
        )
        .expect_err("the baseline cannot be recorded");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(error.detail()["invalidated"], true);
        assert!(path.is_file(), "the record is still there, but invalid");
        let reading =
            read_activity(&fixture.state, &observer, &root.root_id).expect("the record reads");
        assert_eq!(reading.state, ActivityStateName::Unverified);
        assert!(
            !fixture.list(&resolved).roots[0].space_idle.is_idle(),
            "an invalidated baseline is never an idle fact"
        );
    }

    #[test]
    fn an_idle_baseline_refuses_any_present_hold_including_a_stale_one() {
        let fixture = Fixture::new("baseline-stale-hold");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        // A hold written under another boot is positively stale: stage 1 treats
        // it as not blocking idleness, and the design still refuses a baseline
        // while the hold file exists.
        let mut previous_boot = fixture.observer();
        previous_boot.boot = "1600000000.0".to_owned();
        let held = hold(
            &fixture.state,
            &previous_boot,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "previous-boot".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a holder from before the reboot".to_owned(),
            },
        )
        .expect("the hold is written");
        let reading = read_holds(&fixture.state, &observer, &root.root_id).expect("the holds read");
        assert_eq!(reading.holds[0].1, HoldState::Stale);
        assert!(reading.is_idle(), "a stale hold does not block stage 1");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        std::fs::remove_file(&path).expect("the record is lost");

        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect_err("a present stale hold refuses the baseline");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert_eq!(error.detail()["hold_state"], "stale");
        assert_eq!(error.detail()["holder"], "previous-boot");
        assert!(!path.exists(), "a refused baseline writes no record");

        // Releasing the hold is the operator's step, and then the baseline works.
        release(&fixture.state, &observer, &root.root_id, "previous-boot")
            .expect("the stale hold releases");
        assert!(!held.path.exists());
        idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect("the baseline writes once no hold exists");
        assert!(path.is_file());
    }

    #[test]
    fn a_release_never_invents_an_activity_record() {
        let fixture = Fixture::new("activity-no-invention");
        fixture.register(&fixture.record);
        let root_id = "0".repeat(32);
        let observer = fixture.observer();
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a hold on a root Barge has no record of".to_owned(),
            },
        )
        .expect("the hold is written");
        let released =
            release(&fixture.state, &observer, &root_id, "prover").expect("the hold releases");
        assert!(released.released);
        assert_eq!(released.activity.state, ActivityStateName::Missing);
        assert!(
            !fixture
                .state
                .activity_path(&root_id)
                .expect("the path")
                .exists(),
            "a release of an unrecorded root writes no activity record"
        );
    }

    #[test]
    fn an_activity_record_that_does_not_verify_is_unknown_and_never_idle() {
        let fixture = Fixture::new("activity-invalid");
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        let valid = std::fs::read(&path).expect("the record reads");
        for edit in [
            |value: &mut serde_json::Value| value["schema"] = "barge.activity/2".into(),
            |value: &mut serde_json::Value| value["root_id"] = "1".repeat(32).into(),
            |value: &mut serde_json::Value| value["volume_uuid"] = "not-a-uuid".into(),
            |value: &mut serde_json::Value| value["releases"] = 3.into(),
            |value: &mut serde_json::Value| value["first_created_ms"] = u64::MAX.into(),
            |value: &mut serde_json::Value| value["unknown_field"] = true.into(),
        ] {
            let mut document: serde_json::Value =
                serde_json::from_slice(&valid).expect("the record parses");
            edit(&mut document);
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&document).expect("the record encodes"),
            )
            .expect("the record is written");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("the record is private");
            let reading =
                read_activity(&fixture.state, &observer, &root.root_id).expect("the record reads");
            assert_eq!(reading.state, ActivityStateName::Unverified, "{document}");
            let listed = fixture.list(&resolved);
            assert_blocked(listed.roots[0].space_idle, IdleReason::ActivityUnknown);
        }
        // A record the platform cannot read is unknown too, never a release.
        std::fs::write(&path, b"not json").expect("the record is overwritten");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the record is private");
        assert_eq!(
            read_activity(&fixture.state, &observer, &root.root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Unverified
        );
        // A record with a mode that is not private is not trusted.
        std::fs::write(&path, &valid).expect("the record is restored");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("the record is world readable");
        assert_eq!(
            read_activity(&fixture.state, &observer, &root.root_id)
                .expect("the record reads")
                .state,
            ActivityStateName::Unverified
        );
        // A missing file is missing, and the root stays kept.
        std::fs::remove_file(&path).expect("the record is removed");
        let reading =
            read_activity(&fixture.state, &observer, &root.root_id).expect("the record reads");
        assert_eq!(reading.state, ActivityStateName::Missing);
        let listed = fixture.list(&resolved);
        assert_blocked(listed.roots[0].space_idle, IdleReason::ActivityUnknown);
    }

    #[test]
    fn the_idleness_matrix_keeps_every_root_that_is_not_positively_idle() {
        let live = HoldReading {
            holds: vec![(
                super::HoldRecord {
                    schema: crate::contract::HOLD_SCHEMA.to_owned(),
                    root_id: "0".repeat(32),
                    holder: "prover".to_owned(),
                    pid: 7,
                    process_start: 7,
                    boot_identity: BOOT.to_owned(),
                    scope: HoldScope::Write,
                    purpose: "a fixture holder".to_owned(),
                    held_ms: 5,
                },
                HoldState::Live,
                None,
            )],
            total: 1,
            truncated: false,
        };
        let mut stale = live.clone();
        stale.holds[0].1 = HoldState::Stale;
        let mut undecidable = live.clone();
        undecidable.holds[0].1 = HoldState::Unknown;
        let mut truncated = HoldReading::empty();
        truncated.truncated = true;

        let owned = owned_marker(RootKind::AreaRoot);
        let absent = MarkerState::Absent;
        let unknown = MarkerState::Unknown {
            reason: "the marker is foreign".to_owned(),
        };
        let activity = released(&"0".repeat(32), 500, 100);
        let unreleased = ActivityReading {
            state: ActivityStateName::Unreleased,
            record: Some(record(&"0".repeat(32), 0, 100, 0)),
            reason: None,
        };
        let future = released(&"0".repeat(32), 5_000, 100);
        let mut future_own_write = activity.clone();
        if let Some(record) = future_own_write.record.as_mut() {
            record.recorded_ms = 5_000;
        }

        assert_eq!(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &HoldReading::empty(),
                IdleParticipation::Counts
            ))
            .state,
            IdleState::Idle
        );
        assert_eq!(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &stale,
                IdleParticipation::Scratch
            ))
            .state,
            IdleState::Idle,
            "a stale hold does not keep a root busy"
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &absent,
                &activity,
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::MarkerUnverified,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &unknown,
                &activity,
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::MarkerUnverified,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &ActivityReading::missing(),
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::ActivityUnknown,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &ActivityReading::unverified("unreadable"),
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::ActivityUnknown,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &unreleased,
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::ActivityUnreleased,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &future,
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::ActivityFuture,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &future_own_write,
                &HoldReading::empty(),
                IdleParticipation::Counts,
            )),
            IdleReason::ActivityFuture,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &live,
                IdleParticipation::Counts,
            )),
            IdleReason::HoldLive,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &undecidable,
                IdleParticipation::Counts,
            )),
            IdleReason::HoldUnknown,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &truncated,
                IdleParticipation::Counts,
            )),
            IdleReason::HoldUnknown,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &HoldReading::empty(),
                IdleParticipation::Excluded,
            )),
            IdleReason::ParticipationExcluded,
        );
        assert_blocked(
            assess_idleness(&idle_inputs(
                &owned,
                &activity,
                &HoldReading::empty(),
                IdleParticipation::Undeclared,
            )),
            IdleReason::ParticipationUndeclared,
        );
        // A scratch root is pressure-eligible only when it is positively
        // concluded: the pressure predicate relaxes the age, never the four
        // facts the conclusion records.
        let concluded = owned_marker(RootKind::ScratchRun);
        assert_eq!(
            assess_idleness(&idle_inputs(
                &concluded,
                &activity,
                &HoldReading::empty(),
                IdleParticipation::Scratch
            ))
            .state,
            IdleState::Idle,
            "a positively concluded scratch root is pressure-eligible"
        );
        for conclusion in [
            None,
            Some(super::RootConclusion {
                concluded_ms: 900,
                outcome: super::ConclusionOutcome::Retained,
                closure: super::ConclusionClosure::Positive,
                kept_reason: super::KeptReason::Evidence,
                nested_scope: super::NestedScope::Exclusive,
            }),
            Some(super::RootConclusion {
                concluded_ms: 900,
                outcome: super::ConclusionOutcome::Closed,
                closure: super::ConclusionClosure::None,
                kept_reason: super::KeptReason::Evidence,
                nested_scope: super::NestedScope::Exclusive,
            }),
            Some(super::RootConclusion {
                concluded_ms: 900,
                outcome: super::ConclusionOutcome::Closed,
                closure: super::ConclusionClosure::Positive,
                kept_reason: super::KeptReason::Evidence,
                nested_scope: super::NestedScope::Hosting,
            }),
            Some(super::RootConclusion {
                concluded_ms: 900,
                outcome: super::ConclusionOutcome::Closed,
                closure: super::ConclusionClosure::Positive,
                kept_reason: super::KeptReason::Exception,
                nested_scope: super::NestedScope::Exclusive,
            }),
        ] {
            let MarkerState::Owned(mut marker) = owned_marker(RootKind::ScratchRun) else {
                unreachable!("the fixture marker is owned")
            };
            marker.conclusion = conclusion;
            let kept = MarkerState::Owned(marker);
            assert_blocked(
                assess_idleness(&idle_inputs(
                    &kept,
                    &activity,
                    &HoldReading::empty(),
                    IdleParticipation::Scratch,
                )),
                IdleReason::UnconcludedScratch,
            );
        }
    }

    #[test]
    fn the_lru_order_is_release_then_creation_then_root_id_across_areas() {
        // Two "areas" are just roots here: the order is volume-wide, so no
        // candidate carries its area into the comparison.
        let older_release = LruKey::of(&record(&"a".repeat(32), 100, 50, 1)).expect("a release");
        let newer_release = LruKey::of(&record(&"b".repeat(32), 200, 10, 1)).expect("a release");
        assert!(
            older_release < newer_release,
            "the older release goes first"
        );
        assert!(LruKey::of(&record(&"c".repeat(32), 0, 10, 0)).is_none());

        let mut keys = [
            LruKey::of(&record(&"f".repeat(32), 300, 20, 1)).expect("a release"),
            LruKey::of(&record(&"e".repeat(32), 300, 10, 1)).expect("a release"),
            LruKey::of(&record(&"d".repeat(32), 300, 10, 1)).expect("a release"),
            LruKey::of(&record(&"9".repeat(32), 300, 0, 1)).expect("a release"),
            LruKey::of(&record(&"0".repeat(32), 100, 99, 1)).expect("a release"),
        ];
        keys.sort();
        let order: Vec<String> = keys.iter().map(|key| key.root_id.clone()).collect();
        assert_eq!(
            order,
            vec![
                "0".repeat(32),
                "d".repeat(32),
                "e".repeat(32),
                "f".repeat(32),
                "9".repeat(32),
            ],
            "older release, then older creation, then root id; an unrecorded creation is last"
        );
        keys.reverse();
        assert_eq!(
            keys.iter().map(|key| key.root_id.clone()).next(),
            Some("9".repeat(32)),
            "the order is total and reverses cleanly"
        );
    }

    #[test]
    fn an_idle_baseline_repairs_a_lost_record_and_refuses_a_held_root() {
        let fixture = Fixture::new("activity-baseline");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, resolved) = fixture.create_root(&fixture.record, &declaration);
        let observer = fixture.observer();
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        std::fs::remove_file(&path).expect("the record is lost");

        // A live hold refuses, and nothing is written.
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let request = IdleBaselineRequest {
            root_id: root.root_id.clone(),
            volume_uuid: None,
            mount_relative_path: None,
        };
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect_err("a held root is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(!path.exists(), "a refused baseline writes no record");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the hold releases");

        // An undecidable hold refuses as custody not proven.
        let held = hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "undecidable".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&held.path).expect("the hold reads"))
                .expect("the hold parses");
        document["pid"] = 4242.into();
        std::fs::write(
            &held.path,
            serde_json::to_vec_pretty(&document).expect("the hold encodes"),
        )
        .expect("the hold is written");
        let mut undecidable = fixture.observer();
        undecidable.unreadable.insert(4242);
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &undecidable,
            &request,
        )
        .expect_err("an undecidable hold is refused");
        assert_eq!(error.kind(), ErrorKind::CustodyNotProven);
        std::fs::remove_file(&held.path).expect("the fixture hold is removed");
        std::fs::remove_dir(fixture.state.holds_dir().join(&root.root_id))
            .expect("the fixture hold directory is removed");

        // The repair writes a fresh record, resolved from the durable identity.
        let report = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect("the baseline writes");
        assert_eq!(report.root_id, root.root_id);
        assert_eq!(report.path, root.path);
        assert_eq!(report.activity.state, ActivityStateName::Released);
        assert_eq!(report.activity.releases, Some(0));
        assert_eq!(report.activity.generation, Some(1));
        assert_eq!(report.activity.first_created_ms, Some(root.created_ms));
        let written = read_activity(&fixture.state, &observer, &root.root_id)
            .expect("the record reads")
            .record
            .expect("the record parsed");
        assert_eq!(written.volume_uuid, UUID);
        assert!(written.last_release_ms > 0);
        let listed = fixture.list(&resolved);
        assert_eq!(listed.roots[0].space_idle.state, IdleState::Idle);

        // A second baseline keeps the counted releases and bumps the generation.
        hold(
            &fixture.state,
            &observer,
            &HoldRequest {
                root_id: root.root_id.clone(),
                holder: "prover".to_owned(),
                pid: std::process::id(),
                scope: HoldScope::Write,
                purpose: "a fixture holder".to_owned(),
            },
        )
        .expect("the hold is written");
        release(&fixture.state, &observer, &root.root_id, "prover").expect("the hold releases");
        let reported = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &request,
        )
        .expect("a second baseline writes");
        assert_eq!(reported.activity.releases, Some(1));
        // generation 1 was the first baseline, 2 the release, 3 this baseline.
        assert_eq!(reported.activity.generation, Some(3));
    }

    #[test]
    fn an_idle_baseline_refuses_what_it_cannot_verify() {
        let fixture = Fixture::new("activity-baseline-refusals");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();

        // A root Barge has no record of.
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: "1".repeat(32),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect_err("an unknown root is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);

        // Half an explicit location is a usage refusal.
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: Some(fixture.uuid.clone()),
                mount_relative_path: None,
            },
        )
        .expect_err("half a location is refused");
        assert_eq!(error.kind(), ErrorKind::Usage);

        // The explicit location must be the root the marker names.
        let other = "2".repeat(32);
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: other.clone(),
                volume_uuid: Some(fixture.uuid.clone()),
                mount_relative_path: Some(format!("example/pool/{}", root.root_id)),
            },
        )
        .expect_err("a marker that names another root is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);
        assert!(
            !fixture
                .state
                .activity_path(&other)
                .expect("the path")
                .exists(),
            "a refused baseline writes nothing"
        );

        // A marker that does not verify is refused before custody.
        rewrite_marker(&marker_path(&root), |value| {
            value["resolved_path"] = "/somewhere/else".into();
        });
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: Some(fixture.uuid.clone()),
                mount_relative_path: Some(format!("example/pool/{}", root.root_id)),
            },
        )
        .expect_err("an unverified marker is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedBoundary);

        // An explicit location that disagrees with Barge's durable identity
        // record is refused rather than resolved.
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: Some(fixture.uuid.clone()),
                mount_relative_path: Some(format!("example/pool/{}", "3".repeat(32))),
            },
        )
        .expect_err("a location that disagrees with the identity record is refused");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert_eq!(
            error.detail()["record_path"],
            format!("example/pool/{}", root.root_id).as_str()
        );

        // An explicit path outside the volume is refused by containment.
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: Some(fixture.uuid.clone()),
                mount_relative_path: Some("../escape".to_owned()),
            },
        )
        .expect_err("a path outside the volume is refused");
        assert_eq!(error.kind(), ErrorKind::RefusedContainment);

        // An absent volume is never reconstructed from the record.
        let absent = fixture.absent_facts();
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &absent,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect_err("an absent volume is refused");
        assert_eq!(error.kind(), ErrorKind::VolumeAbsent);
    }

    #[test]
    fn an_idle_baseline_is_refused_by_an_invalid_registry_and_writes_nothing() {
        let fixture = Fixture::new("activity-baseline-registry");
        let declaration = fixture.declaration("example/pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        fixture.state.initialize().expect("the layout exists");
        std::fs::write(fixture.state.registry_path(), b"{ not a registry")
            .expect("the invalid registry is written");
        let path = fixture
            .state
            .activity_path(&root.root_id)
            .expect("the activity path");
        std::fs::remove_file(&path).expect("the record is lost");
        let platform = fixture.platform();
        let facts = fixture.facts();
        let mounts = fixture.mounts();
        let observer = fixture.observer();
        let error = idle_baseline(
            &fixture.state,
            &platform,
            &facts,
            &mounts,
            &observer,
            &IdleBaselineRequest {
                root_id: root.root_id.clone(),
                volume_uuid: None,
                mount_relative_path: None,
            },
        )
        .expect_err("an invalid registry refuses");
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(!path.exists(), "the refusal wrote no record");
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
            fn list_names(
                &self,
                parent: &DirHandle,
                max_names: usize,
            ) -> Result<crate::os::DirNames> {
                self.inner.list_names(parent, max_names)
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

            fn create_child_symlink(
                &self,
                parent: &DirHandle,
                name: &str,
                target: &Path,
            ) -> Result<()> {
                self.inner.create_child_symlink(parent, name, target)
            }
            fn read_link(&self, path: &Path) -> Result<Option<PathBuf>> {
                self.inner.read_link(path)
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
            &fixture.state,
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
            &fixture.state,
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
            &fixture.state,
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
            RootSpec {
                kind: RootKind::AreaRoot,
            },
        )
        .expect("the present volume creates the root");
        assert_eq!(volume.record.volume_uuid, fixture.uuid);
        assert!(root.path.is_dir());
        assert!(lock.exists(), "a successful creation takes the volume lock");
    }

    #[test]
    fn a_root_without_its_own_modification_time_is_never_idle() {
        // The root directory's own modification time is the floor, and it is the
        // only fact that bounds its age: its entries' times can be arbitrarily old
        // while the directory itself changed recently. A platform that reports no
        // directory time must therefore leave the age unknown, never fall back to
        // a child's timestamp. `barge maintain`'s area policy asks this same
        // listing to scan, so this is that path's regression too.
        let fixture = Fixture::new("activity-no-root-time");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let artifact = root.path.join("target/debug/artifact");
        std::fs::create_dir_all(artifact.parent().expect("the payload parent"))
            .expect("the payload folder is created");
        std::fs::write(&artifact, b"bytes").expect("the payload is written");
        // An entry whose own modification time is decades old: with the root's
        // time hidden, that is exactly the replacement the scan must refuse.
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(&artifact)
            .expect("the payload opens")
            .set_modified(old)
            .expect("the payload's time is set");

        let resolved = fixture.resolved(&fixture.record, &declaration);
        let observer = fixture.observer();
        let dirs = crate::testkit::Faults::default();
        dirs.hiding_directory_modified_times();
        let listing = list_roots_with(
            &dirs,
            &observer,
            &fixture.state,
            &resolved,
            4096,
            ActivityMode::Scan,
        )
        .expect("the area lists");
        let row = listing
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root.root_id.as_str()))
            .expect("the root is listed");
        assert_eq!(
            row.last_activity_ms, None,
            "a child's old time never replaces the root's own missing one"
        );
        let activity = row
            .activity
            .as_ref()
            .expect("the scan reports its evidence");
        assert!(!activity.complete);
        assert!(
            activity
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("own modification time cannot be read now")),
            "{activity:?}"
        );
    }

    #[test]
    fn logical_bytes_are_regular_file_sizes_only() {
        // Logical size is a regular file's own length. A plain directory has allocation
        // but no logical bytes, a nested file adds exactly its own size, and a symbolic
        // link adds nothing.
        let fixture = crate::testkit::TempDir::new("activity-bytes");
        let dirs = crate::testkit::Faults::default();
        let handle = dirs.open_path(fixture.path()).expect("the directory opens");
        let empty = scan_activity(&dirs, &handle, crate::state::now_ms());
        assert!(empty.complete, "{empty:?}");
        assert_eq!(
            empty.logical_bytes, 0,
            "an empty directory has no logical bytes"
        );

        let nested = fixture.join("target/debug");
        std::fs::create_dir_all(&nested).expect("the nested folders");
        std::fs::write(nested.join("artifact"), vec![0u8; 1234]).expect("the file is written");
        std::os::unix::fs::symlink(nested.join("artifact"), fixture.join("link"))
            .expect("the link is created");
        let scanned = scan_activity(&dirs, &handle, crate::state::now_ms());
        assert!(scanned.complete, "{scanned:?}");
        assert_eq!(
            scanned.logical_bytes, 1234,
            "only the regular file's own size is logical: {scanned:?}"
        );
        assert!(
            scanned.allocated_bytes > empty.allocated_bytes,
            "the directories and the file add allocation: {scanned:?}"
        );
    }

    #[test]
    fn a_stale_listing_time_never_stands_in_for_the_roots_own() {
        // The floor is the freshly opened descriptor's modification time. An earlier
        // listing of the same directory says ten days ago while the directory itself
        // says now: the scan must use now, so a root that changed since that listing
        // is not reported as old.
        let fixture = Fixture::new("activity-stale-listing");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let dirs = crate::testkit::Faults::default();
        let now = crate::state::now_ms();
        dirs.listing_the_modified_time_as(now.saturating_sub(10 * 86_400_000));
        let handle = dirs.open_path(&root.path).expect("the root opens");
        let scanned = scan_activity(&dirs, &handle, now);
        let last = scanned.last_activity_ms.expect("the fresh time floors it");
        assert!(
            last > now.saturating_sub(60_000),
            "the scan used the stale listing time {last}, not the directory's own"
        );
        assert!(scanned.complete);

        // Through the listing a deciding caller uses, the same root is not idle past
        // the declared 86 400 s even though the listing says ten days have passed.
        let resolved = fixture.resolved(&fixture.record, &declaration);
        let observer = fixture.observer();
        let listing = list_roots_with(
            &dirs,
            &observer,
            &fixture.state,
            &resolved,
            4096,
            ActivityMode::Scan,
        )
        .expect("the area lists");
        let row = listing
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root.root_id.as_str()))
            .expect("the root is listed");
        let last = row.last_activity_ms.expect("a fresh activity time");
        assert!(last > now.saturating_sub(60_000), "{row:?}");
    }

    #[test]
    fn a_scan_that_reaches_its_bound_is_never_complete() {
        // An empty listing can consume the whole bound: the scan must then report
        // that it did not finish, never a complete observation with an old time.
        let fixture = Fixture::new("activity-bound");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let dirs = crate::testkit::Faults::default();
        // The pause is longer than the wall clock this scan is given.
        dirs.sleeping_before_listing(Duration::from_millis(250));
        let handle = dirs.open_path(&root.path).expect("the root opens");
        let scanned = scan_activity_within(
            &dirs,
            &handle,
            crate::state::now_ms(),
            Duration::from_millis(100),
        );
        assert_eq!(
            scanned.last_activity_ms, None,
            "a scan past its bound reported an activity time"
        );
        assert!(!scanned.complete);
        assert!(
            scanned
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("wall-clock bound")),
            "{scanned:?}"
        );

        // The same root with a bound it can meet is complete.
        let dirs = crate::testkit::Faults::default();
        let handle = dirs.open_path(&root.path).expect("the root opens");
        let scanned = scan_activity(&dirs, &handle, crate::state::now_ms());
        assert!(scanned.complete, "{scanned:?}");
        assert!(scanned.last_activity_ms.is_some());
    }

    #[test]
    fn a_root_without_modification_times_is_never_idle() {
        // The scan decides an age both delete paths compare against. A platform
        // that reports no modification time at all must leave that age unknown:
        // reading it as zero would make a live root look decades idle, here and on
        // `barge maintain`'s area policy, which asks the same listing to scan.
        let fixture = Fixture::new("activity-no-times");
        fixture.register(&fixture.record);
        let declaration = fixture.declaration("pool");
        let (root, _) = fixture.create_root(&fixture.record, &declaration);
        let artifact = root.path.join("target/debug/artifact");
        std::fs::create_dir_all(artifact.parent().expect("the payload parent"))
            .expect("the payload folder is created");
        std::fs::write(&artifact, b"bytes").expect("the payload is written");

        let resolved = fixture.resolved(&fixture.record, &declaration);
        let observer = fixture.observer();
        let scanned = fixture.list_with_activity(&resolved, &observer, ActivityMode::Scan);
        let row = scanned
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root.root_id.as_str()))
            .expect("the root is listed");
        assert!(
            row.last_activity_ms.is_some(),
            "the real platform reports modification times"
        );

        let dirs = crate::testkit::Faults::default();
        dirs.hiding_modified_times();
        let listing = list_roots_with(
            &dirs,
            &observer,
            &fixture.state,
            &resolved,
            4096,
            ActivityMode::Scan,
        )
        .expect("the area lists");
        let row = listing
            .roots
            .iter()
            .find(|row| row.root_id.as_deref() == Some(root.root_id.as_str()))
            .expect("the root is listed");
        assert_eq!(
            row.last_activity_ms, None,
            "a missing modification time is unknown, never zero"
        );
        let activity = row
            .activity
            .as_ref()
            .expect("the scan reports its evidence");
        assert!(!activity.complete);
        assert!(
            activity
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("modification time")),
            "{activity:?}"
        );
    }
}

#[cfg(test)]
mod mount_lookup_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::cell::Cell;
    use std::path::PathBuf;

    use super::{MountLookup, VolumeRoot};
    use crate::error::{Error, ErrorKind, Result};
    use crate::os::{MountPoint, VolumeLocator, VolumeUuid};
    use crate::testkit::{FakeVolume, VolumeFixture};

    /// The fixture volume, as the platform would report it at `root`.
    fn volume_at(fixture: &VolumeFixture, root: PathBuf) -> FakeVolume {
        FakeVolume {
            root,
            uuid: fixture.uuid.clone(),
            device: fixture.device,
            present: Cell::new(true),
            shifted: Cell::new(false),
            duplicate: Cell::new(false),
        }
    }

    /// A locator whose mount table cannot be read.
    struct UnreadableMounts;

    impl VolumeLocator for UnreadableMounts {
        fn mounts_for_uuid(&self, _uuid: &VolumeUuid) -> Result<Vec<MountPoint>> {
            Err(Error::new(
                ErrorKind::UnknownState,
                "the mount table is unreadable",
            ))
        }
    }

    #[test]
    fn a_remounted_volume_resolves_by_its_uuid_while_the_stored_point_refuses() {
        let fixture = VolumeFixture::new("mount-lookup-moved");
        let moved = fixture.dir.join("moved");
        std::fs::rename(&fixture.root, &moved).expect("the mount moves to a new name");
        let live = volume_at(&fixture, moved.clone());
        let dirs = fixture.platform();

        // The record still names the old point, which no longer holds the volume.
        let found = VolumeRoot::open_with(
            &dirs,
            &live,
            &live,
            MountLookup::ByUuid(&live),
            &fixture.record,
        )
        .expect("the one live mount with the UUID is the volume");
        assert_eq!(found.mount_point(), moved.as_path());

        let stored = VolumeRoot::open(&dirs, &live, &live, &fixture.record)
            .expect_err("the stored point no longer holds the volume");
        assert_eq!(stored.kind(), ErrorKind::VolumeMismatch);
    }

    #[test]
    fn a_stored_point_is_never_trusted_when_the_census_finds_the_volume_elsewhere() {
        let fixture = VolumeFixture::new("mount-lookup-occupied");
        let occupant = fixture.dir.join("occupant");
        std::fs::create_dir(&occupant).expect("the occupied point is created");
        let mut record = fixture.record.clone();
        record.mount_hint = Some(occupant.clone());
        let live = fixture.facts();
        let dirs = fixture.platform();

        let found = VolumeRoot::open_with(&dirs, &live, &live, MountLookup::ByUuid(&live), &record)
            .expect("the census finds the registered volume at its live mount");
        assert_eq!(found.mount_point(), fixture.root.as_path());

        VolumeRoot::open(&dirs, &live, &live, &record)
            .expect_err("the stored point is occupied by something else and is not trusted");
    }

    #[test]
    fn no_live_mount_is_volume_absent_and_a_mounted_twice_uuid_is_unknown() {
        let fixture = VolumeFixture::new("mount-lookup-absent");
        let live = fixture.facts();
        let dirs = fixture.platform();

        live.present.set(false);
        let absent = VolumeRoot::open_with(
            &dirs,
            &live,
            &live,
            MountLookup::ByUuid(&live),
            &fixture.record,
        )
        .expect_err("no live mount with the UUID is absent");
        assert_eq!(absent.kind(), ErrorKind::VolumeAbsent);

        live.present.set(true);
        live.duplicate_mounts();
        let duplicate = VolumeRoot::open_with(
            &dirs,
            &live,
            &live,
            MountLookup::ByUuid(&live),
            &fixture.record,
        )
        .expect_err("two live mounts with the UUID are never chosen between");
        assert_eq!(duplicate.kind(), ErrorKind::UnknownState);
        assert_eq!(duplicate.detail()["reason"], "duplicate_volume_uuid");
    }

    #[test]
    fn an_unreadable_mount_table_keeps_the_volume_unknown_and_is_not_absence() {
        let fixture = VolumeFixture::new("mount-lookup-unreadable");
        let live = fixture.facts();
        let dirs = fixture.platform();

        let unknown = VolumeRoot::open_with(
            &dirs,
            &live,
            &live,
            MountLookup::ByUuid(&UnreadableMounts),
            &fixture.record,
        )
        .expect_err("an unreadable table is unknown, not an absent volume");
        assert_eq!(unknown.kind(), ErrorKind::UnknownState);
    }
}
