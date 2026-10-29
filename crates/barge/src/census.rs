//! Oxide's output-registry census, and the drift report that answers the
//! carried "device or inode changed" finding (stage 4 design, slice F5; open
//! design issue O4-006).
//!
//! `barge inspect` already reads Oxide's state in place and reports
//! observations. This module adds the one bounded, read-only observation the
//! finding needs: Oxide's output registry
//! (`<internal pool>/outputs/registry.json`, `oxide.outputs.registry/2` and `/1`)
//! is read strictly, and every row is reported with the facts Oxide recorded
//! and — where the recorded volume is present and verifiable — Barge's own
//! identity verdict for that root, from the same descriptor walk a root
//! verification takes (stage 4 design, "Verifying a root").
//!
//! The point is the one distinction Oxide's own reason text conflates. A
//! changed **device** is normal after a remount: it is reported as
//! `device_equal: false` with the verdict still `verified`. A changed **inode**
//! means the directory is not the one that was recorded, so the root is kept as
//! `unknown(inode_changed)`. A row whose volume is away is
//! `unknown(volume_unverified)`: never skipped, never called absent, and never
//! counted as an empty registry. The same rule holds one level up: a registry
//! document without its `rows` member is divergent, and a pools record that
//! cannot be read makes the census's own source unknown, so neither is ever
//! reported as an empty or absent registry.
//!
//! Nothing here is authority. No row becomes a Barge record, nothing is
//! written, and the report is used by `inspect` alone. The read boundary this
//! widens is O4-006, and it is retired when Oxide adopts its area.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::contract::RowSet;
use crate::error::{Error, ErrorKind, Result};
use crate::identity::{IdentityReason, RemountEvidence};
use crate::inspect::{Observation, ObservationState, Observed, OxidePools};
use crate::os::{
    DirId, DirOps, EntryKind, ExpectedVolume, MountPrefix, MountRoot, VolumeFacts, VolumeUuid,
};
use crate::state::{Bounds, Deadline};

/// The observation source for Oxide's output registry.
pub const OXIDE_REGISTRY_SOURCE: &str = "oxide.outputs.registry";
/// Oxide's output registry inside one internal pool.
pub const OXIDE_REGISTRY_FILE: &str = "registry.json";
/// Oxide's output directory inside one internal pool.
pub const OXIDE_OUTPUTS_DIR: &str = "outputs";
/// The ownership marker inside every Oxide output root.
pub const OXIDE_ROOT_MARKER_FILE: &str = ".oxide-output.json";
/// Oxide's own registry size cap, which Barge keeps rather than inventing one.
pub const OXIDE_REGISTRY_SIZE_CAP_BYTES: usize = 4 * 1024 * 1024;
/// Oxide's own registry row cap.
pub const OXIDE_REGISTRY_ROWS: usize = 4096;
/// Oxide's own owner-marker cap: one bounded read per root path.
pub const OXIDE_ROOT_MARKER_SIZE_CAP_BYTES: usize = 64 * 1024;
/// Oxide's current owner-marker version, which requires a nonce.
const OXIDE_ROOT_MARKER_V2: &str = "oxide.outputs.root/2";
/// Oxide's previous owner-marker version, which predates the nonce.
const OXIDE_ROOT_MARKER_V1: &str = "oxide.outputs.root/1";
/// The one deadline the whole census shares with the registry read.
pub const OXIDE_CENSUS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// The most components a recorded root path is walked through.
pub const OXIDE_CENSUS_DEPTH: usize = 64;
/// The most distinct unknown reasons one drift report counts.
pub const OXIDE_CENSUS_REASONS: usize = 16;

/// Which real root an Oxide registry row names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OxideRootKind {
    /// A root on a registered external volume.
    External,
    /// A checkout-local fallback root.
    Local,
}

/// Oxide's output registry, as Oxide writes it (`deny_unknown_fields`).
///
/// Only the document's own rows are interpreted. `intents` and
/// `cancelled_overflow` are accepted because they are members of the document's
/// field set, and they stay whatever stage 1 called them: presence only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideRegistry {
    version: String,
    schema: u64,
    revision: u64,
    complete: bool,
    incomplete: Option<OxideRegistryIncomplete>,
    created_by: String,
    rows: Vec<OxideRegistryRow>,
}

impl OxideRegistry {
    /// The version the document states.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The document's numeric schema.
    pub fn schema(&self) -> u64 {
        self.schema
    }

    /// The revision Oxide advanced before its last change.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether Oxide says the document is complete.
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// Why the document is incomplete, when it says so.
    pub fn incomplete(&self) -> Option<&OxideRegistryIncomplete> {
        self.incomplete.as_ref()
    }

    /// The transaction that created the document.
    pub fn created_by(&self) -> &str {
        &self.created_by
    }

    /// Every row the document holds, in file order.
    pub fn rows(&self) -> &[OxideRegistryRow] {
        &self.rows
    }
}

/// Oxide's own record of why its registry is incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideRegistryIncomplete {
    reason: String,
    since_ms: u64,
    transaction: String,
}

impl OxideRegistryIncomplete {
    /// Oxide's reason text, bounded by the registry read.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// When the document became incomplete.
    pub fn since_ms(&self) -> u64 {
        self.since_ms
    }

    /// The transaction that was interrupted.
    pub fn transaction(&self) -> &str {
        &self.transaction
    }
}

/// One live reference in Oxide's registry, with the facts Oxide recorded for it.
///
/// Nothing is interpreted here: an absent value stays absent, and a value of the
/// right type that cannot be used is reported as such rather than repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideRegistryRow {
    checkout_id: String,
    checkout: PathBuf,
    checkout_volume_uuid: Option<String>,
    receipt: PathBuf,
    member: String,
    root: PathBuf,
    root_kind: OxideRootKind,
    volume_uuid: Option<String>,
    recipe: String,
    nonce: Option<String>,
    generation: u64,
    root_dev: Option<u64>,
    root_ino: Option<u64>,
    marker_digest: Option<String>,
    selected: bool,
    pending: bool,
    pruning: Option<String>,
    pruned_ms: Option<u64>,
    reservation: Option<String>,
    cancelling: Option<String>,
}

impl OxideRegistryRow {
    /// The checkout the root was created for.
    pub fn checkout(&self) -> &Path {
        &self.checkout
    }

    /// The root path Oxide recorded, exactly as recorded.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether this row names a checkout-local fallback root.
    pub fn root_kind(&self) -> OxideRootKind {
        self.root_kind
    }

    /// The recorded volume UUID, when the row carries one.
    pub fn volume_uuid(&self) -> Option<&str> {
        self.volume_uuid.as_deref()
    }

    /// The root's immutable nonce, when the row carries one.
    pub fn nonce(&self) -> Option<&str> {
        self.nonce.as_deref()
    }

    /// The root creation generation Oxide recorded.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The device Oxide recorded when it last committed this selection.
    pub fn root_dev(&self) -> Option<u64> {
        self.root_dev
    }

    /// The inode Oxide recorded when it last committed this selection.
    pub fn root_ino(&self) -> Option<u64> {
        self.root_ino
    }

    /// The owner-marker digest Oxide recorded, when it recorded one.
    pub fn marker_digest(&self) -> Option<&str> {
        self.marker_digest.as_deref()
    }

    /// Whether this reference is a live selection.
    pub fn selected(&self) -> bool {
        self.selected
    }

    /// Whether this reference is a pending selection.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// The prune intent that references this root, when one does.
    pub fn pruning(&self) -> Option<&str> {
        self.pruning.as_deref()
    }

    /// When Oxide pruned this root, for a history row.
    pub fn pruned_ms(&self) -> Option<u64> {
        self.pruned_ms
    }

    /// The receipt reservation transaction that created this row, when one did.
    pub fn reservation(&self) -> Option<&str> {
        self.reservation.as_deref()
    }

    /// Whether an unused reservation's cancellation began.
    pub fn cancelling(&self) -> Option<&str> {
        self.cancelling.as_deref()
    }

    /// Whether the row still references its root at all.
    pub fn live(&self) -> bool {
        self.selected
            || self.pending
            || self.pruning.is_some()
            || self.reservation.is_some()
            || self.cancelling.is_some()
    }
}

/// The output registry as one observation, with every path that was looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OxideRegistryObservation {
    /// What was read, absent, or unknown, naming where it looked.
    pub observation: Observation<OxideRegistry>,
    /// Every registry path that was looked at, in order.
    pub searched: Vec<PathBuf>,
}

/// Look for Oxide's output registry in the candidate internal pools.
///
/// The first candidate that is present decides: a registry that is present but
/// unreadable, oversized or of an unknown version is unknown state, and Barge
/// never looks past it for a more convenient one. An absent registry names every
/// path it looked at, so "no registry" never reads as "a registry with no rows".
///
/// `known_pools` is the complete list, or `None` when Oxide's pools record could
/// not be read. `None` is never an empty list: the census reports unknown state
/// naming the record, because a pool Barge cannot see may hold the registry it
/// would otherwise pronounce absent — or find a different one in a default pool
/// and report that as the whole truth.
pub fn observe_oxide_registry(
    pools: &Observation<OxidePools>,
    known_pools: Option<&[PathBuf]>,
    deadline: Deadline,
) -> OxideRegistryObservation {
    let Some(known) = known_pools else {
        return OxideRegistryObservation {
            observation: Observation::Unknown {
                source: OXIDE_REGISTRY_SOURCE,
                path: pools.path().to_path_buf(),
                detail: format!(
                    "{}: Oxide's pools record could not be read, so the internal pool list is unknown and no registry was looked for; nothing is reported absent",
                    pools.path().display()
                ),
            },
            searched: Vec::new(),
        };
    };
    let mut searched: Vec<PathBuf> = Vec::new();
    for pool in known {
        let path = pool.join(OXIDE_OUTPUTS_DIR).join(OXIDE_REGISTRY_FILE);
        searched.push(path.clone());
        match read_registry(&path, deadline) {
            Observation::Absent { .. } => continue,
            found => {
                return OxideRegistryObservation {
                    observation: found,
                    searched,
                }
            }
        }
    }
    let path = searched
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(OXIDE_OUTPUTS_DIR).join(OXIDE_REGISTRY_FILE));
    let detail = format!(
        "no Oxide output registry is present in any of the {} known internal pool{}",
        searched.len(),
        if searched.len() == 1 { "" } else { "s" }
    );
    OxideRegistryObservation {
        observation: Observation::Absent {
            source: OXIDE_REGISTRY_SOURCE,
            path,
            detail,
        },
        searched,
    }
}

/// Read Oxide's output registry strictly and boundedly.
///
/// A file that is absent, oversized, unreadable, a symbolic link, an unknown
/// version, a version/schema pair Oxide does not read, over Oxide's own row cap,
/// or not exactly the field set that version defines is reported as exactly
/// that, naming the file. Nothing is repaired and nothing is defaulted.
pub fn read_registry(path: &Path, deadline: Deadline) -> Observation<OxideRegistry> {
    let bounds = Bounds::new(OXIDE_REGISTRY_SIZE_CAP_BYTES, OXIDE_REGISTRY_ROWS, deadline);
    let bytes = match crate::state::read_bounded(path, &bounds) {
        crate::state::ReadOutcome::Absent => {
            return Observation::Absent {
                source: OXIDE_REGISTRY_SOURCE,
                path: path.to_path_buf(),
                detail: format!("no output registry at {}", path.display()),
            }
        }
        crate::state::ReadOutcome::Refused(error) => {
            return unknown(path, error.message().to_owned())
        }
        crate::state::ReadOutcome::Present(bytes) => bytes,
    };
    match parse_registry(path, &bytes) {
        Ok(observed) => Observation::Reported(observed),
        Err(error) => unknown(path, error.message().to_owned()),
    }
}

fn unknown(path: &Path, detail: String) -> Observation<OxideRegistry> {
    Observation::Unknown {
        source: OXIDE_REGISTRY_SOURCE,
        path: path.to_path_buf(),
        detail: format!("{}: {detail}", path.display()),
    }
}

fn divergence(path: &Path, message: String) -> Error {
    Error::new(ErrorKind::UnknownState, message)
        .with_detail("path", path.display().to_string().into())
}

fn parse_registry(path: &Path, bytes: &[u8]) -> Result<Observed<OxideRegistry>> {
    let raw: RawRegistry = serde_json::from_slice(bytes).map_err(|error| {
        divergence(
            path,
            format!("the output registry is not the field set Oxide writes: {error}"),
        )
    })?;
    let schema = match (raw.version.as_str(), raw.schema) {
        ("oxide.outputs.registry/2", 2) => 2,
        ("oxide.outputs.registry/1", 1) => 1,
        (version, schema) => {
            return Err(divergence(
                path,
                format!(
                    "the output registry states version {version:?} at schema {schema}, which Barge does not read"
                ),
            ))
        }
    };
    if raw.revision == 0 {
        return Err(divergence(
            path,
            "the output registry has no revision".to_owned(),
        ));
    }
    if raw.created_by.is_empty() || raw.created_by.len() > 1024 {
        return Err(divergence(
            path,
            "the output registry has no usable creating transaction".to_owned(),
        ));
    }
    match (&raw.incomplete, raw.complete) {
        (Some(incomplete), false) => {
            if incomplete.reason.is_empty() || incomplete.reason.len() > 1024 {
                return Err(divergence(
                    path,
                    "the output registry is incomplete without a usable reason".to_owned(),
                ));
            }
        }
        (None, true) => {}
        _ => {
            return Err(divergence(
                path,
                "the output registry's completeness and its reason disagree".to_owned(),
            ))
        }
    }
    if raw.rows.len() > OXIDE_REGISTRY_ROWS {
        return Err(divergence(
            path,
            format!(
                "the output registry holds {} rows, over Oxide's own {OXIDE_REGISTRY_ROWS}-row cap",
                raw.rows.len()
            ),
        ));
    }
    let rows = raw.rows.into_iter().map(OxideRegistryRow::from).collect();
    Ok(Observed::new(
        OXIDE_REGISTRY_SOURCE,
        raw.version.clone(),
        path.to_path_buf(),
        OxideRegistry {
            version: raw.version,
            schema,
            revision: raw.revision,
            complete: raw.complete,
            incomplete: raw.incomplete.map(OxideRegistryIncomplete::from),
            created_by: raw.created_by,
            rows,
        },
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    version: String,
    #[serde(default)]
    schema: u64,
    revision: u64,
    complete: bool,
    #[serde(default)]
    incomplete: Option<RawIncomplete>,
    created_by: String,
    /// Oxide always writes `rows`, even when it holds none. A document without
    /// it is divergent, never an empty registry: absence of the member is not
    /// evidence that no root is recorded.
    rows: Vec<RawRow>,
    /// Accepted as a member of the document's own field set; never interpreted.
    #[allow(dead_code)]
    #[serde(default)]
    intents: Vec<serde_json::Value>,
    /// Accepted as a member of the document's own field set; never interpreted.
    #[allow(dead_code)]
    #[serde(default)]
    cancelled_overflow: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIncomplete {
    reason: String,
    since_ms: u64,
    transaction: String,
}

impl From<RawIncomplete> for OxideRegistryIncomplete {
    fn from(raw: RawIncomplete) -> Self {
        Self {
            reason: raw.reason,
            since_ms: raw.since_ms,
            transaction: raw.transaction,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    checkout_id: String,
    checkout: PathBuf,
    #[serde(default)]
    checkout_volume_uuid: Option<String>,
    receipt: PathBuf,
    member: String,
    root: PathBuf,
    root_kind: OxideRootKind,
    #[serde(default)]
    volume_uuid: Option<String>,
    recipe: String,
    #[serde(default)]
    nonce: Option<String>,
    generation: u64,
    #[serde(default)]
    root_dev: Option<u64>,
    #[serde(default)]
    root_ino: Option<u64>,
    #[serde(default)]
    marker_digest: Option<String>,
    selected: bool,
    pending: bool,
    #[serde(default)]
    pruning: Option<String>,
    #[serde(default)]
    pruned_ms: Option<u64>,
    #[serde(default)]
    reservation: Option<String>,
    #[serde(default)]
    cancelling: Option<String>,
}

impl From<RawRow> for OxideRegistryRow {
    fn from(raw: RawRow) -> Self {
        Self {
            checkout_id: raw.checkout_id,
            checkout: raw.checkout,
            checkout_volume_uuid: raw.checkout_volume_uuid,
            receipt: raw.receipt,
            member: raw.member,
            root: raw.root,
            root_kind: raw.root_kind,
            volume_uuid: raw.volume_uuid,
            recipe: raw.recipe,
            nonce: raw.nonce,
            generation: raw.generation,
            root_dev: raw.root_dev,
            root_ino: raw.root_ino,
            marker_digest: raw.marker_digest,
            selected: raw.selected,
            pending: raw.pending,
            pruning: raw.pruning,
            pruned_ms: raw.pruned_ms,
            reservation: raw.reservation,
            cancelling: raw.cancelling,
        }
    }
}

/// Oxide's owner marker inside one output root, as Oxide writes it.
///
/// Every member of Oxide's field set is required by the strict parse, so a
/// divergent marker is refused. A version-2 marker must also carry its nonce in
/// the shape Oxide requires; a version-1 marker legitimately has none, so that
/// rule is version-scoped. The census compares the identity facts and the
/// generation afterwards; `label`, `recipe`, `checkout_volume_uuid` and
/// `created_ms` are read only in the sense that their presence and type are
/// validated, and the marker's own bytes are compared through their digest.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OxideRootMarker {
    version: String,
    checkout: PathBuf,
    checkout_id: String,
    label: String,
    recipe: String,
    kind: OxideRootKind,
    #[serde(default)]
    volume_uuid: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    checkout_volume_uuid: Option<String>,
    generation: u64,
    created_ms: u64,
}

/// The census of one registry: every row's recorded facts and identity verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideCensusReport {
    /// The path that decided the observation: the registry that was read, the
    /// first candidate that was looked at, or — when the internal pool list
    /// itself is unknown — the pools record that made it so.
    pub path: PathBuf,
    /// Every registry path that was looked at, in order.
    pub searched: Vec<PathBuf>,
    /// Whether the registry was reported, absent or unknown.
    pub state: ObservationState,
    /// Why it is absent or unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The document's own facts, when it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<OxideRegistryFacts>,
    /// The rows, bounded to Oxide's own row cap. Empty exactly when no registry
    /// was read, which `state` and `detail` say.
    pub rows: RowSet<OxideCensusRow>,
    /// What the census counted, when a registry was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift: Option<OxideCensusDrift>,
}

impl Default for OxideCensusReport {
    /// A document written before this field existed reads as "no census
    /// reported", never as a registry with no rows.
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            searched: Vec::new(),
            state: ObservationState::UnknownState,
            detail: Some("this document reports no Oxide output-registry census".to_owned()),
            document: None,
            rows: RowSet::empty(),
            drift: None,
        }
    }
}

impl OxideCensusReport {
    /// Refuse a list whose counts disagree with its rows.
    pub fn validate(&self) -> Result<()> {
        self.rows.validate()?;
        if let Some(drift) = &self.drift {
            drift.unknown_reasons.validate()?;
        }
        Ok(())
    }
}

/// The registry document's own facts, as the census reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideRegistryFacts {
    /// The version the document states.
    pub version: String,
    /// The document's numeric schema.
    pub schema: u64,
    /// The revision Oxide advanced before its last change.
    pub revision: u64,
    /// Whether Oxide says the document is complete.
    pub complete: bool,
    /// Why Oxide says the document is incomplete, when it says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<OxideIncompleteReport>,
    /// The transaction that created the document.
    pub created_by: String,
}

/// Oxide's own record of why its registry is incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideIncompleteReport {
    /// Oxide's reason text.
    pub reason: String,
    /// When the document became incomplete.
    pub since_ms: u64,
    /// The transaction that was interrupted.
    pub transaction: String,
}

/// One registry row as the census reports it: Oxide's recorded facts, then
/// Barge's identity verdict for that root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideCensusRow {
    /// The checkout the root was created for.
    pub checkout: PathBuf,
    /// The checkout identity Oxide recorded.
    pub checkout_id: String,
    /// The checkout's volume UUID, when the row carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_volume_uuid: Option<String>,
    /// The receipt that references this root.
    pub receipt: PathBuf,
    /// The profile member this reference belongs to.
    pub member: String,
    /// The recipe the root was created from.
    pub recipe: String,
    /// The root path Oxide recorded, exactly as recorded.
    pub root: PathBuf,
    /// Whether the row names an external or a checkout-local root.
    pub root_kind: OxideRootKind,
    /// The recorded volume UUID, when the row carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_uuid: Option<String>,
    /// The root's immutable nonce, when the row carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// The root creation generation Oxide recorded.
    pub generation: u64,
    /// The device Oxide recorded when it committed this selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_dev: Option<u64>,
    /// The inode Oxide recorded when it committed this selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_ino: Option<u64>,
    /// The owner-marker digest Oxide recorded, when it recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker_digest: Option<String>,
    /// Whether this reference is a live selection.
    pub selected: bool,
    /// Whether this reference is a pending selection.
    pub pending: bool,
    /// The prune intent that references this root, when one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruning: Option<String>,
    /// When Oxide pruned this root, for a history row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_ms: Option<u64>,
    /// The receipt reservation transaction that created this row, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reservation: Option<String>,
    /// Whether an unused reservation's cancellation began.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelling: Option<String>,
    /// Barge's identity verdict for the recorded root, with its evidence.
    pub identity: CensusIdentity,
}

/// What one row's identity observation concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CensusVerdict {
    /// The recorded and live identity agree. A changed device is reported
    /// separately and does not refuse.
    Verified,
    /// The observation could not decide. Unknown is never absent and never
    /// verified.
    Unknown,
    /// The recorded volume verified and the recorded path is not there.
    Gone,
}

/// One row's identity verdict, with the evidence behind it.
///
/// `inode_equal` and `device_equal` are present exactly when both the recorded
/// and the live fact were read. A row whose volume is away reports neither: they
/// are absent, never `false`, so an unavailable comparison cannot read as a
/// disagreement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusIdentity {
    /// What the observation concluded.
    pub verdict: CensusVerdict,
    /// Why it is unknown, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<IdentityReason>,
    /// Whether the live inode equals the recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode_equal: Option<bool>,
    /// Whether the live device equals the recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_equal: Option<bool>,
    /// The device Oxide recorded, when it recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_device: Option<u64>,
    /// The live device, when the root was reached this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_device: Option<u64>,
    /// The live inode, when the root was reached this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_inode: Option<u64>,
    /// The mount the recorded path was written under, when it could be
    /// established.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_mount: Option<PathBuf>,
    /// The live mount the recorded volume verified at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_mount: Option<PathBuf>,
    /// Whether the live owner-marker digest equals the recorded one, when both
    /// exist. The digest is a plan fact, never identity (stage 4 design, "The
    /// facts, and the three roles a fact may have").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker_digest_equal: Option<bool>,
    /// The device evidence a verified row carries: both numbers and whether
    /// they differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remount: Option<RemountEvidence>,
}

impl CensusIdentity {
    fn unknown(reason: IdentityReason) -> Self {
        Self {
            verdict: CensusVerdict::Unknown,
            reason: Some(reason),
            inode_equal: None,
            device_equal: None,
            recorded_device: None,
            live_device: None,
            live_inode: None,
            recorded_mount: None,
            live_mount: None,
            marker_digest_equal: None,
            remount: None,
        }
    }
}

/// What one census counted, so the carried finding is one number and not a
/// reading exercise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OxideCensusDrift {
    /// How many rows were observed.
    pub examined: usize,
    /// How many rows the bound or deadline kept the census from observing. They
    /// are counted, never assumed.
    pub unexamined: usize,
    /// Rows whose recorded and live identity agree.
    pub verified: usize,
    /// Verified rows whose live device differs from the recorded one: exactly
    /// what a recorded-device comparison refuses, and what a remount produces.
    pub device_moved: usize,
    /// Rows kept because the recorded inode differs from the live one.
    pub inode_changed: usize,
    /// Rows whose owner-marker bytes differ from the recorded digest.
    pub marker_changed: usize,
    /// Rows whose recorded volume verified and whose recorded path is not there.
    pub gone: usize,
    /// Rows the census could not decide, never counted as absent.
    pub unknown: usize,
    /// Unknown rows by reason code, so a batch of refusals is readable.
    pub unknown_reasons: RowSet<CensusReasonCount>,
}

impl OxideCensusDrift {
    fn new() -> Self {
        Self {
            examined: 0,
            unexamined: 0,
            verified: 0,
            device_moved: 0,
            inode_changed: 0,
            marker_changed: 0,
            gone: 0,
            unknown: 0,
            unknown_reasons: RowSet::empty(),
        }
    }
}

/// One unknown reason and how many rows it kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CensusReasonCount {
    /// The reason code.
    pub reason: IdentityReason,
    /// How many rows it accounts for.
    pub count: usize,
}

/// Observe one registry's rows and report the census and its drift.
///
/// Every row is observed independently: one undecidable row never stops the
/// others. The census reads only, takes no lock, and stops at the deadline it is
/// given, reporting what it did not reach with counts.
pub fn report(
    observation: &OxideRegistryObservation,
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    prefixes: &dyn MountPrefix,
    uid: u32,
    deadline: Deadline,
) -> OxideCensusReport {
    let mut report = OxideCensusReport {
        path: observation.observation.path().to_path_buf(),
        searched: observation.searched.clone(),
        state: observation.observation.state(),
        detail: observation.observation.detail().map(str::to_owned),
        document: None,
        rows: RowSet::empty(),
        drift: None,
    };
    let Some(observed) = observation.observation.observed() else {
        return report;
    };
    let registry = observed.value();
    report.document = Some(OxideRegistryFacts {
        version: registry.version().to_owned(),
        schema: registry.schema(),
        revision: registry.revision(),
        complete: registry.complete(),
        incomplete: registry
            .incomplete()
            .map(|incomplete| OxideIncompleteReport {
                reason: incomplete.reason().to_owned(),
                since_ms: incomplete.since_ms(),
                transaction: incomplete.transaction().to_owned(),
            }),
        created_by: registry.created_by().to_owned(),
    });

    let total = registry.rows().len();
    let mut rows = Vec::new();
    let mut drift = OxideCensusDrift::new();
    let mut reasons: Vec<(IdentityReason, usize)> = Vec::new();
    for row in registry.rows() {
        if deadline.expired() {
            break;
        }
        let Some(identity) = observe_row(dirs, facts, mounts, prefixes, uid, row, deadline) else {
            break;
        };
        count(&mut drift, &mut reasons, &identity);
        rows.push(OxideCensusRow::from_row(row, identity));
    }
    drift.examined = rows.len();
    drift.unexamined = total.saturating_sub(rows.len());
    let mut counts: Vec<CensusReasonCount> = reasons
        .into_iter()
        .map(|(reason, count)| CensusReasonCount { reason, count })
        .collect();
    counts.sort_by_key(|entry| reason_name(entry.reason));
    let reason_total = counts.len();
    drift.unknown_reasons = RowSet::bounded(counts, reason_total, OXIDE_CENSUS_REASONS);
    report.rows = RowSet::bounded(rows, total, OXIDE_REGISTRY_ROWS);
    report.drift = Some(drift);
    report
}

fn count(
    drift: &mut OxideCensusDrift,
    reasons: &mut Vec<(IdentityReason, usize)>,
    identity: &CensusIdentity,
) {
    match identity.verdict {
        CensusVerdict::Verified => {
            drift.verified += 1;
            if identity.device_equal == Some(false) {
                drift.device_moved += 1;
            }
            if identity.marker_digest_equal == Some(false) {
                drift.marker_changed += 1;
            }
        }
        CensusVerdict::Gone => drift.gone += 1,
        CensusVerdict::Unknown => {
            drift.unknown += 1;
            let Some(reason) = identity.reason else {
                return;
            };
            if reason == IdentityReason::InodeChanged {
                drift.inode_changed += 1;
            }
            match reasons.iter_mut().find(|(seen, _)| *seen == reason) {
                Some((_, count)) => *count += 1,
                None => reasons.push((reason, 1)),
            }
        }
    }
}

/// The reason code's own text, so a summary is stable and sortable.
fn reason_name(reason: IdentityReason) -> &'static str {
    match reason {
        IdentityReason::IdentityRecordMissing => "identity_record_missing",
        IdentityReason::IdentityRecordInvalid => "identity_record_invalid",
        IdentityReason::VolumeUnverified => "volume_unverified",
        IdentityReason::DuplicateVolumeUuid => "duplicate_volume_uuid",
        IdentityReason::NotARealDirectory => "not_a_real_directory",
        IdentityReason::PathChanged => "path_changed",
        IdentityReason::InodeChanged => "inode_changed",
        IdentityReason::MarkerIdentityMismatch => "marker_identity_mismatch",
        IdentityReason::MarkerAbsent => "marker_absent",
        IdentityReason::MarkerUnreadable => "marker_unreadable",
        IdentityReason::MarkerForeignOwner => "marker_foreign_owner",
        IdentityReason::MarkerVersionUnknown => "marker_version_unknown",
        IdentityReason::MarkerSchemaDivergent => "marker_schema_divergent",
        IdentityReason::MarkerGenerationUnexpected => "marker_generation_unexpected",
    }
}

/// Whether a value is Oxide's marker nonce shape: 128 bits of lowercase
/// hexadecimal, as Oxide's own marker validation requires.
fn is_hex32(value: Option<&str>) -> bool {
    value.is_some_and(|nonce| {
        nonce.len() == 32
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

/// Observe one record's live identity, or `None` when the deadline expired
/// before the observation could be decided.
fn observe_row(
    dirs: &dyn DirOps,
    facts: &dyn VolumeFacts,
    mounts: &dyn MountRoot,
    prefixes: &dyn MountPrefix,
    uid: u32,
    row: &OxideRegistryRow,
    deadline: Deadline,
) -> Option<CensusIdentity> {
    if deadline.expired() {
        return None;
    }
    let unknown = |reason| Some(CensusIdentity::unknown(reason));
    let Some(recorded_dev) = row.root_dev() else {
        // The row records no identity facts to compare against. A v1-era row is
        // the ordinary case, and the honest answer is that nothing can decide.
        return unknown(IdentityReason::IdentityRecordMissing);
    };
    let Some(recorded_ino) = row.root_ino() else {
        return unknown(IdentityReason::IdentityRecordMissing);
    };
    let Some(raw_uuid) = row.volume_uuid() else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    let Ok(uuid) = VolumeUuid::parse(raw_uuid) else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    if uuid.as_str() != raw_uuid {
        // A UUID Barge cannot compare exactly is not the recorded volume.
        return unknown(IdentityReason::VolumeUnverified);
    }
    let Ok(candidates) = facts.mount_census(&uuid) else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    if candidates.len() > 1 {
        return unknown(IdentityReason::DuplicateVolumeUuid);
    }
    let Some(mount) = candidates.first() else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    let expected = ExpectedVolume {
        volume_uuid: &uuid,
        mount_hint: Some(mount.as_path()),
    };
    let Ok(volume) = crate::os::verify_volume(facts, mounts, mount, &expected) else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    let Ok(mut current) = dirs.open_path(volume.path()) else {
        return unknown(IdentityReason::VolumeUnverified);
    };
    if current.identity().device != volume.identity().device {
        return unknown(IdentityReason::VolumeUnverified);
    }
    let Ok(recorded_mount) = recorded_mount(prefixes, row.root(), mount) else {
        return unknown(IdentityReason::PathChanged);
    };
    let Ok(relative) = row.root().strip_prefix(&recorded_mount) else {
        return unknown(IdentityReason::PathChanged);
    };
    let components: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => name.to_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    if components.is_empty()
        || components.len() != relative.components().count()
        || components.len() > OXIDE_CENSUS_DEPTH
    {
        return unknown(IdentityReason::PathChanged);
    }
    let live_mount = mount.clone();
    let mut leaf_gone = false;
    for (index, component) in components.iter().enumerate() {
        if deadline.expired() {
            return None;
        }
        let entry = match dirs.child_facts(&current, component) {
            Ok(Some(entry)) => entry,
            Ok(None) if index + 1 != components.len() => {
                return unknown(IdentityReason::PathChanged)
            }
            Ok(None) => {
                leaf_gone = true;
                break;
            }
            Err(_) => return unknown(IdentityReason::NotARealDirectory),
        };
        if entry.kind != EntryKind::Directory
            || entry.uid != uid
            || entry.device != volume.identity().device
        {
            return unknown(IdentityReason::NotARealDirectory);
        }
        let Ok(opened) = dirs.open_child_dir(&current, component) else {
            return unknown(IdentityReason::PathChanged);
        };
        if opened.identity()
            != (DirId {
                device: entry.device,
                inode: entry.inode,
            })
        {
            return unknown(IdentityReason::PathChanged);
        }
        current = opened;
    }
    if leaf_gone {
        // "Gone" is only reachable from a volume that still verifies, and only
        // when nothing moved under the walk that reached the missing leaf.
        let stable = volume.reverify(facts).is_ok()
            && facts.mount_census(&uuid).ok().as_ref() == Some(&candidates)
            && dirs.recheck_dir(&current).ok() == Some(current.identity());
        return Some(if stable {
            CensusIdentity {
                verdict: CensusVerdict::Gone,
                reason: None,
                inode_equal: None,
                device_equal: None,
                recorded_device: Some(recorded_dev),
                live_device: None,
                live_inode: None,
                recorded_mount: Some(recorded_mount),
                live_mount: Some(live_mount),
                marker_digest_equal: None,
                remount: None,
            }
        } else {
            CensusIdentity::unknown(IdentityReason::VolumeUnverified)
        });
    }
    let live = current.identity();
    let live_device = live.device.get();
    let inode_equal = live.inode == recorded_ino;
    let device_equal = live_device == recorded_dev;
    let remount = RemountEvidence {
        device_at_record: recorded_dev,
        device_live: live_device,
        changed: !device_equal,
    };
    let kept = |reason, marker_digest_equal| CensusIdentity {
        verdict: CensusVerdict::Unknown,
        reason: Some(reason),
        inode_equal: Some(inode_equal),
        device_equal: Some(device_equal),
        recorded_device: Some(recorded_dev),
        live_device: Some(live_device),
        live_inode: Some(live.inode),
        recorded_mount: Some(recorded_mount.clone()),
        live_mount: Some(live_mount.clone()),
        marker_digest_equal,
        remount: None,
    };
    if !inode_equal {
        return Some(kept(IdentityReason::InodeChanged, None));
    }
    if deadline.expired() {
        return None;
    }
    let marker_facts = match dirs.child_facts(&current, OXIDE_ROOT_MARKER_FILE) {
        Ok(Some(facts)) => facts,
        Ok(None) => return Some(kept(IdentityReason::MarkerAbsent, None)),
        Err(_) => return Some(kept(IdentityReason::MarkerUnreadable, None)),
    };
    if marker_facts.uid != uid {
        return Some(kept(IdentityReason::MarkerForeignOwner, None));
    }
    if marker_facts.kind != EntryKind::File
        || !marker_facts.is_private()
        || marker_facts.size > OXIDE_ROOT_MARKER_SIZE_CAP_BYTES as u64
    {
        return Some(kept(IdentityReason::MarkerUnreadable, None));
    }
    let bytes = match dirs.read_child_file(
        &current,
        OXIDE_ROOT_MARKER_FILE,
        OXIDE_ROOT_MARKER_SIZE_CAP_BYTES,
    ) {
        Ok(Some(bytes)) => bytes,
        _ => return Some(kept(IdentityReason::MarkerUnreadable, None)),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Some(kept(IdentityReason::MarkerSchemaDivergent, None)),
    };
    if !matches!(
        value.get("version").and_then(serde_json::Value::as_str),
        Some(OXIDE_ROOT_MARKER_V2 | OXIDE_ROOT_MARKER_V1)
    ) {
        return Some(kept(IdentityReason::MarkerVersionUnknown, None));
    }
    let marker: OxideRootMarker = match serde_json::from_slice(&bytes) {
        Ok(marker) => marker,
        Err(_) => return Some(kept(IdentityReason::MarkerSchemaDivergent, None)),
    };
    let digest_equal = row
        .marker_digest()
        .map(|recorded| recorded == crate::digest::sha256_hex(&bytes));
    // The marker's own version rule comes first, exactly as Oxide applies it: a
    // version-2 marker must carry its 128-bit lowercase-hex nonce. A marker that
    // breaks it is divergent even when the row agrees with it, because two
    // identically absent or malformed nonces compare equal without identifying
    // anything. A version-1 marker legitimately has none, so the rule is
    // version-scoped.
    if marker.version == OXIDE_ROOT_MARKER_V2 && !is_hex32(marker.nonce.as_deref()) {
        return Some(kept(IdentityReason::MarkerSchemaDivergent, digest_equal));
    }
    if marker.kind != row.root_kind()
        || marker.checkout != row.checkout()
        || marker.checkout_id != row.checkout_id
        || marker.volume_uuid.as_deref() != row.volume_uuid()
        || marker.nonce.as_deref() != row.nonce()
    {
        return Some(kept(IdentityReason::MarkerIdentityMismatch, digest_equal));
    }
    if marker.generation != row.generation() {
        return Some(kept(
            IdentityReason::MarkerGenerationUnexpected,
            digest_equal,
        ));
    }
    if deadline.expired() {
        return None;
    }
    // The observation the verdict rests on is re-read before it is reported, so
    // a component, marker or mount that changed mid-walk is unknown, never a
    // stale verdict.
    let stable = dirs
        .child_facts(&current, OXIDE_ROOT_MARKER_FILE)
        .ok()
        .flatten()
        .as_ref()
        == Some(&marker_facts)
        && dirs.recheck_dir(&current).ok() == Some(current.identity())
        && volume.reverify(facts).is_ok()
        && facts.mount_census(&uuid).ok().as_ref() == Some(&candidates);
    if !stable {
        return Some(kept(IdentityReason::PathChanged, digest_equal));
    }
    Some(CensusIdentity {
        verdict: CensusVerdict::Verified,
        reason: None,
        inode_equal: Some(true),
        device_equal: Some(device_equal),
        recorded_device: Some(recorded_dev),
        live_device: Some(live_device),
        live_inode: Some(live.inode),
        recorded_mount: Some(recorded_mount),
        live_mount: Some(live_mount),
        marker_digest_equal: digest_equal,
        remount: Some(remount),
    })
}

/// The mount a recorded path was written under: the live mount when the path is
/// still below it, and the platform's recorded mount prefix otherwise. Both are
/// replaced by the verified live mount before the walk.
fn recorded_mount(prefixes: &dyn MountPrefix, root: &Path, live_mount: &Path) -> Result<PathBuf> {
    if root.starts_with(live_mount) {
        return Ok(live_mount.to_path_buf());
    }
    prefixes.mount_prefix(root).ok_or_else(|| {
        Error::new(
            ErrorKind::UnknownState,
            "the recorded root is not under a mount the platform records",
        )
    })
}

impl OxideCensusRow {
    fn from_row(row: &OxideRegistryRow, identity: CensusIdentity) -> Self {
        Self {
            checkout: row.checkout.clone(),
            checkout_id: row.checkout_id.clone(),
            checkout_volume_uuid: row.checkout_volume_uuid.clone(),
            receipt: row.receipt.clone(),
            member: row.member.clone(),
            recipe: row.recipe.clone(),
            root: row.root.clone(),
            root_kind: row.root_kind,
            volume_uuid: row.volume_uuid.clone(),
            nonce: row.nonce.clone(),
            generation: row.generation,
            root_dev: row.root_dev,
            root_ino: row.root_ino,
            marker_digest: row.marker_digest.clone(),
            selected: row.selected,
            pending: row.pending,
            pruning: row.pruning.clone(),
            pruned_ms: row.pruned_ms,
            reservation: row.reservation.clone(),
            cancelling: row.cancelling.clone(),
            identity,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::cell::Cell;
    use std::path::{Path, PathBuf};

    use super::{
        observe_oxide_registry, read_registry, report, CensusVerdict, IdentityReason,
        OxideCensusReport, OxideCensusRow, OxideRegistryObservation, OxideRootKind,
        OXIDE_REGISTRY_SIZE_CAP_BYTES, OXIDE_ROOT_MARKER_SIZE_CAP_BYTES,
    };
    use crate::contract::RowSet;
    use crate::inspect::{
        read_pools, Observation, ObservationState, OxidePools, OXIDE_POOLS_SOURCE,
    };
    use crate::os::{
        DeviceId, DirOps, FileSystem, MountPoint, MountPrefix, MountRoot, ProcessObserver,
        VolumeFacts, VolumeIdentity, VolumeUuid,
    };
    use crate::state::Deadline;
    use crate::testkit::TempDir;

    const UUID: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";
    const OTHER_UUID: &str = "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D";
    const CHECKOUT_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const NONCE: &str = "22222222222222222222222222222222";
    const CHECKOUT: &str = "/Users/fixture/Dev/project";
    const RECEIPT: &str = "/Users/fixture/Dev/project/.oxide/receipt.json";

    /// Injected volume facts over a real fixture directory: the fixture mount is
    /// the live mount, and `recorded_mount` is the prefix recorded paths were
    /// written under, which is how a mount-point change is stated.
    struct FakeVolume {
        uuid: VolumeUuid,
        mount: PathBuf,
        recorded_mount: PathBuf,
        device: DeviceId,
        present: Cell<bool>,
        duplicate: bool,
    }

    impl FakeVolume {
        fn new(fixture: &TempDir) -> Self {
            let mount = fixture.join("volume");
            std::fs::create_dir_all(&mount).expect("the fixture mount root is created");
            let device = crate::os::platform()
                .open_path(&mount)
                .expect("the fixture mount opens")
                .identity()
                .device;
            Self {
                uuid: VolumeUuid::parse(UUID).expect("a fixture UUID"),
                recorded_mount: mount.clone(),
                mount,
                device,
                present: Cell::new(true),
                duplicate: false,
            }
        }

        fn identity(&self) -> VolumeIdentity {
            VolumeIdentity {
                mount_point: self.mount.clone(),
                filesystem: FileSystem::Apfs,
                device: self.device,
                volume_uuid: Some(self.uuid.clone()),
                boot_volume: false,
            }
        }
    }

    impl VolumeFacts for FakeVolume {
        fn mount_census(&self, uuid: &VolumeUuid) -> crate::error::Result<Vec<PathBuf>> {
            if self.present.get() && &self.uuid == uuid {
                let mut mounts = vec![self.mount.clone()];
                if self.duplicate {
                    mounts.push(self.mount.with_file_name("volume 1"));
                }
                return Ok(mounts);
            }
            Ok(Vec::new())
        }

        fn identify(&self, _path: &Path) -> crate::error::Result<VolumeIdentity> {
            if !self.present.get() {
                return Err(crate::error::Error::new(
                    crate::error::ErrorKind::VolumeAbsent,
                    "the fixture volume is not attached",
                ));
            }
            Ok(self.identity())
        }
    }

    impl MountRoot for FakeVolume {
        fn mount_root(&self, path: &Path) -> crate::error::Result<MountPoint> {
            if path != self.mount {
                return Err(crate::error::Error::new(
                    crate::error::ErrorKind::VolumeMismatch,
                    "not the fixture mount root",
                ));
            }
            Ok(MountPoint::new(self.mount.clone()))
        }
    }

    impl MountPrefix for FakeVolume {
        fn mount_prefix(&self, path: &Path) -> Option<PathBuf> {
            path.starts_with(&self.recorded_mount)
                .then(|| self.recorded_mount.clone())
        }
    }

    /// One recorded root, as Oxide would write the row and its marker.
    struct Row {
        root: PathBuf,
        inode: u64,
        device: u64,
        marker: Vec<u8>,
        nonce: String,
        generation: u64,
    }

    /// A fixture volume with one recordable root inside it.
    struct Fixture {
        dir: TempDir,
        uuid: VolumeUuid,
        volume: FakeVolume,
        row: Row,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = TempDir::new(label);
            let volume = FakeVolume::new(&dir);
            let root = volume.mount.join("oxide-outputs").join("root");
            std::fs::create_dir_all(&root).expect("the fixture root is created");
            let platform = crate::os::platform();
            let facts = platform
                .open_path(&root)
                .expect("the fixture root opens")
                .identity();
            let marker = marker_bytes(
                "oxide.outputs.root/2",
                OxideRootKind::External,
                Some(UUID),
                Some(NONCE),
                1,
            );
            write_marker(&root, &marker);
            let uuid = volume.uuid.clone();
            Self {
                dir,
                uuid,
                volume,
                row: Row {
                    root,
                    inode: facts.inode,
                    device: facts.device.get(),
                    marker,
                    nonce: NONCE.to_owned(),
                    generation: 1,
                },
            }
        }

        fn uid(&self) -> u32 {
            crate::os::platform()
                .effective_uid()
                .expect("the fixture uid is readable")
        }

        fn row_json(&self) -> serde_json::Value {
            serde_json::json!({
                "checkout_id": CHECKOUT_ID,
                "checkout": CHECKOUT,
                "checkout_volume_uuid": OTHER_UUID,
                "receipt": RECEIPT,
                "member": "target",
                "root": self.row.root,
                "root_kind": "external",
                "volume_uuid": self.uuid.as_str(),
                "recipe": "target-debug",
                "nonce": self.row.nonce,
                "generation": self.row.generation,
                "root_dev": self.row.device,
                "root_ino": self.row.inode,
                "marker_digest": crate::digest::sha256_hex(&self.row.marker),
                "selected": true,
                "pending": false,
            })
        }

        fn registry(&self, rows: Vec<serde_json::Value>) -> OxideRegistryObservation {
            self.registry_with(rows, true)
        }

        fn registry_with(
            &self,
            rows: Vec<serde_json::Value>,
            complete: bool,
        ) -> OxideRegistryObservation {
            let document = serde_json::json!({
                "version": "oxide.outputs.registry/2",
                "schema": 2,
                "revision": 7,
                "complete": complete,
                "created_by": "fixture-transaction",
                "rows": rows,
                "intents": [],
                "cancelled_overflow": [],
            });
            self.dir.write(
                "pool/outputs/registry.json",
                &serde_json::to_vec(&document).unwrap(),
            );
            super::observe_oxide_registry(
                &absent_pools(),
                Some(&[self.dir.join("pool")]),
                Deadline::after(std::time::Duration::from_secs(5)),
            )
        }

        fn report(&self, observation: &OxideRegistryObservation) -> OxideCensusReport {
            let platform = crate::os::platform();
            report(
                observation,
                &platform,
                &self.volume,
                &self.volume,
                &self.volume,
                self.uid(),
                Deadline::after(std::time::Duration::from_secs(5)),
            )
        }

        fn census(&self, rows: Vec<serde_json::Value>) -> OxideCensusReport {
            let observation = self.registry(rows);
            self.report(&observation)
        }

        fn row<'a>(&self, report: &'a OxideCensusReport) -> &'a OxideCensusRow {
            assert_eq!(report.rows.count, 1, "{report:#?}");
            &report.rows.rows[0]
        }
    }

    /// An absent pools record: Oxide recorded no pools, so only its defaults can
    /// be in use, and the candidate list is complete.
    fn absent_pools() -> Observation<OxidePools> {
        Observation::Absent {
            source: OXIDE_POOLS_SOURCE,
            path: PathBuf::from("/fixture/state/pools.json"),
            detail: "no pools record".to_owned(),
        }
    }

    /// A pools record that could not be read: the pool list is unknown, so the
    /// candidate list is not.
    fn unknown_pools() -> Observation<OxidePools> {
        Observation::Unknown {
            source: OXIDE_POOLS_SOURCE,
            path: PathBuf::from("/fixture/state/pools.json"),
            detail: "/fixture/state/pools.json: the file is not one JSON document".to_owned(),
        }
    }

    /// Write one owner marker exactly as Oxide does: a private regular file.
    fn write_marker(root: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(super::OXIDE_ROOT_MARKER_FILE);
        std::fs::write(&path, bytes).expect("the fixture marker is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("the fixture marker is private");
    }

    fn marker_bytes(
        version: &str,
        kind: OxideRootKind,
        volume_uuid: Option<&str>,
        nonce: Option<&str>,
        generation: u64,
    ) -> Vec<u8> {
        let mut value = serde_json::json!({
            "version": version,
            "checkout": CHECKOUT,
            "checkout_id": CHECKOUT_ID,
            "label": "profile: dev",
            "recipe": "target-debug",
            "kind": kind,
            "generation": generation,
            "created_ms": 1_792_525_019_847_u64,
        });
        if let Some(volume_uuid) = volume_uuid {
            value["volume_uuid"] = serde_json::json!(volume_uuid);
        }
        if let Some(nonce) = nonce {
            value["nonce"] = serde_json::json!(nonce);
        }
        if version == "oxide.outputs.root/2" {
            value["checkout_volume_uuid"] = serde_json::json!(OTHER_UUID);
        }
        serde_json::to_vec(&value).expect("the marker encodes")
    }

    #[test]
    fn a_verified_row_reports_both_identity_facts_and_the_recorded_selection() {
        let fixture = Fixture::new("census-verified");
        let report = fixture.census(vec![fixture.row_json()]);
        assert_eq!(report.state, ObservationState::Reported);
        let document = report.document.as_ref().expect("the document was read");
        assert_eq!(document.version, "oxide.outputs.registry/2");
        assert_eq!(document.schema, 2);
        assert_eq!(document.revision, 7);
        assert!(document.complete);
        assert!(document.incomplete.is_none());
        let row = fixture.row(&report);
        assert_eq!(row.root_kind, OxideRootKind::External);
        assert_eq!(row.member, "target");
        assert_eq!(row.generation, 1);
        assert!(row.selected);
        assert!(!row.pending);
        assert_eq!(row.identity.verdict, CensusVerdict::Verified, "{row:#?}");
        assert_eq!(row.identity.inode_equal, Some(true));
        assert_eq!(row.identity.device_equal, Some(true));
        assert_eq!(row.identity.recorded_device, Some(fixture.row.device));
        assert_eq!(row.identity.live_device, Some(fixture.row.device));
        assert_eq!(row.identity.live_inode, Some(fixture.row.inode));
        assert_eq!(row.identity.marker_digest_equal, Some(true));
        assert_eq!(
            row.identity.recorded_mount.as_deref(),
            Some(fixture.volume.mount.as_path())
        );
        let remount = row.identity.remount.as_ref().expect("the remount evidence");
        assert!(!remount.changed);
        let drift = report.drift.as_ref().expect("the drift report");
        assert_eq!(drift.examined, 1);
        assert_eq!(drift.verified, 1);
        assert_eq!(drift.device_moved, 0);
        assert_eq!(drift.inode_changed, 0);
        assert_eq!(drift.unknown, 0);
        assert_eq!(drift.gone, 0);
    }

    #[test]
    fn the_carried_finding_a_changed_device_verifies_and_names_both_numbers() {
        let fixture = Fixture::new("census-device-moved");
        let mut recorded = fixture.row_json();
        let live_device = fixture.row.device;
        recorded["root_dev"] = serde_json::json!(live_device + 1);
        let report = fixture.census(vec![recorded.clone()]);
        let row = fixture.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Verified);
        assert_eq!(row.identity.inode_equal, Some(true));
        assert_eq!(row.identity.device_equal, Some(false));
        assert_eq!(row.identity.recorded_device, Some(live_device + 1));
        assert_eq!(row.identity.live_device, Some(live_device));
        let remount = row.identity.remount.as_ref().expect("the remount evidence");
        assert!(remount.changed);
        assert_eq!(remount.device_at_record, live_device + 1);
        assert_eq!(remount.device_live, live_device);
        // The regression the carried finding needs: this is exactly the pair a
        // recorded-device comparison refuses, and the verdict above does not.
        assert_ne!(live_device, recorded["root_dev"].as_u64().unwrap());
        let drift = report.drift.as_ref().expect("the drift report");
        assert_eq!(drift.verified, 1);
        assert_eq!(drift.device_moved, 1);
        assert_eq!(drift.inode_changed, 0);
    }

    #[test]
    fn a_changed_inode_keeps_the_root_and_reports_both_inodes() {
        let fixture = Fixture::new("census-inode-changed");
        let mut recorded = fixture.row_json();
        recorded["root_ino"] = serde_json::json!(fixture.row.inode + 1);
        let report = fixture.census(vec![recorded]);
        let row = fixture.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Unknown);
        assert_eq!(row.identity.reason, Some(IdentityReason::InodeChanged));
        assert_eq!(row.identity.inode_equal, Some(false));
        assert_eq!(row.identity.live_inode, Some(fixture.row.inode));
        assert_eq!(row.identity.device_equal, Some(true));
        assert_eq!(
            row.identity.recorded_mount.as_deref(),
            Some(fixture.volume.mount.as_path())
        );
        let drift = report.drift.as_ref().expect("the drift report");
        assert_eq!(drift.inode_changed, 1);
        assert_eq!(drift.verified, 0);
        let reasons = &drift.unknown_reasons;
        assert_eq!(reasons.count, 1);
        assert_eq!(reasons.rows[0].reason, IdentityReason::InodeChanged);
        assert_eq!(reasons.rows[0].count, 1);
    }

    #[test]
    fn a_gone_root_and_an_away_volume_never_read_the_same() {
        let present = Fixture::new("census-gone");
        let gone_path = present.row.root.clone();
        std::fs::remove_dir_all(&gone_path).expect("the fixture root is removed");
        let report = present.census(vec![present.row_json()]);
        let row = present.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Gone);
        assert!(row.identity.reason.is_none());
        assert_eq!(row.identity.inode_equal, None);
        assert_eq!(row.identity.live_device, None);
        assert_eq!(report.drift.as_ref().map(|drift| drift.gone), Some(1));

        let away = Fixture::new("census-away");
        away.volume.present.set(false);
        let report = away.census(vec![away.row_json()]);
        let row = away.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Unknown);
        assert_eq!(row.identity.reason, Some(IdentityReason::VolumeUnverified));
        assert_eq!(row.identity.inode_equal, None);
        assert_eq!(row.identity.device_equal, None);
        assert_eq!(row.identity.live_mount, None);
        let drift = report.drift.as_ref().expect("the drift report");
        assert_eq!(drift.unknown, 1);
        assert_eq!(drift.gone, 0);
        assert_eq!(drift.verified, 0);
    }

    #[test]
    fn a_remounted_volume_is_found_by_uuid_and_reports_the_moved_mount() {
        let mut fixture = Fixture::new("census-mount-moved");
        // The row was written while the volume was mounted at another name; the
        // live mount is the fixture mount. The recorded prefix is replaced and
        // the components below it are walked exactly.
        let recorded_mount = fixture.dir.join("Formerly");
        fixture.volume.recorded_mount = recorded_mount.clone();
        let mut recorded = fixture.row_json();
        let relative = fixture
            .row
            .root
            .strip_prefix(&fixture.volume.mount)
            .expect("the root is below the live mount");
        recorded["root"] = serde_json::json!(recorded_mount.join(relative));
        let report = fixture.census(vec![recorded]);
        let row = fixture.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Verified, "{row:#?}");
        assert_eq!(
            row.identity.recorded_mount.as_deref(),
            Some(recorded_mount.as_path())
        );
        assert_eq!(
            row.identity.live_mount.as_deref(),
            Some(fixture.volume.mount.as_path())
        );
        assert_eq!(row.identity.device_equal, Some(true));
    }

    #[test]
    fn a_duplicated_volume_uuid_refuses_the_row_and_never_picks_a_mount() {
        let mut fixture = Fixture::new("census-duplicate");
        fixture.volume.duplicate = true;
        let report = fixture.census(vec![fixture.row_json()]);
        let row = fixture.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Unknown);
        assert_eq!(
            row.identity.reason,
            Some(IdentityReason::DuplicateVolumeUuid)
        );
        assert_eq!(row.identity.live_mount, None);
    }

    #[test]
    fn the_owner_marker_is_checked_without_becoming_identity() {
        let fixture = Fixture::new("census-marker");
        // A rewritten marker keeps the root: its digest is a plan fact, and the
        // identity facts still agree.
        let rewritten = fixture.row_json();
        // The same identity facts, written again: only the bytes differ.
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture.row.marker).expect("the fixture marker decodes");
        value["label"] = serde_json::json!("profile: dev (rewritten)");
        let replacement = serde_json::to_vec(&value).expect("the marker encodes");
        write_marker(&fixture.row.root, &replacement);
        let report = fixture.census(vec![rewritten.clone()]);
        let row = fixture.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Verified);
        assert_eq!(row.identity.marker_digest_equal, Some(false));
        assert_eq!(
            report.drift.as_ref().map(|drift| drift.marker_changed),
            Some(1)
        );

        // An absent marker is unknown, never verified.
        std::fs::remove_file(fixture.row.root.join(super::OXIDE_ROOT_MARKER_FILE))
            .expect("the fixture marker is removed");
        let report = fixture.census(vec![rewritten.clone()]);
        let row = fixture.row(&report);
        assert_eq!(row.identity.verdict, CensusVerdict::Unknown);
        assert_eq!(row.identity.reason, Some(IdentityReason::MarkerAbsent));

        // A marker that names another nonce is an identity mismatch.
        write_marker(
            &fixture.row.root,
            &marker_bytes(
                "oxide.outputs.root/2",
                OxideRootKind::External,
                Some(UUID),
                Some("33333333333333333333333333333333"),
                1,
            ),
        );
        let report = fixture.census(vec![rewritten.clone()]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerIdentityMismatch)
        );

        // An unexpected generation is its own reason.
        write_marker(
            &fixture.row.root,
            &marker_bytes(
                "oxide.outputs.root/2",
                OxideRootKind::External,
                Some(UUID),
                Some(NONCE),
                3,
            ),
        );
        let report = fixture.census(vec![rewritten.clone()]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerGenerationUnexpected)
        );

        // An unknown version and a divergent field set are both unknown.
        write_marker(
            &fixture.row.root,
            &marker_bytes(
                "oxide.outputs.root/9",
                OxideRootKind::External,
                Some(UUID),
                Some(NONCE),
                1,
            ),
        );
        let report = fixture.census(vec![rewritten.clone()]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerVersionUnknown)
        );
        write_marker(
            &fixture.row.root,
            b"{\"version\":\"oxide.outputs.root/2\",\"foreign\":true}",
        );
        let report = fixture.census(vec![rewritten]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerSchemaDivergent)
        );
    }

    #[test]
    fn a_version_two_marker_without_its_nonce_is_unknown_even_when_the_row_agrees() {
        // The concrete defect: a version-2 marker needs its 128-bit lowercase-hex
        // nonce. When the row records the same absent or malformed value, the two
        // sides compare equal without identifying anything, so the marker's own
        // version rule has to refuse before identity is reported.
        let fixture = Fixture::new("census-marker-nonce");
        let mut row = fixture.row_json();

        // Absent on both sides.
        let mut absent: serde_json::Value =
            serde_json::from_slice(&fixture.row.marker).expect("the fixture marker decodes");
        absent
            .as_object_mut()
            .expect("a marker object")
            .remove("nonce");
        write_marker(
            &fixture.row.root,
            &serde_json::to_vec(&absent).expect("the marker encodes"),
        );
        row["nonce"] = serde_json::json!(null);
        let report = fixture.census(vec![row.clone()]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerSchemaDivergent),
            "a version-2 marker with no nonce is divergent"
        );
        assert_eq!(
            fixture.row(&report).identity.verdict,
            CensusVerdict::Unknown
        );

        // Malformed on both sides, in each way Oxide refuses.
        for malformed in [
            "not-a-nonce",
            "ABCDEF0123456789ABCDEF0123456789",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
        ] {
            write_marker(
                &fixture.row.root,
                &marker_bytes(
                    "oxide.outputs.root/2",
                    OxideRootKind::External,
                    Some(UUID),
                    Some(malformed),
                    1,
                ),
            );
            row["nonce"] = serde_json::json!(malformed);
            let report = fixture.census(vec![row.clone()]);
            let identity = &fixture.row(&report).identity;
            assert_eq!(
                identity.reason,
                Some(IdentityReason::MarkerSchemaDivergent),
                "`{malformed}` is not a nonce Oxide accepts"
            );
            assert_ne!(identity.verdict, CensusVerdict::Verified);
        }

        // The boundary, exactly as Oxide draws it: a version-1 marker predates
        // the nonce, so the same absent value verifies there.
        write_marker(
            &fixture.row.root,
            &marker_bytes(
                "oxide.outputs.root/1",
                OxideRootKind::External,
                Some(UUID),
                None,
                1,
            ),
        );
        row["nonce"] = serde_json::json!(null);
        let report = fixture.census(vec![row]);
        let identity = &fixture.row(&report).identity;
        assert_eq!(
            identity.verdict,
            CensusVerdict::Verified,
            "a version-1 marker legitimately has no nonce: {identity:#?}"
        );
    }

    #[test]
    fn an_oversized_or_symlinked_marker_is_unknown_not_read() {
        let fixture = Fixture::new("census-marker-unreadable");
        let marker = fixture.row.root.join(super::OXIDE_ROOT_MARKER_FILE);
        write_marker(
            &fixture.row.root,
            &vec![b'x'; OXIDE_ROOT_MARKER_SIZE_CAP_BYTES + 1],
        );
        let report = fixture.census(vec![fixture.row_json()]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerUnreadable)
        );
        std::fs::remove_file(&marker).expect("the oversized marker is removed");
        std::os::unix::fs::symlink(&fixture.row.root, &marker).expect("the marker link is made");
        let report = fixture.census(vec![fixture.row_json()]);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::MarkerUnreadable)
        );
    }

    #[test]
    fn one_undecidable_row_never_stops_the_others() {
        let fixture = Fixture::new("census-per-row");
        let mut without_inode = fixture.row_json();
        without_inode
            .as_object_mut()
            .expect("a row object")
            .remove("root_ino");
        let mut absent_volume = fixture.row_json();
        absent_volume["volume_uuid"] = serde_json::json!(null);
        let report = fixture.census(vec![without_inode, absent_volume, fixture.row_json()]);
        assert_eq!(report.rows.count, 3);
        assert_eq!(
            report.rows.rows[0].identity.reason,
            Some(IdentityReason::IdentityRecordMissing)
        );
        assert_eq!(
            report.rows.rows[1].identity.reason,
            Some(IdentityReason::VolumeUnverified)
        );
        assert_eq!(
            report.rows.rows[2].identity.verdict,
            CensusVerdict::Verified
        );
        let drift = report.drift.as_ref().expect("the drift report");
        assert_eq!(drift.examined, 3);
        assert_eq!(drift.verified, 1);
        assert_eq!(drift.unknown, 2);
        assert_eq!(drift.unknown_reasons.count, 2);
    }

    #[test]
    fn an_incomplete_registry_is_reported_with_its_reason_and_still_censused() {
        let fixture = Fixture::new("census-incomplete");
        let document = serde_json::json!({
            "version": "oxide.outputs.registry/2",
            "schema": 2,
            "revision": 8,
            "complete": false,
            "incomplete": {
                "reason": "the receipt seed was incomplete (unreadable receipt)",
                "since_ms": 1_792_525_019_847_u64,
                "transaction": "fixture-transaction",
            },
            "created_by": "fixture-transaction",
            "rows": [fixture.row_json()],
        });
        let path = fixture.dir.write(
            "pool/outputs/registry.json",
            &serde_json::to_vec(&document).unwrap(),
        );
        assert!(path.exists());
        let observation = observe_oxide_registry(
            &absent_pools(),
            Some(&[fixture.dir.join("pool")]),
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        let report = fixture.report(&observation);
        let facts = report.document.as_ref().expect("the document was read");
        assert!(!facts.complete);
        let incomplete = facts.incomplete.as_ref().expect("the reason");
        assert!(incomplete.reason.contains("receipt seed"), "{incomplete:?}");
        assert_eq!(incomplete.since_ms, 1_792_525_019_847);
        assert_eq!(incomplete.transaction, "fixture-transaction");
        assert_eq!(report.rows.count, 1, "the rows are still censused");
    }

    #[test]
    fn a_version_one_registry_is_read_and_its_rows_are_reported_honestly() {
        let fixture = Fixture::new("census-v1");
        let mut row = fixture.row_json();
        row["nonce"] = serde_json::json!(null);
        row["root_dev"] = serde_json::json!(null);
        row["root_ino"] = serde_json::json!(null);
        row["marker_digest"] = serde_json::json!(null);
        let document = serde_json::json!({
            "version": "oxide.outputs.registry/1",
            "schema": 1,
            "revision": 3,
            "complete": true,
            "created_by": "legacy-transaction",
            "rows": [row],
        });
        let path = fixture.dir.write(
            "pool/outputs/registry.json",
            &serde_json::to_vec(&document).unwrap(),
        );
        let observation = read_registry(&path, Deadline::after(std::time::Duration::from_secs(5)));
        let observed = observation.observed().expect("a reported v1 registry");
        assert_eq!(observed.version(), "oxide.outputs.registry/1");
        assert_eq!(observed.value().schema(), 1);
        assert_eq!(observed.value().rows()[0].root_ino(), None);
        let observation = observe_oxide_registry(
            &absent_pools(),
            Some(&[fixture.dir.join("pool")]),
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        let report = fixture.report(&observation);
        assert_eq!(
            fixture.row(&report).identity.reason,
            Some(IdentityReason::IdentityRecordMissing)
        );
    }

    #[test]
    fn an_unknown_version_a_divergent_document_and_an_oversized_one_are_unknown_state() {
        let fixture = TempDir::new("census-registry-divergent");
        let valid = serde_json::json!({
            "version": "oxide.outputs.registry/2",
            "schema": 2,
            "revision": 1,
            "complete": true,
            "created_by": "t",
            "rows": [],
        });
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                // Oxide always writes `rows`; a document without it is not an
                // empty registry, and Barge must not report one.
                "no-rows",
                serde_json::to_vec(&serde_json::json!({
                    "version": "oxide.outputs.registry/2",
                    "schema": 2,
                    "revision": 1,
                    "complete": true,
                    "created_by": "t"
                }))
                .unwrap(),
            ),
            (
                "version-9",
                serde_json::to_vec(&serde_json::json!({
                    "version": "oxide.outputs.registry/9",
                    "schema": 2,
                    "revision": 1,
                    "complete": true,
                    "created_by": "t",
                    "rows": []
                }))
                .unwrap(),
            ),
            (
                "schema-mismatch",
                serde_json::to_vec(&serde_json::json!({
                    "version": "oxide.outputs.registry/2",
                    "schema": 1,
                    "revision": 1,
                    "complete": true,
                    "created_by": "t",
                    "rows": []
                }))
                .unwrap(),
            ),
            (
                "unknown-field",
                serde_json::to_vec(&serde_json::json!({
                    "version": "oxide.outputs.registry/2",
                    "schema": 2,
                    "revision": 1,
                    "complete": true,
                    "created_by": "t",
                    "rows": [],
                    "nonce": "a field Barge does not know"
                }))
                .unwrap(),
            ),
            (
                "no-revision",
                serde_json::to_vec(&serde_json::json!({
                    "version": "oxide.outputs.registry/2",
                    "schema": 2,
                    "revision": 0,
                    "complete": true,
                    "created_by": "t",
                    "rows": []
                }))
                .unwrap(),
            ),
            (
                "incomplete-without-reason",
                serde_json::to_vec(&serde_json::json!({
                    "version": "oxide.outputs.registry/2",
                    "schema": 2,
                    "revision": 1,
                    "complete": false,
                    "created_by": "t",
                    "rows": []
                }))
                .unwrap(),
            ),
            ("not-json", b"{not json".to_vec()),
        ];
        for (name, bytes) in cases {
            let path = fixture.write(&format!("{name}.json"), &bytes);
            let observation =
                read_registry(&path, Deadline::after(std::time::Duration::from_secs(5)));
            assert_eq!(
                observation.state(),
                ObservationState::UnknownState,
                "{name} must be unknown state: {observation:?}"
            );
            let detail = observation
                .detail()
                .expect("an unknown observation says why");
            assert!(detail.contains(name), "{name}: {detail}");
        }
        let oversized = fixture.write(
            "oversized.json",
            &vec![b' '; OXIDE_REGISTRY_SIZE_CAP_BYTES + 1],
        );
        assert_eq!(
            read_registry(
                &oversized,
                Deadline::after(std::time::Duration::from_secs(5))
            )
            .state(),
            ObservationState::UnknownState
        );
        let link = fixture.join("link.json");
        let target = fixture.write("target.json", &serde_json::to_vec(&valid).unwrap());
        std::os::unix::fs::symlink(&target, &link).expect("the fixture link is made");
        assert_eq!(
            read_registry(&link, Deadline::after(std::time::Duration::from_secs(5))).state(),
            ObservationState::UnknownState
        );
        // A registry over Oxide's own row cap is not a bounded document.
        let rows: Vec<serde_json::Value> = (0..super::OXIDE_REGISTRY_ROWS + 1)
            .map(|_| {
                serde_json::json!({
                    "checkout_id": CHECKOUT_ID,
                    "checkout": CHECKOUT,
                    "receipt": RECEIPT,
                    "member": "target",
                    "root": "/Volumes/Builds/oxide-outputs/root",
                    "root_kind": "external",
                    "recipe": "target-debug",
                    "generation": 1,
                    "selected": true,
                    "pending": false,
                })
            })
            .collect();
        let over = fixture.write(
            "over-cap.json",
            &serde_json::to_vec(&serde_json::json!({
                "version": "oxide.outputs.registry/2",
                "schema": 2,
                "revision": 1,
                "complete": true,
                "created_by": "t",
                "rows": rows,
            }))
            .unwrap(),
        );
        assert_eq!(
            read_registry(&over, Deadline::after(std::time::Duration::from_secs(5))).state(),
            ObservationState::UnknownState
        );
    }

    #[test]
    fn an_absent_registry_is_absent_and_names_every_pool_it_looked_at() {
        let fixture = TempDir::new("census-absent");
        let pools = vec![fixture.join("first"), fixture.join("second")];
        let observation = observe_oxide_registry(
            &absent_pools(),
            Some(&pools),
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        assert_eq!(observation.observation.state(), ObservationState::Absent);
        assert_eq!(
            observation.searched,
            vec![
                pools[0].join("outputs/registry.json"),
                pools[1].join("outputs/registry.json")
            ]
        );
        let platform = crate::os::platform();
        let volume = FakeVolume {
            uuid: VolumeUuid::parse(UUID).unwrap(),
            mount: fixture.path().to_path_buf(),
            recorded_mount: fixture.path().to_path_buf(),
            device: DeviceId::new(1),
            present: Cell::new(true),
            duplicate: false,
        };
        let report = report(
            &observation,
            &platform,
            &volume,
            &volume,
            &volume,
            501,
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        assert_eq!(report.state, ObservationState::Absent);
        assert!(report.document.is_none());
        assert!(report.drift.is_none(), "an absent registry has no drift");
        assert_eq!(report.rows.count, 0);
        assert_eq!(report.rows.total, 0);
        assert!(report.detail.is_some(), "absence says where it looked");
        assert!(!fixture.join("first").exists(), "observing creates nothing");
    }

    #[test]
    fn an_unknown_pools_record_is_unknown_state_and_never_an_absent_census() {
        // The default pool holds a registry, and Oxide's pools record cannot be
        // read. The registry in the default pool may not be the one Oxide uses,
        // so the census reports its source as unknown: it neither pronounces the
        // registry absent nor reports the wrong one as the whole truth.
        let fixture = TempDir::new("census-pools-unknown");
        let cache = fixture.join("cache");
        let default_pool = cache.join("build-pool");
        std::fs::create_dir_all(default_pool.join("outputs")).expect("the default pool exists");
        std::fs::write(
            default_pool.join("outputs/registry.json"),
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"t","rows":[]}"#,
        )
        .expect("the default pool registry is written");
        let observation = observe_oxide_registry(
            &unknown_pools(),
            None,
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        assert_eq!(
            observation.observation.state(),
            ObservationState::UnknownState
        );
        assert_eq!(
            observation.observation.path(),
            Path::new("/fixture/state/pools.json"),
            "the report names the record that made the pool list unknown"
        );
        assert!(
            observation.searched.is_empty(),
            "no registry was looked for, so nothing is reported absent"
        );
        let detail = observation
            .observation
            .detail()
            .expect("an unknown observation says why");
        assert!(detail.contains("pools record"), "{detail}");
        assert!(detail.contains("nothing is reported absent"), "{detail}");

        let platform = crate::os::platform();
        let volume = FakeVolume {
            uuid: VolumeUuid::parse(UUID).unwrap(),
            mount: fixture.path().to_path_buf(),
            recorded_mount: fixture.path().to_path_buf(),
            device: DeviceId::new(1),
            present: Cell::new(true),
            duplicate: false,
        };
        let report = super::report(
            &observation,
            &platform,
            &volume,
            &volume,
            &volume,
            501,
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        assert_eq!(report.state, ObservationState::UnknownState);
        assert!(report.document.is_none());
        assert!(report.drift.is_none(), "an unknown source has no drift");
        assert_eq!(report.rows.count, 0);
        assert_eq!(report.rows.total, 0);
        assert!(
            !default_pool
                .join("outputs/registry.json")
                .metadata()
                .is_ok_and(|metadata| { metadata.len() == 0 }),
            "the default pool's registry was not touched"
        );
    }

    #[test]
    fn an_absent_pools_record_still_searches_the_default_pools() {
        // An absent record is a fact: Oxide recorded no pool, so only its
        // defaults can be in use and the list is complete. A registry there is
        // reported, and the report says which default pool it came from.
        let fixture = TempDir::new("census-pools-absent");
        let cache = fixture.join("cache");
        let default_pool = cache.join("build-pool");
        let path = fixture.write(
            "cache/build-pool/outputs/registry.json",
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":4,"complete":true,"created_by":"t","rows":[]}"#,
        );
        let known = crate::inspect::default_pools(&cache);
        assert_eq!(known.len(), 2);
        let observation = observe_oxide_registry(
            &absent_pools(),
            Some(&known),
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        assert_eq!(observation.observation.state(), ObservationState::Reported);
        assert_eq!(observation.observation.path(), path);
        assert_eq!(observation.searched, vec![path.clone()]);
        assert!(
            default_pool.join("outputs/registry.json").is_file(),
            "the registry is the one the fixture wrote"
        );
    }

    #[test]
    fn a_recorded_pools_list_is_searched_in_order_before_the_defaults() {
        let fixture = TempDir::new("census-pools-recorded");
        let cache = fixture.join("cache");
        let recorded = fixture.join("recorded-pool");
        let pools = fixture.write(
            "state/pools.json",
            &serde_json::to_vec(&vec![recorded.clone()]).expect("the record encodes"),
        );
        let pools = read_pools(&pools);
        let known = crate::inspect::pool_candidates(&pools, &cache).expect("a known list");
        assert_eq!(
            known,
            vec![recorded, cache.join("build-pool"), cache.join("pool")],
            "the recorded pools come first and the defaults follow once"
        );
    }

    #[test]
    fn a_present_but_unreadable_registry_is_not_looked_past() {
        let fixture = TempDir::new("census-present-unknown");
        let first = fixture.join("first");
        let second = fixture.join("second");
        std::fs::create_dir_all(first.join("outputs")).expect("the first pool exists");
        std::fs::create_dir_all(second.join("outputs")).expect("the second pool exists");
        std::fs::write(first.join("outputs/registry.json"), b"{broken")
            .expect("the broken registry is written");
        std::fs::write(
            second.join("outputs/registry.json"),
            br#"{"version":"oxide.outputs.registry/2","schema":2,"revision":1,"complete":true,"created_by":"t","rows":[]}"#,
        )
        .expect("a valid registry is written elsewhere");
        let observation = observe_oxide_registry(
            &absent_pools(),
            Some(&[first.clone(), second]),
            Deadline::after(std::time::Duration::from_secs(5)),
        );
        assert_eq!(
            observation.observation.state(),
            ObservationState::UnknownState
        );
        assert_eq!(
            observation.observation.path(),
            first.join("outputs/registry.json")
        );
    }

    #[test]
    fn the_census_reads_and_writes_nothing() {
        let fixture = Fixture::new("census-read-only");
        let observation = fixture.registry(vec![fixture.row_json()]);
        let before = snapshot(fixture.dir.path());
        let report = fixture.report(&observation);
        assert_eq!(report.rows.count, 1);
        assert_eq!(
            snapshot(fixture.dir.path()),
            before,
            "the census created, removed or changed nothing"
        );
    }

    /// Every path under one directory with its size and mode, so a test can
    /// prove a read-only pass changed nothing.
    fn snapshot(root: &Path) -> Vec<(PathBuf, u64, u32)> {
        let mut entries = Vec::new();
        fn walk(path: &Path, entries: &mut Vec<(PathBuf, u64, u32)>) {
            let Ok(listing) = std::fs::read_dir(path) else {
                return;
            };
            for entry in listing.flatten() {
                let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                use std::os::unix::fs::PermissionsExt;
                entries.push((entry.path(), metadata.len(), metadata.permissions().mode()));
                if metadata.is_dir() {
                    walk(&entry.path(), entries);
                }
            }
        }
        walk(root, &mut entries);
        entries.sort();
        entries
    }

    #[test]
    fn the_report_round_trips_and_refuses_counts_that_disagree() {
        let fixture = Fixture::new("census-round-trip");
        let report = fixture.census(vec![fixture.row_json()]);
        let encoded = serde_json::to_value(&report).expect("the report encodes");
        let decoded: OxideCensusReport =
            serde_json::from_value(encoded.clone()).expect("the report decodes");
        assert_eq!(decoded, report);
        decoded.validate().expect("the counts agree with the rows");

        let mut broken = encoded;
        broken["rows"]["count"] = serde_json::json!(2);
        let decoded: OxideCensusReport =
            serde_json::from_value(broken).expect("the document still decodes");
        assert!(decoded.validate().is_err());
    }

    #[test]
    fn the_default_report_claims_no_census() {
        let report = OxideCensusReport::default();
        assert_eq!(report.state, ObservationState::UnknownState);
        assert_eq!(report.rows.count, 0);
        assert!(report.drift.is_none());
        assert!(report.detail.is_some());
        assert_eq!(report.rows, RowSet::empty());
    }

    #[test]
    fn the_reason_count_order_is_stable() {
        let names: Vec<&str> = [
            IdentityReason::VolumeUnverified,
            IdentityReason::InodeChanged,
            IdentityReason::MarkerAbsent,
        ]
        .into_iter()
        .map(super::reason_name)
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec!["inode_changed", "marker_absent", "volume_unverified"]
        );
    }
}
